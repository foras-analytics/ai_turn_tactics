use super::*;

fn pos(x: u32, y: u32) -> Position {
    Position { x, y }
}

fn fixture(config: MatchConfig, setup: impl FnOnce(&mut BTreeMap<PlayerId, Player>)) -> Engine {
    let mut engine = Engine::new(config).unwrap();
    setup(&mut engine.players);
    engine.calls.clear();
    let reasons = engine
        .players
        .keys()
        .map(|&id| (id, vec![WakeReason::Start]))
        .collect();
    engine.open_calls(reasons);
    engine
}

fn turn(orders: Vec<(Tick, Action)>, after: Option<Tick>, events: Vec<WakeEvent>) -> PlayerReply {
    PlayerReply {
        orders: orders
            .into_iter()
            .map(|(tick, action)| NewOrder { tick, action })
            .collect(),
        changes: vec![],
        memory: None,
        wake: WakeRequest {
            after_ticks: after,
            events,
        },
    }
}

fn idle() -> PlayerReply {
    turn(vec![], Some(10_000), vec![])
}

fn give(engine: &mut Engine, id: PlayerId, reply: PlayerReply) {
    assert_eq!(
        engine
            .submit_reply(id, &serde_json::to_string(&reply).unwrap())
            .unwrap(),
        ReplyOutcome::Accepted,
    );
}

fn settle(engine: &mut Engine) {
    let ids: Vec<_> = engine
        .pending_calls()
        .map(|call| call.observation.player)
        .collect();
    for id in ids {
        give(engine, id, idle());
    }
}

fn view(engine: &Engine, id: PlayerId) -> &Observation {
    &engine
        .pending_calls()
        .find(|call| call.observation.player == id)
        .unwrap()
        .observation
}

fn mov(x: u32, y: u32) -> Action {
    Action::Move {
        destination: pos(x, y),
    }
}

#[test]
fn startup_uses_shared_public_board_and_separated_spawns() {
    let engine = Engine::new(MatchConfig::default()).unwrap();
    let a = view(&engine, 1);
    let b = view(&engine, 6);
    assert_eq!(a.board, b.board);
    assert_eq!(a.board_size, 10);
    assert_eq!(a.wake_reasons, [WakeReason::Start]);
    assert_eq!((a.action_points, a.range), (0, 2));
    for (i, tank) in a.board.iter().enumerate() {
        assert_eq!(tank.health, 3);
        for other in &a.board[i + 1..] {
            assert!(tank.position.distance(other.position) > a.range);
        }
    }
    for spawn_mode in [SpawnMode::EvenlySpaced, SpawnMode::Random] {
        let cfg = MatchConfig {
            spawn_mode,
            ..MatchConfig::default()
        };
        let first = Engine::new(cfg.clone()).unwrap();
        let second = Engine::new(cfg).unwrap();
        let board = &view(&first, 1).board;
        assert_eq!(board, &view(&second, 1).board);
        assert_eq!(
            board
                .iter()
                .map(|tank| tank.position)
                .collect::<BTreeSet<_>>()
                .len(),
            6
        );
    }
}

#[test]
fn batch_commits_only_after_all_replies_and_preserves_the_starting_prompt() {
    let mut config = MatchConfig::default();
    config.players[0].starting_prompt = "Stay mobile.".into();
    let mut engine = Engine::new(config).unwrap();
    let mut reply = turn(vec![], Some(1), vec![]);
    reply.memory = Some("ü".into());
    give(&mut engine, 1, reply);
    assert_eq!(engine.advance_tick(), Err(EngineError::AwaitingReplies));
    assert_eq!(engine.tick(), 0);
    assert!(engine.observer_snapshot().players[0].memory.is_empty());
    assert!(view(&engine, 2).memory.is_empty());
    settle(&mut engine);
    engine.advance_tick().unwrap();
    let call = engine.pending_calls().next().unwrap();
    assert_eq!(call.starting_prompt, "Stay mobile.");
    assert!(!call.rules.is_empty());
    assert_eq!(call.observation.memory, "ü");
    assert_eq!(call.observation.memory_used_bytes, 2);
    assert_eq!(call.observation.memory_limit_bytes, 32768);
}

