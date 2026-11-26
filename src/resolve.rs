//! Tick resolution. Every phase sees the state committed by the phases before it.

use super::*;
use rand::Rng;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Phase {
    Movement,
    Upgrades,
    HealthGifts,
    Combat,
    ApGifts,
    Social,
}

impl Action {
    pub(super) fn phase(&self) -> Phase {
        match self {
            Self::Move { .. } => Phase::Movement,
            Self::UpgradeRange {} | Self::Repair {} => Phase::Upgrades,
            Self::GiftHealth { .. } => Phase::HealthGifts,
            Self::Attack { .. } => Phase::Combat,
            Self::GiftAp { .. } => Phase::ApGifts,
            Self::Message { .. }
            | Self::Vote { .. }
            | Self::DeclarePeace { .. }
            | Self::RevokePeace { .. } => Phase::Social,
        }
    }
}

type Orders = Vec<(PlayerId, PendingOrder)>;

#[derive(Default)]
struct TickResolution {
    reasons: BTreeMap<PlayerId, BTreeSet<WakeReason>>,
    next_tick_wakes: BTreeMap<PlayerId, BTreeSet<WakeReason>>,
    eliminated: BTreeSet<PlayerId>,
}

impl TickResolution {
    fn signal(&mut self, player: PlayerId, reason: WakeReason) {
        self.reasons.entry(player).or_default().insert(reason);
    }

    fn event(&mut self, player: PlayerId, event: WakeEvent) {
        self.signal(player, WakeReason::Event(event));
    }

    fn wake_next_tick(&mut self, player: PlayerId, reason: WakeReason) {
        self.next_tick_wakes
            .entry(player)
            .or_default()
            .insert(reason);
    }
}

#[derive(Clone, Copy)]
enum Resource {
    Health,
    Ap,
}

impl Engine {
    pub(super) fn resolve_tick(&mut self) {
        let mut resolution = TickResolution {
            reasons: std::mem::take(&mut self.next_tick_wakes),
            ..TickResolution::default()
        };
        if self.tick % self.config.ap_interval == 0 {
            self.grant_ap(&mut resolution);
        }
        let mut orders = self.take_due_orders();
        self.move_tanks(
            orders.remove(&Phase::Movement).unwrap_or_default(),
            &mut resolution,
        );
        self.upgrade_tanks(
            orders.remove(&Phase::Upgrades).unwrap_or_default(),
            &mut resolution,
        );
        self.transfer(
            orders.remove(&Phase::HealthGifts).unwrap_or_default(),
            Resource::Health,
            &mut resolution,
        );
        self.attack(
            orders.remove(&Phase::Combat).unwrap_or_default(),
            &mut resolution,
        );
        self.transfer(
            orders.remove(&Phase::ApGifts).unwrap_or_default(),
            Resource::Ap,
            &mut resolution,
        );
        self.social_orders(
            orders.remove(&Phase::Social).unwrap_or_default(),
            &mut resolution,
        );
        self.check_outcome(&resolution);
        if self.outcome.is_none() {
            self.next_tick_wakes = std::mem::take(&mut resolution.next_tick_wakes);
            let reasons = self.select_wakeups(resolution);
            self.open_calls(reasons);
        }
    }

    fn grant_ap(&mut self, resolution: &mut TickResolution) {
        let mut totals = BTreeMap::<PlayerId, usize>::new();
        for player in self.players.values_mut() {
            if let Some(candidate) = player.vote.take() {
                if !player.alive() {
                    *totals.entry(candidate).or_default() += 1;
                }
            }
        }
        for (&id, player) in &mut self.players {
            if player.alive() {
                player.ap += 1;
                resolution.wake_next_tick(id, WakeReason::ApGained);
                if totals.get(&id).copied().unwrap_or(0) >= 3 {
                    player.ap += 1;
                    player.bonus_ticks.push(self.tick);
                }
            }
            resolution.event(id, WakeEvent::ApGrant);
        }
        self.public_events
            .push(PublicEvent::ApRound { tick: self.tick });
    }

    fn take_due_orders(&mut self) -> BTreeMap<Phase, Orders> {
        let mut due = BTreeMap::<Phase, Orders>::new();
        for (&id, player) in &mut self.players {
            let mut pending = Vec::new();
            for order in std::mem::take(&mut player.pending) {
                if order.tick == self.tick {
                    due.entry(order.action.phase())
                        .or_default()
                        .push((id, order));
                } else {
                    pending.push(order);
                }
            }
            player.pending = pending;
        }
        due
    }

