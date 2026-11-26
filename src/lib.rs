//! A deterministic, provider-independent game engine for AI Turn Tactics.

mod board;
mod protocol;
mod resolve;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

pub use board::{AttackerDisclosure, MatchConfig, PlayerConfig, SpawnMode};
pub use protocol::*;
use rand::{SeedableRng, rngs::StdRng};
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineError {
    InvalidConfig(&'static str),
    PlayerNotAwaited(PlayerId),
    AwaitingReplies,
    MatchFinished,
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::PlayerNotAwaited(player) => {
                write!(formatter, "player {player} has no pending call")
            }
            Self::AwaitingReplies => {
                formatter.write_str("the wake-up batch has outstanding replies")
            }
            Self::MatchFinished => formatter.write_str("the match has finished"),
        }
    }
}

impl std::error::Error for EngineError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawReason {
    AllRemainingTanksDestroyed,
    RoundLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MatchOutcome {
    Win {
        players: Vec<PlayerId>,
    },
    Draw {
        reason: DrawReason,
        players: Vec<PlayerId>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplyOutcome {
    Accepted,
    Retry { error: String },
    Passed { error: String },
}

/// Full state for a trusted local observer. Never send this to a player adapter.
#[derive(Serialize)]
pub struct ObserverSnapshot<'a> {
    pub config: &'a MatchConfig,
    pub tick: Tick,
    pub board_size: u32,
    pub outcome: Option<&'a MatchOutcome>,
    pub players: Vec<ObservedPlayer<'a>>,
    pub rooms: Vec<&'a Room>,
    pub messages: &'a [Message],
    pub public_events: &'a [PublicEvent],
    pub awaiting_replies: Vec<PlayerId>,
}

#[derive(Serialize)]
pub struct ObservedPlayer<'a> {
    pub id: PlayerId,
    pub position: Position,
    pub health: u64,
    pub action_points: u64,
    pub range: u64,
    pub memory: &'a str,
    pub vote: Option<PlayerId>,
    pub peace: Vec<PeaceDeclaration>,
    pub pending_orders: &'a [PendingOrder],
    pub order_results: &'a [OrderResult],
    pub damage: &'a [DamageReport],
    pub jury_bonus_ticks: &'a [Tick],
    pub wake_at: Option<Tick>,
    pub wake_events: &'a BTreeSet<WakeEvent>,
}

#[derive(Clone, Debug, Default)]
struct WakePlan {
    at: Option<Tick>,
    events: BTreeSet<WakeEvent>,
}

#[derive(Clone, Debug, Default)]
struct ObservationCursor {
    messages: usize,
    events: usize,
    results: usize,
    damage: usize,
    bonuses: usize,
}

struct Player {
    position: Position,
    health: u64,
    ap: u64,
    range: u64,
    memory: String,
    pending: Vec<PendingOrder>,
    next_order_id: OrderId,
    wake: WakePlan,
    cursor: ObservationCursor,
    order_results: Vec<OrderResult>,
    damage: Vec<DamageReport>,
    bonus_ticks: Vec<Tick>,
    vote: Option<PlayerId>,
    peace: BTreeMap<Vec<PlayerId>, bool>,
}

impl Player {
    fn new(position: Position) -> Self {
        Self {
            position,
            health: board::START_HEALTH,
            ap: 0,
            range: board::START_RANGE,
            memory: String::new(),
            pending: Vec::new(),
            next_order_id: 1,
            wake: WakePlan::default(),
            cursor: ObservationCursor::default(),
            order_results: Vec::new(),
            damage: Vec::new(),
            bonus_ticks: Vec::new(),
            vote: None,
            peace: BTreeMap::new(),
        }
    }

    fn alive(&self) -> bool {
        self.health > 0
    }