#[test]
fn unknown_references_retry_the_whole_reply_and_fall_back_to_the_next_grant() {
    let config = MatchConfig {
        ap_interval: 2,
        ..MatchConfig::default()
    };
    let mut engine = Engine::new(config).unwrap();
    let original = view(&engine, 1).clone();
    let mut reply = turn(vec![(1, Action::Attack { target: 999 })], Some(1), vec![]);
    reply.memory = Some("should not commit".into());
    let first = engine
        .submit_reply(1, &serde_json::to_string(&reply).unwrap())
        .unwrap();
    assert!(matches!(first, ReplyOutcome::Retry { error } if error.contains("unknown player 999")));
    assert_eq!(view(&engine, 1), &original);

    reply.orders[0].action = Action::Message {
        address: MessageAddress::Room { room_id: 999 },
        text: "no room".into(),
    };
    let second = engine
        .submit_reply(1, &serde_json::to_string(&reply).unwrap())
        .unwrap();
    assert!(matches!(second, ReplyOutcome::Passed { error } if error.contains("unknown room 999")));
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(engine.pending_calls().count(), 0);
    engine.advance_tick().unwrap();
    let observation = view(&engine, 1);
    assert_eq!(observation.wake_reasons, [WakeReason::Scheduled]);
    assert!(observation.memory.is_empty());
    assert!(observation.pending_orders.is_empty());
    assert!(observation.order_results.is_empty());
}

#[test]
fn schema_and_parser_reject_extra_fields_in_actions_and_addresses() {
    let schema = serde_json::to_value(reply_schema()).unwrap();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["$defs"]["WakeRequest"]["properties"]["after_ticks"]["minimum"],
        1
    );
    for bad_action in [
        r#"{"type":"repair","amount":3}"#,
        r#"{"type":"attack","target":2,"extra":true}"#,
        r#"{"type":"message","address":{"channel":"public","recipient":2},"text":"hello"}"#,
    ] {
        let body = format!(
            r#"{{"orders":[{{"tick":1,"action":{bad_action}}}],"changes":[],"wake":{{"after_ticks":1,"events":[]}}}}"#
        );
        let mut engine = Engine::new(MatchConfig::default()).unwrap();
        assert!(
            matches!(
                engine.submit_reply(1, &body).unwrap(),
                ReplyOutcome::Retry { .. }
            ),
            "{bad_action}"
        );
    }
}

#[test]
fn oversized_memory_is_validated_in_bytes_and_retry_can_replace_entire_reply() {
    let config = MatchConfig {
        memory_limit_bytes: 3,
        ..MatchConfig::default()
    };
    let mut engine = Engine::new(config).unwrap();
    let mut reply = turn(vec![], Some(1), vec![]);
    reply.memory = Some("ää".into());
    assert!(matches!(
        engine.submit_reply(1, &serde_json::to_string(&reply).unwrap()).unwrap(),
        ReplyOutcome::Retry { error } if error.contains("4 bytes")
    ));
    reply.memory = Some("✓".into());
    give(&mut engine, 1, reply);
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(view(&engine, 1).memory_used_bytes, 3);
}