    fn move_tanks(&mut self, orders: Orders, resolution: &mut TickResolution) {
        let occupied: BTreeSet<_> = self
            .players
            .values()
            .map(|player| player.position)
            .collect();
        let mut destinations = BTreeMap::<Position, usize>::new();
        let mut valid = Vec::new();
        for (id, order) in orders {
            let Action::Move { destination } = order.action else {
                unreachable!()
            };
            let player = &self.players[&id];
            let result = if !player.alive() {
                Err(Failure::Eliminated)
            } else if destination.x >= self.size
                || destination.y >= self.size
                || destination.distance(player.position) != 1
            {
                Err(Failure::InvalidMove)
            } else if occupied.contains(&destination) {
                Err(Failure::Occupied)
            } else if player.ap == 0 {
                Err(Failure::InsufficientAp)
            } else {
                Ok(())
            };
            match result {
                Ok(()) => {
                    *destinations.entry(destination).or_default() += 1;
                    valid.push((id, order, destination));
                }
                Err(reason) => self.record(id, order, OrderOutcome::Failed { reason }, resolution),
            }
        }
        for (id, order, destination) in valid {
            let outcome = if destinations[&destination] > 1 {
                OrderOutcome::Failed {
                    reason: Failure::Contested,
                }
            } else {
                let player = self.players.get_mut(&id).expect("known player");
                player.position = destination;
                player.ap -= 1;
                OrderOutcome::Applied
            };
            self.record(id, order, outcome, resolution);
        }
    }

    fn upgrade_tanks(&mut self, orders: Orders, resolution: &mut TickResolution) {
        for (id, order) in orders {
            let player = self.players.get_mut(&id).expect("known player");
            let outcome = if !player.alive() {
                OrderOutcome::Failed {
                    reason: Failure::Eliminated,
                }
            } else if player.ap < 3 {
                OrderOutcome::Failed {
                    reason: Failure::InsufficientAp,
                }
            } else {
                player.ap -= 3;
                match order.action {
                    Action::Repair {} => player.health += 1,
                    Action::UpgradeRange {} => player.range += 1,
                    _ => unreachable!(),
                }
                OrderOutcome::Applied
            };
            self.record(id, order, outcome, resolution);
        }
    }

    fn transfer(&mut self, orders: Orders, resource: Resource, resolution: &mut TickResolution) {
        // Deduct every outgoing amount before applying credits, which prevents gift chains.
        let mut credits = BTreeMap::<PlayerId, u64>::new();
        for (id, order) in orders {
            let (target, requested) = match order.action {
                Action::GiftHealth { target, amount } | Action::GiftAp { target, amount } => {
                    (target, amount)
                }
                _ => unreachable!(),
            };
            let outcome = if resolution.eliminated.contains(&id) {
                OrderOutcome::CancelledByElimination
            } else {
                let allow_wreck = matches!(resource, Resource::Health);
                match self.reachable_target(id, target, allow_wreck) {
                    Err(reason) => OrderOutcome::Failed { reason },
                    Ok(()) => {
                        let player = self.players.get_mut(&id).expect("known donor");
                        let (balance, minimum, insufficient) = match resource {
                            Resource::Health => {
                                (&mut player.health, 1, Failure::InsufficientHealth)
                            }
                            Resource::Ap => (&mut player.ap, 0, Failure::InsufficientAp),
                        };
                        let transferred = requested.min(*balance - minimum);
                        if transferred == 0 {
                            OrderOutcome::Failed {
                                reason: insufficient,
                            }
                        } else {
                            *balance -= transferred;
                            *credits.entry(target).or_default() += transferred;
                            OrderOutcome::Transferred {
                                requested,
                                transferred,
                            }
                        }
                    }
                }
            };
            self.record(id, order, outcome, resolution);
        }
        for (target, amount) in credits {
            let player = self.players.get_mut(&target).expect("known recipient");
            match resource {
                Resource::Health => {
                    if !player.alive() {
                        player.vote = None;
                        resolution.signal(target, WakeReason::Revived);
                    }
                    player.health += amount;
                }
                Resource::Ap => {
                    player.ap += amount;
                    resolution.wake_next_tick(target, WakeReason::ApGained);
                }
            }
        }
    }

