//! A deterministic stand-in that uses the same observations and reply schema as a model.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ai_turn_tactics::{
    Action, MessageAddress, NewOrder, Observation, PlayerReply, Position, PublicTank, Role,
    WakeEvent, WakeReason, WakeRequest,
};

pub fn reply(view: &Observation) -> PlayerReply {
    let tick = view.tick + 1;
    let own = view
        .board
        .iter()
        .find(|tank| tank.player == view.player)
        .unwrap();
    let mut enemies: Vec<_> = view
        .board
        .iter()
        .filter(|tank| tank.player != view.player && tank.health != 0)
        .collect();
    enemies.sort_by_key(|tank| {
        (
            own.position.distance(tank.position),
            tank.health,
            tank.player,
        )
    });

    let mut actions = Vec::new();
    if view.wake_reasons.contains(&WakeReason::Start) && view.player <= 2 {
        actions.push(Action::Message {
            address: MessageAddress::Public {},
            text: format!("Tank {} is in position.", view.player),
        });
    }
    if let Some(target) = enemies.first() {
        if view.role == Role::Juror {
            actions.push(Action::Vote {
                target: target.player,
            });
        } else if view.action_points > 0 {
            if own.position.distance(target.position) <= view.range {
                for _ in 0..view.action_points.min(target.health) {
                    actions.push(Action::Attack {
                        target: target.player,
                    });
                }
            } else if let Some(destination) = approach(view, own, target) {
                actions.push(Action::Move { destination });
            }
        }
    }

    PlayerReply {
        orders: actions
            .into_iter()
            .map(|action| NewOrder { tick, action })
            .collect(),
        changes: Vec::new(),
        memory: None,
        wake: WakeRequest {
            after_ticks: None,
            events: vec![WakeEvent::ApGrant],
        },
    }
}

/// Find the first step on a shortest route into range, treating wrecks as occupied.
fn approach(view: &Observation, own: &PublicTank, target: &PublicTank) -> Option<Position> {
    let occupied: BTreeSet<_> = view.board.iter().map(|tank| tank.position).collect();
    let mut first_step = BTreeMap::from([(own.position, own.position)]);
    let mut frontier = VecDeque::from([own.position]);
    let mut directions = [(1, 0), (0, 1), (-1, 0), (0, -1)];
    directions.rotate_left(((view.tick / 2 + u64::from(own.player)) % 4) as usize);
    while let Some(position) = frontier.pop_front() {
        if position.distance(target.position) <= view.range {
            return Some(first_step[&position]);
        }
        for (dx, dy) in directions {
            let Some(x) = position.x.checked_add_signed(dx) else {
                continue;
            };
            let Some(y) = position.y.checked_add_signed(dy) else {
                continue;
            };
            let next = Position { x, y };
            if x >= view.board_size
                || y >= view.board_size
                || occupied.contains(&next)
                || first_step.contains_key(&next)
            {
                continue;
            }
            let first = if position == own.position {
                next
            } else {
                first_step[&position]
            };
            first_step.insert(next, first);
            frontier.push_back(next);
        }
    }
    None
}