#[test]
fn occupied_and_contested_moves_fail_without_spending_ap() {
    let mut engine = fixture(MatchConfig::default(), |players| {
        for (id, x) in [(1, 0), (2, 2), (3, 3), (4, 5), (5, 6), (6, 8)] {
            let player = players.get_mut(&id).unwrap();
            player.position = pos(x, 0);
            player.ap = 1;
        }
        players.get_mut(&5).unwrap().health = 0;
    });
    for (id, dest) in [(1, 1), (2, 1), (3, 2), (4, 6)] {
        give(
            &mut engine,
            id,
            turn(vec![(1, mov(dest, 0))], None, vec![WakeEvent::OrderFailure]),
        );
    }
    settle(&mut engine);
    engine.advance_tick().unwrap();
    for (id, reason) in [
        (1, Failure::Contested),
        (2, Failure::Contested),
        (3, Failure::Occupied),
        (4, Failure::Occupied),
    ] {
        let observation = view(&engine, id);
        assert_eq!(observation.action_points, 1);
        assert_eq!(
            observation.order_results[0].outcome,
            OrderOutcome::Failed { reason }
        );
    }
}

#[test]
fn moves_use_the_start_of_movement_snapshot_even_when_an_occupant_leaves() {
    let mut engine = fixture(MatchConfig::default(), |players| {
        players.get_mut(&1).unwrap().position = pos(0, 0);
        players.get_mut(&2).unwrap().position = pos(1, 0);
        players.get_mut(&1).unwrap().ap = 1;
        players.get_mut(&2).unwrap().ap = 1;
        for id in 3..=6 {
            players.get_mut(&id).unwrap().position = pos(id, 7);
        }
    });
    give(&mut engine, 1, turn(vec![(1, mov(1, 0))], Some(1), vec![]));
    give(&mut engine, 2, turn(vec![(1, mov(2, 0))], Some(1), vec![]));
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(
        view(&engine, 1).order_results[0].outcome,
        OrderOutcome::Failed {
            reason: Failure::Occupied
        }
    );
    assert_eq!(
        view(&engine, 2).order_results[0].outcome,
        OrderOutcome::Applied
    );
}

#[test]
fn same_tick_moves_change_range_and_a_volley_spends_ap_in_player_order() {
    let mut engine = fixture(MatchConfig::default(), |players| {
        for (id, x, y, ap) in [
            (1, 0, 0, 3),
            (2, 3, 0, 0),
            (3, 1, 2, 1),
            (4, 8, 0, 0),
            (5, 8, 1, 0),
            (6, 8, 2, 0),
        ] {
            let player = players.get_mut(&id).unwrap();
            player.position = pos(x, y);
            player.ap = ap;
        }
    });
    give(
        &mut engine,
        1,
        turn(
            vec![
                (1, Action::Attack { target: 3 }),
                (1, Action::Attack { target: 2 }),
                (1, mov(1, 0)),
                (1, Action::Attack { target: 2 }),
                (1, Action::Attack { target: 2 }),
            ],
            Some(1),
            vec![],
        ),
    );
    give(&mut engine, 2, turn(vec![], None, vec![WakeEvent::Damage]));
    give(&mut engine, 3, turn(vec![(1, mov(1, 3))], Some(1), vec![]));
    settle(&mut engine);
    engine.advance_tick().unwrap();
    let shooter = view(&engine, 1);
    assert_eq!(shooter.action_points, 0);
    assert_eq!(
        shooter
            .order_results
            .iter()
            .map(|r| r.outcome.clone())
            .collect::<Vec<_>>(),
        [
            OrderOutcome::Applied,
            OrderOutcome::Failed {
                reason: Failure::OutOfRange
            },
            OrderOutcome::Applied,
            OrderOutcome::Applied,
            OrderOutcome::Failed {
                reason: Failure::InsufficientAp
            },
        ]
    );
    let target = view(&engine, 2);
    assert_eq!(target.damage[0].damage, 2);
    assert_eq!(target.damage[0].attackers, None);
    assert!(view(&engine, 3).damage.is_empty());
    assert!(target.public_events.is_empty());
    assert_eq!(
        engine.observer_snapshot().players[1].damage[0].attackers,
        Some(vec![1, 1])
    );
}