    fn agreements(&self) -> Vec<PeaceDeclaration> {
        self.peace
            .iter()
            .map(|(members, &public)| PeaceDeclaration {
                members: members.clone(),
                public,
            })
            .collect()
    }
}

struct ReplyUpdate {
    pending: Vec<PendingOrder>,
    next_order_id: OrderId,
    memory: Option<String>,
    wake: WakePlan,
}

enum Settlement {
    Accepted(ReplyUpdate),
    Pass,
}

struct CallSlot {
    call: PlayerCall,
    settlement: Option<Settlement>,
}

/// Authoritative match state. Pass only [`PlayerCall`] data to a player adapter.
///
/// ```
/// use ai_turn_tactics::{Engine, MatchConfig};
///
/// let mut game = Engine::new(MatchConfig::default())?;
/// let players: Vec<_> = game.pending_calls().map(|call| call.observation.player).collect();
/// let wait = r#"{"orders":[],"changes":[],"wake":{"events":["ap_grant"]}}"#;
/// for player in players {
///     game.submit_reply(player, wait)?;
/// }
/// game.advance_tick()?;
/// assert_eq!(game.tick(), 1);
/// # Ok::<(), ai_turn_tactics::EngineError>(())
/// ```
pub struct Engine {
    config: MatchConfig,
    rules: String,
    size: u32,
    tick: Tick,
    last_tick: Tick,
    players: BTreeMap<PlayerId, Player>,
    rooms: BTreeMap<RoomId, Room>,
    room_ids: BTreeMap<Vec<PlayerId>, RoomId>,
    room_rng: StdRng,
    messages: Vec<Message>,
    public_events: Vec<PublicEvent>,
    calls: BTreeMap<PlayerId, CallSlot>,
    /// Mandatory wake reasons from the last resolved tick, due on the next tick.
    next_tick_wakes: BTreeMap<PlayerId, BTreeSet<WakeReason>>,
    outcome: Option<MatchOutcome>,
}

impl Engine {
    /// Set up the board and open the initial wake-up batch at tick zero.
    /// Player IDs start at 1 and follow the order of the configured slots.
    pub fn new(config: MatchConfig) -> Result<Self, EngineError> {
        let last_tick = config.validate()?;
        let (size, starts) = board::make_board(&config);
        let players: BTreeMap<_, _> = (1..).zip(starts.into_iter().map(Player::new)).collect();
        let reasons = players
            .keys()
            .map(|&id| (id, vec![WakeReason::Start]))
            .collect();
        let rules = rules_text(&config, last_tick);
        let room_rng = StdRng::seed_from_u64(config.seed);
        let mut engine = Self {
            config,
            rules,
            size,
            tick: 0,
            last_tick,
            players,
            rooms: BTreeMap::new(),
            room_ids: BTreeMap::new(),
            room_rng,
            messages: Vec::new(),
            public_events: Vec::new(),
            calls: BTreeMap::new(),
            next_tick_wakes: BTreeMap::new(),
            outcome: None,
        };
        engine.open_calls(reasons);
        Ok(engine)
    }

    pub fn tick(&self) -> Tick {
        self.tick
    }

    pub fn outcome(&self) -> Option<&MatchOutcome> {
        self.outcome.as_ref()
    }

    /// Inspect committed state and history, including private player information.
    pub fn observer_snapshot(&self) -> ObserverSnapshot<'_> {
        ObserverSnapshot {
            config: &self.config,
            tick: self.tick,
            board_size: self.size,
            outcome: self.outcome.as_ref(),
            players: self
                .players
                .iter()
                .map(|(&id, player)| ObservedPlayer {
                    id,
                    position: player.position,
                    health: player.health,
                    action_points: player.ap,
                    range: player.range,
                    memory: &player.memory,
                    vote: player.vote,
                    peace: player.agreements(),
                    pending_orders: &player.pending,
                    order_results: &player.order_results,
                    damage: &player.damage,
                    jury_bonus_ticks: &player.bonus_ticks,
                    wake_at: player.wake.at,
                    wake_events: &player.wake.events,
                })
                .collect(),
            rooms: self.rooms.values().collect(),
            messages: &self.messages,
            public_events: &self.public_events,
            awaiting_replies: self
                .pending_calls()
                .map(|call| call.observation.player)
                .collect(),
        }
    }

    /// Returns all calls still awaiting a reply. A retry reuses the original observation.
    pub fn pending_calls(&self) -> impl Iterator<Item = &PlayerCall> {
        self.calls
            .values()
            .filter(|slot| slot.settlement.is_none())
            .map(|slot| &slot.call)
    }

    /// Apply one tick. Advancing while any replies remain pending is an error.
    pub fn advance_tick(&mut self) -> Result<(), EngineError> {
        self.require_ready()?;
        self.tick += 1;
        self.resolve_tick();
        Ok(())
    }

    /// Skip idle ticks and resolve the next grant, due order, wake-up, or match limit.
    /// The skipped ticks cannot alter the match or produce observations.
    pub fn advance_to_next_event(&mut self) -> Result<(), EngineError> {
        self.require_ready()?;
        let mut next = if self.next_tick_wakes.is_empty() {
            self.next_grant_tick()
        } else {
            self.tick + 1
        };
        for player in self.players.values() {
            for order in &player.pending {
                next = next.min(order.tick);
            }
            if let Some(wake_at) = player.wake.at {
                next = next.min(wake_at);
            }
        }
        self.tick = next;
        self.resolve_tick();
        Ok(())
    }