    fn reachable_target(
        &self,
        id: PlayerId,
        target: PlayerId,
        allow_wreck: bool,
    ) -> Result<(), Failure> {
        let player = &self.players[&id];
        let other = &self.players[&target];
        if !player.alive() {
            Err(Failure::Eliminated)
        } else if target == id {
            Err(Failure::InvalidTarget)
        } else if !allow_wreck && !other.alive() {
            Err(Failure::TargetEliminated)
        } else if player.position.distance(other.position) > player.range {
            Err(Failure::OutOfRange)
        } else {
            Ok(())
        }
    }

    fn attack(&mut self, orders: Orders, resolution: &mut TickResolution) {
        let mut hits = BTreeMap::<PlayerId, Vec<PlayerId>>::new();
        for (id, order) in orders {
            let Action::Attack { target } = order.action else {
                unreachable!()
            };
            let outcome = match self.reachable_target(id, target, false) {
                Err(reason) => OrderOutcome::Failed { reason },
                Ok(()) => {
                    let player = self.players.get_mut(&id).expect("known attacker");
                    if player.ap == 0 {
                        OrderOutcome::Failed {
                            reason: Failure::InsufficientAp,
                        }
                    } else {
                        player.ap -= 1;
                        hits.entry(target).or_default().push(id);
                        OrderOutcome::Applied
                    }
                }
            };
            self.record(id, order, outcome, resolution);
        }
        let mut spoils = Vec::new();
        for (target, shots) in hits {
            let player = self.players.get_mut(&target).expect("known target");
            let damage = shots.len() as u64;
            player.health = player.health.saturating_sub(damage);
            let lethal = !player.alive();
            let shooters: BTreeSet<_> = shots.iter().copied().collect();
            player.damage.push(DamageReport {
                tick: self.tick,
                damage,
                lethal,
                attackers: Some(shots),
            });
            resolution.event(target, WakeEvent::Damage);
            resolution.wake_next_tick(target, WakeReason::DamageTaken);
            if lethal {
                let share = std::mem::take(&mut player.ap) / shooters.len() as u64;
                spoils.push((shooters, share));
                resolution.eliminated.insert(target);
                resolution.wake_next_tick(target, WakeReason::Eliminated);
                for order in std::mem::take(&mut player.pending) {
                    self.record(
                        target,
                        order,
                        OrderOutcome::CancelledByElimination,
                        resolution,
                    );
                }
            }
        }
        // All damage and eliminations are known before crediting the surviving attackers.
        for (shooters, share) in spoils {
            for shooter in shooters {
                let player = self.players.get_mut(&shooter).expect("known attacker");
                if player.alive() && share > 0 {
                    player.ap += share;
                    resolution.wake_next_tick(shooter, WakeReason::ApGained);
                }
            }
        }
    }

    fn social_orders(&mut self, orders: Orders, resolution: &mut TickResolution) {
        for (id, order) in orders {
            let outcome = if resolution.eliminated.contains(&id) {
                OrderOutcome::CancelledByElimination
            } else {
                match &order.action {
                    Action::Message { address, text } => {
                        self.deliver_message(id, address, text, resolution);
                        OrderOutcome::Applied
                    }
                    Action::Vote { target } => self.vote(id, *target),
                    Action::DeclarePeace { members, public } => {
                        self.change_peace(id, members, Some(*public))
                    }
                    Action::RevokePeace { members } => self.change_peace(id, members, None),
                    _ => unreachable!(),
                }
            };
            self.record(id, order, outcome, resolution);
        }
    }

    fn deliver_message(
        &mut self,
        id: PlayerId,
        address: &MessageAddress,
        text: &str,
        resolution: &mut TickResolution,
    ) {
        let (recipients, channel, room, wake) = match address {
            MessageAddress::Public {} => (
                self.players.keys().copied().collect(),
                MessageChannel::Public,
                None,
                WakeEvent::PublicMessage,
            ),
            MessageAddress::Direct { recipient } => (
                vec![*recipient],
                MessageChannel::Direct,
                None,
                WakeEvent::DirectMessage,
            ),
            MessageAddress::Group { recipients } => {
                let mut members = recipients.clone();
                members.push(id);
                members.sort_unstable();
                let room_id = self.room_for(&members);
                (
                    members,
                    MessageChannel::Group,
                    Some(room_id),
                    WakeEvent::GroupMessage,
                )
            }
            MessageAddress::Room { room_id } => (
                self.rooms[room_id].members.clone(),
                MessageChannel::Group,
                Some(*room_id),
                WakeEvent::GroupMessage,
            ),
        };
        for &recipient in &recipients {
            if recipient != id {
                resolution.event(recipient, wake);
            }
        }
        self.messages.push(Message {
            sender: id,
            recipients,
            tick: self.tick,
            channel,
            room,
            text: text.to_owned(),
        });
    }