#[test]
fn simultaneous_elimination_cancels_orders_and_splits_loot_including_dead_attackers() {
    let mut engine = fixture(MatchConfig::default(), |players| {
        for (id, x, ap, hp) in [(1, 0, 1, 1), (2, 1, 6, 2), (3, 2, 1, 3)] {
            let player = players.get_mut(&id).unwrap();
            player.position = pos(x, 0);
            player.ap = ap;
            player.health = hp;
        }
    });
    give(
        &mut engine,
        1,
        turn(
            vec![(1, Action::Attack { target: 2 })],
            None,
            vec![WakeEvent::ApGrant],
        ),
    );
    give(
        &mut engine,
        2,
        turn(
            vec![
                (1, Action::Attack { target: 1 }),
                (
                    1,
                    Action::Message {
                        address: MessageAddress::Direct { recipient: 3 },
                        text: "cancelled".into(),
                    },
                ),
                (2, Action::Repair {}),
            ],
            None,
            vec![WakeEvent::ApGrant],
        ),
    );
    give(
        &mut engine,
        3,
        turn(vec![(1, Action::Attack { target: 2 })], Some(1), vec![]),
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(engine.pending_calls().count(), 1);
    assert_eq!(view(&engine, 3).action_points, 2);
    settle(&mut engine);
    engine.advance_to_next_event().unwrap();
    assert_eq!(engine.tick(), 2);
    assert_eq!(view(&engine, 3).wake_reasons, [WakeReason::ApGained]);
    assert_eq!(view(&engine, 1).role, Role::Juror);
    let dead = view(&engine, 2);
    assert_eq!(
        dead.wake_reasons,
        [WakeReason::DamageTaken, WakeReason::Eliminated]
    );
    assert_eq!(dead.damage[0].attackers, Some(vec![1, 3]));
    assert_eq!(dead.action_points, 0);
    assert!(dead.pending_orders.is_empty());
    assert_eq!(
        dead.order_results
            .iter()
            .filter(|r| r.outcome == OrderOutcome::CancelledByElimination)
            .count(),
        2
    );
    assert_eq!(view(&engine, 3).action_points, 2);
    assert!(view(&engine, 3).messages.is_empty());
}

#[test]
fn upgrades_and_repair_follow_the_players_order_then_enable_revival_before_combat() {
    let mut engine = fixture(MatchConfig::default(), |players| {
        for (id, x, health, ap, range) in [(1, 0, 3, 6, 2), (2, 3, 0, 0, 5), (3, 4, 3, 1, 2)] {
            let player = players.get_mut(&id).unwrap();
            player.position = pos(x, 0);
            player.health = health;
            player.ap = ap;
            player.range = range;
        }
        players.get_mut(&2).unwrap().vote = Some(3);
    });
    give(
        &mut engine,
        1,
        turn(
            vec![
                (1, Action::Repair {}),
                (1, Action::UpgradeRange {}),
                (1, Action::UpgradeRange {}),
                (
                    1,
                    Action::GiftHealth {
                        target: 2,
                        amount: 4,
                    },
                ),
            ],
            Some(1),
            vec![],
        ),
    );
    give(
        &mut engine,
        3,
        turn(vec![(1, Action::Attack { target: 2 })], Some(1), vec![]),
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    let donor = view(&engine, 1);
    assert_eq!(donor.range, 3);
    assert_eq!(
        donor.order_results[2].outcome,
        OrderOutcome::Failed {
            reason: Failure::InsufficientAp
        }
    );
    assert_eq!(
        donor.order_results[3].outcome,
        OrderOutcome::Transferred {
            requested: 4,
            transferred: 3
        }
    );
    let revived = view(&engine, 2);
    assert_eq!(revived.wake_reasons, [WakeReason::Revived]);
    assert_eq!(
        (revived.role, revived.range, revived.action_points),
        (Role::Tank, 5, 0)
    );
    assert_eq!(revived.own_vote, None);
    assert_eq!(revived.damage[0].damage, 1);
    assert_eq!(
        revived
            .board
            .iter()
            .find(|tank| tank.player == 2)
            .unwrap()
            .health,
        2
    );
}

#[test]
fn gift_transfers_are_partial_and_cannot_chain_on_the_same_tick() {
    let mut engine = fixture(MatchConfig::default(), |players| {
        for (id, x, health, ap) in [(1, 0, 4, 3), (2, 1, 1, 0), (3, 2, 3, 0)] {
            let player = players.get_mut(&id).unwrap();
            player.position = pos(x, 0);
            player.health = health;
            player.ap = ap;
        }
    });
    give(
        &mut engine,
        1,
        turn(
            vec![
                (
                    1,
                    Action::GiftHealth {
                        target: 2,
                        amount: 5,
                    },
                ),
                (
                    1,
                    Action::GiftAp {
                        target: 2,
                        amount: 5,
                    },
                ),
            ],
            Some(1),
            vec![],
        ),
    );
    give(
        &mut engine,
        2,
        turn(
            vec![
                (
                    1,
                    Action::GiftHealth {
                        target: 3,
                        amount: 1,
                    },
                ),
                (
                    1,
                    Action::GiftAp {
                        target: 3,
                        amount: 1,
                    },
                ),
                (
                    2,
                    Action::GiftAp {
                        target: 3,
                        amount: 1,
                    },
                ),
            ],
            Some(1),
            vec![],
        ),
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    let donor = view(&engine, 1);
    assert_eq!(donor.action_points, 0);
    assert!(donor.order_results.iter().all(|r| r.outcome
        == OrderOutcome::Transferred {
            requested: 5,
            transferred: 3
        }));
    let recipient = view(&engine, 2);
    assert_eq!(recipient.action_points, 3);
    assert_eq!(recipient.pending_orders.len(), 1);
    assert_eq!(
        recipient.order_results[0].outcome,
        OrderOutcome::Failed {
            reason: Failure::InsufficientHealth
        }
    );
    assert_eq!(
        recipient.order_results[1].outcome,
        OrderOutcome::Failed {
            reason: Failure::InsufficientAp
        }
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(view(&engine, 2).wake_reasons, [WakeReason::ApGained]);
    assert_eq!(engine.observer_snapshot().players[2].action_points, 1);
    settle(&mut engine);
    engine.advance_to_next_event().unwrap();
    assert_eq!(engine.tick(), 3);
    assert_eq!(view(&engine, 3).wake_reasons, [WakeReason::ApGained]);
}

#[test]
fn jurors_can_change_votes_and_granted_ap_can_fund_an_action_on_the_grant_tick() {
    let config = MatchConfig {
        ap_interval: 3,
        ..MatchConfig::default()
    };
    let mut engine = fixture(config, |players| {
        players.get_mut(&1).unwrap().position = pos(0, 0);
        players.get_mut(&2).unwrap().position = pos(1, 0);
        players.get_mut(&3).unwrap().position = pos(9, 9);
        for id in 4..=6 {
            let player = players.get_mut(&id).unwrap();
            player.position = pos(id, 9);
            player.health = 0;
        }
    });
    give(
        &mut engine,
        1,
        turn(
            vec![
                (3, Action::Attack { target: 2 }),
                (3, Action::Attack { target: 2 }),
            ],
            None,
            vec![WakeEvent::ApGrant],
        ),
    );
    give(
        &mut engine,
        2,
        turn(vec![], None, vec![WakeEvent::ApGrant, WakeEvent::Damage]),
    );
    give(&mut engine, 3, turn(vec![], None, vec![WakeEvent::ApGrant]));
    for id in 4..=6 {
        give(
            &mut engine,
            id,
            turn(
                vec![
                    (1, Action::Vote { target: 3 }),
                    (2, Action::Vote { target: 1 }),
                    (3, Action::Vote { target: 3 }),
                ],
                None,
                vec![WakeEvent::ApGrant],
            ),
        );
    }
    for _ in 0..3 {
        engine.advance_tick().unwrap();
    }
    assert_eq!(view(&engine, 1).jury_bonus_ticks, [3]);
    assert_eq!(view(&engine, 1).action_points, 0);
    assert_eq!(view(&engine, 2).damage[0].damage, 2);
    assert_eq!(
        view(&engine, 2).wake_reasons,
        [
            WakeReason::Event(WakeEvent::ApGrant),
            WakeReason::Event(WakeEvent::Damage),
        ]
    );
    assert!(view(&engine, 3).jury_bonus_ticks.is_empty());
    assert_eq!(
        view(&engine, 3).public_events,
        [PublicEvent::ApRound { tick: 3 }]
    );
    assert_eq!(view(&engine, 4).own_vote, Some(3));
    give(
        &mut engine,
        2,
        turn(vec![], Some(1), vec![WakeEvent::PublicMessage]),
    );
    settle(&mut engine);
    engine.advance_to_next_event().unwrap();
    assert_eq!(engine.tick(), 4);
    assert_eq!(engine.pending_calls().count(), 3);
    assert_eq!(
        view(&engine, 2).wake_reasons,
        [
            WakeReason::Scheduled,
            WakeReason::ApGained,
            WakeReason::DamageTaken
        ]
    );
}

#[test]
fn room_ids_reuse_exact_membership_and_messages_respect_wake_preferences_and_cursors() {
    let mut engine = Engine::new(MatchConfig::default()).unwrap();
    let send = |address, text: &str| Action::Message {
        address,
        text: text.into(),
    };
    give(
        &mut engine,
        1,
        turn(
            vec![
                (1, send(MessageAddress::Public {}, "everyone")),
                (1, send(MessageAddress::Direct { recipient: 2 }, "direct")),
                (
                    2,
                    send(
                        MessageAddress::Group {
                            recipients: vec![2, 3],
                        },
                        "group",
                    ),
                ),
            ],
            Some(3),
            vec![],
        ),
    );
    give(
        &mut engine,
        2,
        turn(
            vec![(
                1,
                send(
                    MessageAddress::Group {
                        recipients: vec![3, 1],
                    },
                    "same room",
                ),
            )],
            None,
            vec![WakeEvent::DirectMessage],
        ),
    );
    give(
        &mut engine,
        3,
        turn(vec![], None, vec![WakeEvent::GroupMessage]),
    );
    give(&mut engine, 4, turn(vec![], Some(1), vec![]));
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(
        view(&engine, 2).wake_reasons,
        [WakeReason::Event(WakeEvent::DirectMessage)]
    );
    assert_eq!(view(&engine, 2).messages.len(), 3);
    assert_eq!(view(&engine, 4).messages.len(), 1);
    assert!(view(&engine, 4).rooms.is_empty());
    let room_id = view(&engine, 3).rooms[0].id;
    assert_eq!(view(&engine, 3).rooms[0].members, [1, 2, 3]);
    let intruder = turn(
        vec![(
            2,
            send(MessageAddress::Room { room_id }, "from a nonmember"),
        )],
        Some(1),
        vec![],
    );
    let response = engine
        .submit_reply(4, &serde_json::to_string(&intruder).unwrap())
        .unwrap();
    assert_eq!(
        response,
        ReplyOutcome::Retry {
            error: format!("unknown room {room_id}")
        }
    );
    give(
        &mut engine,
        3,
        turn(
            vec![
                (2, send(MessageAddress::Room { room_id }, "reply by ID")),
                (
                    3,
                    send(
                        MessageAddress::Group {
                            recipients: vec![1, 4],
                        },
                        "different room",
                    ),
                ),
            ],
            Some(1),
            vec![],
        ),
    );
    give(&mut engine, 2, turn(vec![], Some(1), vec![]));
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(
        view(&engine, 2)
            .messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        ["group", "reply by ID"]
    );
    assert!(
        view(&engine, 2)
            .messages
            .iter()
            .all(|m| m.room == Some(room_id))
    );
    assert_eq!(view(&engine, 3).messages.len(), 2);
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(view(&engine, 1).rooms.len(), 2);
    let different = view(&engine, 1)
        .rooms
        .iter()
        .find(|room| room.members == [1, 3, 4])
        .unwrap();
    assert_ne!(different.id, room_id);
    assert_eq!(engine.observer_snapshot().messages.len(), 6);
}

#[test]
fn player_json_has_no_other_players_private_state_or_damage_history() {
    let config = MatchConfig {
        attacker_disclosure: AttackerDisclosure::Hidden,
        ..MatchConfig::default()
    };
    let mut engine = fixture(config, |players| {
        let one = players.get_mut(&1).unwrap();
        one.position = pos(0, 0);
        one.ap = 8;
        one.range = 9;
        one.memory = "private notes".into();
        players.get_mut(&2).unwrap().position = pos(1, 0);
    });
    give(
        &mut engine,
        1,
        turn(
            vec![
                (
                    1,
                    Action::DeclarePeace {
                        members: vec![1, 2],
                        public: false,
                    },
                ),
                (1, Action::Attack { target: 2 }),
                (2, Action::Repair {}),
            ],
            Some(1),
            vec![],
        ),
    );
    give(&mut engine, 2, turn(vec![], Some(1), vec![]));
    give(&mut engine, 3, turn(vec![], Some(1), vec![]));
    settle(&mut engine);
    engine.advance_tick().unwrap();
    let one = view(&engine, 1);
    assert_eq!(one.pending_orders.len(), 1);
    assert_eq!(one.own_peace.len(), 1);
    let two = view(&engine, 2);
    let three = view(&engine, 3);
    assert!(two.own_peace.is_empty());
    assert!(two.public_events.is_empty());
    assert!(two.pending_orders.is_empty());
    assert!(two.memory.is_empty());
    assert_eq!(two.damage[0].attackers, None);
    assert!(three.damage.is_empty());
    let json = serde_json::to_value(two).unwrap();
    assert_eq!(
        json["board"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["health", "player", "position"]
    );
    assert_eq!(json["action_points"], 0);
    assert_eq!(json["range"], 2);
}

#[test]
fn rescheduling_and_cancellation_keep_ownership_and_future_execution_order() {
    let mut engine = Engine::new(MatchConfig::default()).unwrap();
    give(
        &mut engine,
        1,
        turn(
            vec![(4, Action::Repair {}), (3, Action::UpgradeRange {})],
            Some(1),
            vec![],
        ),
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(
        view(&engine, 1)
            .pending_orders
            .iter()
            .map(|o| o.id)
            .collect::<Vec<_>>(),
        [2, 1]
    );
    let mut reply = turn(vec![], Some(1), vec![]);
    reply.changes = vec![
        OrderChange::Cancel { order_id: 2 },
        OrderChange::Reschedule {
            order_id: 1,
            tick: 3,
        },
    ];
    give(&mut engine, 1, reply);
    engine.advance_tick().unwrap();
    assert_eq!(
        view(&engine, 1).pending_orders,
        [PendingOrder {
            id: 1,
            tick: 3,
            action: Action::Repair {}
        }]
    );
}

#[test]
fn exact_survivor_set_can_win_by_peace_on_the_last_tick() {
    let config = MatchConfig {
        ap_interval: 2,
        ap_round_limit: 1,
        ..MatchConfig::default()
    };
    let mut engine = fixture(config, |players| {
        for id in 3..=6 {
            players.get_mut(&id).unwrap().health = 0;
        }
    });
    give(
        &mut engine,
        1,
        turn(
            vec![
                (
                    1,
                    Action::DeclarePeace {
                        members: vec![1, 2],
                        public: true,
                    },
                ),
                (
                    1,
                    Action::DeclarePeace {
                        members: vec![1, 2, 3],
                        public: false,
                    },
                ),
            ],
            Some(1),
            vec![],
        ),
    );
    give(
        &mut engine,
        2,
        turn(
            vec![(
                2,
                Action::DeclarePeace {
                    members: vec![2, 1],
                    public: false,
                },
            )],
            Some(1),
            vec![],
        ),
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(view(&engine, 1).own_peace.len(), 2);
    assert_eq!(
        view(&engine, 2).public_events,
        [PublicEvent::Peace {
            tick: 1,
            player: 1,
            members: vec![1, 2],
            active: true
        }]
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(
        engine.outcome(),
        Some(&MatchOutcome::Win {
            players: vec![1, 2]
        })
    );
    assert_eq!(engine.pending_calls().count(), 0);
    assert_eq!(engine.advance_tick(), Err(EngineError::MatchFinished));
}

#[test]
fn mutual_destruction_of_final_tanks_is_a_draw() {
    let mut engine = fixture(MatchConfig::default(), |players| {
        for (id, x) in [(1, 0), (2, 1)] {
            let player = players.get_mut(&id).unwrap();
            player.position = pos(x, 0);
            player.health = 1;
            player.ap = 1;
        }
        for id in 3..=6 {
            players.get_mut(&id).unwrap().health = 0;
        }
    });
    give(
        &mut engine,
        1,
        turn(vec![(1, Action::Attack { target: 2 })], Some(1), vec![]),
    );
    give(
        &mut engine,
        2,
        turn(vec![(1, Action::Attack { target: 1 })], Some(1), vec![]),
    );
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert_eq!(
        engine.outcome(),
        Some(&MatchOutcome::Draw {
            reason: DrawReason::AllRemainingTanksDestroyed,
            players: vec![1, 2],
        })
    );
}

#[test]
fn the_configured_round_limit_ends_an_unresolved_match() {
    let config = MatchConfig {
        ap_interval: 2,
        ap_round_limit: 1,
        ..MatchConfig::default()
    };
    let mut engine = Engine::new(config).unwrap();
    settle(&mut engine);
    engine.advance_tick().unwrap();
    assert!(engine.outcome().is_none());
    engine.advance_tick().unwrap();
    assert_eq!(
        engine.outcome(),
        Some(&MatchOutcome::Draw {
            reason: DrawReason::RoundLimit,
            players: vec![1, 2, 3, 4, 5, 6],
        })
    );
}

#[test]
fn event_advancement_matches_stepping_through_idle_ticks() {
    let make_game = || {
        let mut engine = Engine::new(MatchConfig::default()).unwrap();
        give(
            &mut engine,
            1,
            turn(
                vec![(
                    10,
                    Action::Message {
                        address: MessageAddress::Direct { recipient: 2 },
                        text: "Ready.".into(),
                    },
                )],
                Some(20),
                vec![],
            ),
        );
        give(
            &mut engine,
            2,
            turn(vec![], None, vec![WakeEvent::DirectMessage]),
        );
        settle(&mut engine);
        engine
    };
    let mut stepped = make_game();
    let mut skipped = make_game();
    for expected in [10, 20, 100, 101] {
        while stepped.tick() < expected {
            stepped.advance_tick().unwrap();
        }
        skipped.advance_to_next_event().unwrap();
        assert_eq!(skipped.tick(), expected);
        if expected == 101 {
            assert_eq!(view(&skipped, 2).wake_reasons, [WakeReason::ApGained]);
        }
        assert_eq!(
            serde_json::to_value(skipped.observer_snapshot()).unwrap(),
            serde_json::to_value(stepped.observer_snapshot()).unwrap(),
        );
        assert_eq!(
            skipped.pending_calls().collect::<Vec<_>>(),
            stepped.pending_calls().collect::<Vec<_>>()
        );
        settle(&mut stepped);
        settle(&mut skipped);
    }
}