    fn require_ready(&self) -> Result<(), EngineError> {
        if self.outcome.is_some() {
            return Err(EngineError::MatchFinished);
        }
        if !self.calls.is_empty() {
            return Err(EngineError::AwaitingReplies);
        }
        Ok(())
    }

    fn next_grant_tick(&self) -> Tick {
        // An active match is always earlier than its final, validated grant tick.
        (self.tick / self.config.ap_interval + 1) * self.config.ap_interval
    }

    /// Validate a raw JSON reply and stage its changes until the whole batch settles.
    pub fn submit_reply(
        &mut self,
        player: PlayerId,
        reply: &str,
    ) -> Result<ReplyOutcome, EngineError> {
        let slot = self
            .calls
            .get(&player)
            .filter(|slot| slot.settlement.is_none())
            .ok_or(EngineError::PlayerNotAwaited(player))?;
        let retry = slot.call.validation_error.is_some();
        let result = serde_json::from_str(reply)
            .map_err(|error| error.to_string())
            .and_then(|reply| self.validate_reply(player, reply));
        let slot = self.calls.get_mut(&player).expect("pending call");
        let outcome = match result {
            Ok(update) => {
                slot.settlement = Some(Settlement::Accepted(update));
                ReplyOutcome::Accepted
            }
            Err(error) if retry => {
                slot.settlement = Some(Settlement::Pass);
                ReplyOutcome::Passed { error }
            }
            Err(error) => {
                slot.call.validation_error = Some(error.clone());
                ReplyOutcome::Retry { error }
            }
        };
        if self.calls.values().all(|slot| slot.settlement.is_some()) {
            self.commit_calls();
        }
        Ok(outcome)
    }

    fn validate_reply(&self, id: PlayerId, reply: PlayerReply) -> Result<ReplyUpdate, String> {
        let player = &self.players[&id];
        if let Some(memory) = &reply.memory {
            if memory.len() > self.config.memory_limit_bytes {
                return Err(format!(
                    "memory uses {} bytes, exceeding the {} byte limit",
                    memory.len(),
                    self.config.memory_limit_bytes
                ));
            }
        }
        let wake = self.validate_wake(reply.wake)?;
        let mut pending = player.pending.clone();
        let mut changed = BTreeSet::new();
        for change in reply.changes {
            let order_id = match change {
                OrderChange::Cancel { order_id } | OrderChange::Reschedule { order_id, .. } => {
                    order_id
                }
            };
            if !changed.insert(order_id) {
                return Err(format!("order {order_id} has more than one change"));
            }
            let index = pending
                .iter()
                .position(|order| order.id == order_id)
                .ok_or_else(|| format!("unknown pending order {order_id}"))?;
            match change {
                OrderChange::Cancel { .. } => {
                    pending.remove(index);
                }
                OrderChange::Reschedule { tick, .. } => {
                    self.validate_future_tick(tick)?;
                    pending[index].tick = tick;
                }
            }
        }
        let mut next_order_id = player.next_order_id;
        for order in reply.orders {
            self.validate_future_tick(order.tick)?;
            self.validate_action(id, &order.action)?;
            pending.push(PendingOrder {
                id: next_order_id,
                tick: order.tick,
                action: order.action,
            });
            next_order_id = next_order_id
                .checked_add(1)
                .ok_or("order ID limit exceeded")?;
        }
        let mut move_ticks = BTreeSet::new();
        for order in &pending {
            if matches!(order.action, Action::Move { .. }) && !move_ticks.insert(order.tick) {
                return Err(format!(
                    "only one move can be scheduled for tick {}",
                    order.tick
                ));
            }
        }
        Ok(ReplyUpdate {
            pending,
            next_order_id,
            memory: reply.memory,
            wake,
        })
    }

    fn validate_future_tick(&self, tick: Tick) -> Result<(), String> {
        if tick <= self.tick {
            return Err(format!(
                "tick {tick} must be later than the current tick {}",
                self.tick
            ));
        }
        Ok(())
    }