    fn room_for(&mut self, members: &[PlayerId]) -> RoomId {
        if let Some(&room) = self.room_ids.get(members) {
            return room;
        }
        // Opaque IDs keep room counts and creation order private to other groups.
        let mut id = self.room_rng.random();
        while id == 0 || self.rooms.contains_key(&id) {
            id = self.room_rng.random();
        }
        self.rooms.insert(
            id,
            Room {
                id,
                members: members.to_vec(),
            },
        );
        self.room_ids.insert(members.to_vec(), id);
        id
    }

    fn vote(&mut self, id: PlayerId, target: PlayerId) -> OrderOutcome {
        if self.players[&id].alive() {
            OrderOutcome::Failed {
                reason: Failure::JuryOnly,
            }
        } else if !self.players[&target].alive() {
            OrderOutcome::Failed {
                reason: Failure::TargetEliminated,
            }
        } else {
            self.players.get_mut(&id).expect("known juror").vote = Some(target);
            OrderOutcome::Applied
        }
    }

    fn change_peace(
        &mut self,
        id: PlayerId,
        members: &[PlayerId],
        visibility: Option<bool>,
    ) -> OrderOutcome {
        let player = self.players.get_mut(&id).expect("known player");
        let mut members = members.to_vec();
        members.sort_unstable();
        let previous = match visibility {
            Some(public) => player.peace.insert(members.clone(), public),
            None => player.peace.remove(&members),
        };
        if visibility.is_none() && previous.is_none() {
            return OrderOutcome::Failed {
                reason: Failure::NoAgreement,
            };
        }
        let active = visibility == Some(true);
        if active != (previous == Some(true)) {
            self.public_events.push(PublicEvent::Peace {
                tick: self.tick,
                player: id,
                members,
                active,
            });
        }
        OrderOutcome::Applied
    }

    fn check_outcome(&mut self, resolution: &TickResolution) {
        let alive: Vec<_> = self
            .players
            .iter()
            .filter_map(|(&id, player)| player.alive().then_some(id))
            .collect();
        self.outcome = if alive.is_empty() {
            Some(MatchOutcome::Draw {
                reason: DrawReason::AllRemainingTanksDestroyed,
                players: resolution.eliminated.iter().copied().collect(),
            })
        } else if alive.len() == 1
            || alive
                .iter()
                .all(|id| self.players[id].peace.contains_key(&alive))
        {
            Some(MatchOutcome::Win { players: alive })
        } else if self.tick == self.last_tick {
            Some(MatchOutcome::Draw {
                reason: DrawReason::RoundLimit,
                players: alive,
            })
        } else {
            None
        };
    }

    fn select_wakeups(
        &self,
        mut resolution: TickResolution,
    ) -> BTreeMap<PlayerId, Vec<WakeReason>> {
        for (&id, player) in &self.players {
            if player.wake.at == Some(self.tick) {
                resolution.signal(id, WakeReason::Scheduled);
            }
        }
        resolution
            .reasons
            .into_iter()
            .filter_map(|(id, reasons)| {
                let subscribed = &self.players[&id].wake.events;
                let matching: Vec<_> = reasons
                    .into_iter()
                    .filter(|reason| match reason {
                        WakeReason::Event(event) => subscribed.contains(event),
                        _ => true,
                    })
                    .collect();
                (!matching.is_empty()).then_some((id, matching))
            })
            .collect()
    }

    fn record(
        &mut self,
        id: PlayerId,
        order: PendingOrder,
        outcome: OrderOutcome,
        resolution: &mut TickResolution,
    ) {
        if matches!(outcome, OrderOutcome::Failed { .. }) {
            resolution.event(id, WakeEvent::OrderFailure);
        }
        self.players
            .get_mut(&id)
            .expect("known player")
            .order_results
            .push(OrderResult {
                id: order.id,
                tick: self.tick,
                action: order.action,
                outcome,
            });
    }
}