    fn validate_wake(&self, wake: WakeRequest) -> Result<WakePlan, String> {
        let at = match wake.after_ticks {
            Some(0) => return Err("wake.after_ticks must be positive".into()),
            Some(delay) => Some(
                self.tick
                    .checked_add(delay)
                    .ok_or("wake tick exceeds tick capacity")?,
            ),
            None => None,
        };
        if at.is_none() && wake.events.is_empty() {
            return Err("choose at least one wake event or a future wake tick".into());
        }
        Ok(WakePlan {
            at,
            events: wake.events.into_iter().collect(),
        })
    }

    fn validate_action(&self, id: PlayerId, action: &Action) -> Result<(), String> {
        match action {
            Action::GiftHealth { target, amount } | Action::GiftAp { target, amount } => {
                self.validate_player(*target)?;
                if *amount == 0 {
                    return Err("gift amount must be positive".into());
                }
            }
            Action::Attack { target } | Action::Vote { target } => self.validate_player(*target)?,
            Action::Message { address, .. } => {
                match address {
                    MessageAddress::Public {} => {}
                    MessageAddress::Direct { recipient } => self.validate_player(*recipient)?,
                    MessageAddress::Group { recipients } => {
                        self.validate_members(recipients)?;
                        if recipients.contains(&id) || recipients.len() < 2 {
                            return Err("a group needs at least two distinct recipients other than the sender".into());
                        }
                    }
                    MessageAddress::Room { room_id } => {
                        if !self
                            .rooms
                            .get(room_id)
                            .is_some_and(|room| room.members.contains(&id))
                        {
                            return Err(format!("unknown room {room_id}"));
                        }
                    }
                }
            }
            Action::DeclarePeace { members, .. } | Action::RevokePeace { members } => {
                self.validate_members(members)?;
                if members.len() < 2 || !members.contains(&id) {
                    return Err(
                        "a peace agreement must include its author and at least one other player"
                            .into(),
                    );
                }
            }
            Action::Move { .. } | Action::UpgradeRange {} | Action::Repair {} => {}
        }
        Ok(())
    }

    fn validate_player(&self, player: PlayerId) -> Result<(), String> {
        self.players
            .contains_key(&player)
            .then_some(())
            .ok_or_else(|| format!("unknown player {player}"))
    }

    fn validate_members(&self, members: &[PlayerId]) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for &member in members {
            self.validate_player(member)?;
            if !seen.insert(member) {
                return Err(format!("player {member} appears more than once"));
            }
        }
        Ok(())
    }

    fn commit_calls(&mut self) {
        let next_ap_tick = self.next_grant_tick();
        for (id, slot) in std::mem::take(&mut self.calls) {
            let player = self.players.get_mut(&id).expect("known player");
            match slot.settlement.expect("whole batch resolved") {
                Settlement::Accepted(update) => {
                    player.pending = update.pending;
                    player.next_order_id = update.next_order_id;
                    if let Some(memory) = update.memory {
                        player.memory = memory;
                    }
                    player.wake = update.wake;
                }
                Settlement::Pass => {
                    player.wake = WakePlan {
                        at: Some(next_ap_tick),
                        events: BTreeSet::new(),
                    }
                }
            }
        }
    }

    fn open_calls(&mut self, reasons: BTreeMap<PlayerId, Vec<WakeReason>>) {
        for (id, wake_reasons) in reasons {
            let observation = self.observe(id, wake_reasons);
            let player = self.players.get_mut(&id).expect("known player");
            // The snapshot owns the current delivery window. Retries reuse this snapshot.
            player.cursor = ObservationCursor {
                messages: self.messages.len(),
                events: self.public_events.len(),
                results: player.order_results.len(),
                damage: player.damage.len(),
                bonuses: player.bonus_ticks.len(),
            };
            if player.wake.at == Some(self.tick) {
                player.wake.at = None;
            }
            self.calls.insert(
                id,
                CallSlot {
                    call: PlayerCall {
                        rules: self.rules.clone(),
                        starting_prompt: self.config.players[(id - 1) as usize]
                            .starting_prompt
                            .clone(),
                        observation,
                        validation_error: None,
                    },
                    settlement: None,
                },
            );
        }
    }

    fn observe(&self, id: PlayerId, wake_reasons: Vec<WakeReason>) -> Observation {
        let player = &self.players[&id];
        let mut pending_orders = player.pending.clone();
        pending_orders.sort_by_key(|order| (order.tick, order.action.phase()));
        Observation {
            player: id,
            tick: self.tick,
            role: if player.alive() {
                Role::Tank
            } else {
                Role::Juror
            },
            wake_reasons,
            board_size: self.size,
            board: self
                .players
                .iter()
                .map(|(&player, state)| PublicTank {
                    player,
                    position: state.position,
                    health: state.health,
                })
                .collect(),
            action_points: player.ap,
            range: player.range,
            pending_orders,
            order_results: player.order_results[player.cursor.results..].to_vec(),
            damage: self.visible_damage(&player.damage[player.cursor.damage..]),
            jury_bonus_ticks: player.bonus_ticks[player.cursor.bonuses..].to_vec(),
            own_vote: player.vote,
            own_peace: player.agreements(),
            public_events: self.public_events[player.cursor.events..].to_vec(),
            messages: self.messages[player.cursor.messages..]
                .iter()
                .filter(|message| message.recipients.contains(&id))
                .cloned()
                .collect(),
            rooms: self
                .rooms
                .values()
                .filter(|room| room.members.contains(&id))
                .cloned()
                .collect(),
            memory: player.memory.clone(),
            memory_used_bytes: player.memory.len(),
            memory_limit_bytes: self.config.memory_limit_bytes,
        }
    }

    fn visible_damage(&self, reports: &[DamageReport]) -> Vec<DamageReport> {
        reports
            .iter()
            .cloned()
            .map(|mut report| {
                report.attackers = match self.config.attacker_disclosure {
                    AttackerDisclosure::Full => report.attackers,
                    AttackerDisclosure::LethalOnly if report.lethal => {
                        report.attackers.map(|mut attackers| {
                            attackers.sort_unstable();
                            attackers.dedup();
                            attackers
                        })
                    }
                    _ => None,
                };
                report
            })
            .collect()
    }
}

fn rules_text(config: &MatchConfig, last_tick: Tick) -> String {
    let disclosure = match config.attacker_disclosure {
        AttackerDisclosure::Full => "Damage reports name each shooter for every hit.",
        AttackerDisclosure::LethalOnly => {
            "Only lethal damage reports name the contributing shooters."
        }
        AttackerDisclosure::Hidden => "Damage reports omit shooter identities.",
    };
    format!(
        "Remain alive as the sole survivor, or agree to a shared win with all remaining tanks. \
        The match ends in a draw at tick {last_tick} if there is no winner. Coordinates start at zero. \
        Tanks start with 3 health, range 2, and 0 AP. Every {} ticks, living tanks gain 1 AP and \
        each tank with at least three jury votes gains 1 more. Jurors may vote once per round and \
        change their vote before the next grant. AP and range are private. \
        Schedule orders and changes for future ticks. AP is checked at execution. \
        Grants resolve first, then moves, then upgrades and repair, then health gifts, then combat, \
        then AP gifts, messages, votes, and peace orders. Multiple eligible actions use your chosen \
        order within their phase. Move one orthogonal square per tick for 1 AP. A destination must \
        start empty. Contested moves fail. Wrecks block movement. Range is Manhattan distance after \
        movement. Each valid shot costs 1 AP and deals 1 damage. Damage lands simultaneously. \
        \n\nRepair (repair) costs 3 AP. Your tank gains 1 health point.\n\
        Range upgrade (upgrade_range) costs 3 AP. Your tank's range increases by 1.\n\
        Health and range have no maximum.\n\n\
        Gifts use your current range and can transfer any affordable amount. Keep at least 1 health when giving \
        health. Gifted health can revive a wreck with the gifted health, 0 AP and retained range. \
        Received gifts can be passed on next tick. AP gifts require both tanks to be alive and \
        become spendable next tick. Elimination cancels pending orders. Attackers split the \
        eliminated tank's remaining AP equally, discarding the remainder and any eliminated \
        attacker's share. Jurors can still send messages and cast ballots. {disclosure} \
        Messages are free and can be public, direct, or sent to a fixed group. You may declare or \
        revoke named peace agreements with multiple groups. A shared win requires matching active \
        declarations from the entire surviving group. Wake conditions are selected in your reply. \
        Every wake-up occurs after the tick resolves. While the match continues, receiving AP from a grant, gift, \
        or loot automatically wakes you on the next tick. Taking damage or being eliminated also wakes you on \
        the next tick. These wake-ups apply regardless of your wake preferences. \
        Multiple reasons on the same tick produce one call. Revival wakes you on the tick it happens. \
        The optional ap_grant and damage events can also wake you on the tick of the event. \
        Update your private memory when you want to retain new information.",
        config.ap_interval
    )
}

#[cfg(test)]
mod tests;
