# Project context

## Goal

Build a small tactics engine for LLM-versus-LLM matches that reward planning, communication, and adaptation under incomplete information. The goal for each player is to be among the last standing.

## Current state

The Rust rules engine and local browser dashboard support both demo players and models served by a standalone local Ollama instance.

The local `qwen3:4b-instruct` integration completed a five-player, one-round smoke match with no validation retries and saved the players' memory in the replay. The run took about three minutes on this machine's CPU.

## Implementation milestones

- The Rust library provides `Engine::new`, `pending_calls`, `submit_reply`, and `advance_tick` for a match driver. `reply_schema()` generates the JSON schema from the reply types, and `observer_snapshot()` exposes the full committed match state. Fixed-input tests cover individual actions, tick ordering, reply validation, and player visibility.
- The `dashboard` binary serves the browser app over loopback. Demo and Ollama players use the same engine reply validator. The driver uses `advance_to_next_event()` to skip ticks without grants, due orders, or wake-ups.

## Player observations and memory

- Tank positions, health, and the available action types are public. A tank's current range and action-point balance are visible only to its player. Other players can infer range from game activity and messages.
- Each call includes the current board, public events since the player's previous call, newly delivered messages addressed to that player, and their private memory. The LLM chooses what to retain in memory so it can carry context across calls.
- Each call also lists the player's pending orders, scheduled ticks, and execution order.
- The wake-up payload is a typed Rust structure serialized as JSON. Include the current tick, wake reasons, player role, own AP and range, and private order results. Send the game rules and the configured starting prompt with every call.
- The engine holds one private text field per player in its match state. A valid reply can replace that field. Each wake-up reports the number of UTF-8 bytes used and the maximum allowed. An oversized replacement triggers the same whole-reply retry as a malformed response.
- The engine default is 32 KiB of memory per player, configurable in match setup. Ollama requests use a 16,384-token context window to leave room for the rules, observations, and memory on this machine. Context overflow is surfaced as a service error.
- Player replies must conform to a strict JSON schema for new orders, pending order changes, optional memory replacement, and required next wake-up conditions. Only structured orders control the game. Free text is available for messages and memory.

## Communication

- Living and eliminated players can send free messages with a minimum delivery delay of one tick. Public broadcasts reach all players, direct messages reach one recipient, and group messages reach a fixed set of members.
- A group room is identified by its exact member set, including the sender. Address a room by ID or by naming its recipients. The same member set reuses the same room across senders and delivery times. Changing the set uses a different room. A delayed message keeps the membership selected when it was queued.
- Each call lists the player's room IDs and memberships and delivers messages received since the previous wake-up. Players can select public, direct, and group messages separately as wake-up triggers.
- Store each delivered message once in match state with its sender, recipients, and delivery tick. Track each player's position in the message log and reuse the same snapshot on a retry.
- The engine uses opaque room IDs so the published IDs do not reveal the order in which unrelated groups were created.

## Game clock and actions

- The engine uses a shared clock measured in ticks. All players who wake on a tick receive observations from the same completed game state. The engine pauses until all replies are collected so reply speed gives no in-game advantage.
- Players can schedule, cancel, or reschedule individual pending orders. New orders and changes can affect the next tick or later. A message can wake its recipient at delivery if they opted in.
- Everyone wakes at match start. If the match continues, any AP received, damage taken, or elimination schedules a mandatory wake-up for the next tick. AP credits include regular and jury grants, gifts, and loot. This keeps players able to respond to changes in their tank state even when they choose to listen only for messages. Revival wakes the affected player on the tick it happens.
- Each reply chooses the next optional wake-up conditions from engine-defined events or requests a wake-up in a specified number of ticks. Optional events include incoming messages, AP grants, taking damage, and order failures. Optional AP grant and damage wake-ups can still occur on the event tick. Mandatory and optional reasons due on the same tick produce one call.
- For an invalid reply, make one retry with the same snapshot and feedback about the validation error. Malformed or incomplete replies and unknown player or room IDs all trigger a retry of the entire reply. If the retry is also invalid, treat the call as a pass and schedule the player for the next regular AP grant tick, including if they are on the jury.

### Tick order

1. Apply regular and jury AP grants. All granted AP is immediately available.
2. Resolve movement from the starting positions for the phase.
3. Resolve range upgrades and self-repair in the player's chosen order, then settle health gifts together. Gifts use positions after movement and ranges after upgrades.
4. Resolve attacks and apply damage simultaneously. Process eliminations, cancel outstanding orders, and split looted AP.
5. Settle AP gifts together for use from the next tick. Deliver due messages and apply jury votes and peace declarations. Votes due on a grant tick count toward the following grant. Check whether the match has ended.
6. If the match continues, call all players whose wake conditions fired. Wait for the entire batch, then keep the players' memory updates and queue orders and changes for future ticks.

## Board and starting conditions

- The first board is an open square grid. Obstacles are a possible later addition.
- Full matches start with at least five players, with six as the default. Five is the smallest roster that can leave three jurors and two living players, and six lets the jury award a bonus with three tanks still in play.
- The starting board size is `ceil(sqrt(15 × player count))` squares per side, giving six players a 10×10 board. Scaling by area keeps the starting density roughly constant.
- Matches can use evenly spaced spawns, random spawns with a minimum separation, or random distinct spawns without a separation constraint. The default uses random spawns outside each other's starting range whenever possible.
- Tanks start with 3 health, 0 AP, and attack range 2. Movement is horizontal or vertical, one square per move. Range uses Manhattan distance, the sum of the horizontal and vertical distances.

## Movement and combat

- Each tank can submit at most one move per tick. A move requires its destination to be empty at the start of the movement phase. Moves into occupied squares and otherwise valid moves contesting the same square fail for that tick. Players keep their AP and can schedule another attempt themselves.
- Attacks target players. Range is checked against positions after movement, so a target that moves out of reach causes the attack to fail without spending AP.
- A tank can fire several shots per tick. The engine tries them in the player's specified order and fires as many valid shots as the tank can afford. It reports the outcome of each shot.
- The engine checks attacks before applying their damage together. Tanks that land lethal shots on each other on the same tick are both destroyed. If combat destroys every remaining tank on the same tick, those players draw.
- Elimination cancels all queued orders. The wreck stays on its square and blocks movement. A health gift revives it on that square with the gifted health, 0 AP, and its previous range upgrades. Keeping the upgrades gives allies a reason to revive a particular tank.
- Split a destroyed tank's remaining AP into equal whole shares among all attackers who contributed damage that tick, including attackers destroyed on the same tick. Discard the remainder and shares belonging to destroyed attackers. Surviving attackers receive their shares before end-of-tick AP gifts settle.
- Players receive the outcomes of their own movement and attack orders on their next call.
- The target has a configurable private attacker report. The default names everyone who dealt damage on the lethal tick only. Other modes name the attacker for every hit or omit attacker names entirely.
- Players infer other tanks' damage and eliminations from the board or from messages. The public event feed omits both.
- The initial public event feed records regular AP rounds and public peace declarations and revocations.

## Action costs and effects

- A successful move costs 1 AP. A successful shot costs 1 AP and deals exactly 1 damage.
- The engine checks AP affordability at execution and follows the player's chosen order wherever the tick rules allow. Scheduling leaves AP available for other actions, and players can plan around future grants.
- Repair costs 3 AP and adds 1 health point. A range upgrade costs 3 AP and increases range by 1. The player prompt states the cost and effect of each action on its own line. A tank may buy either action several times on the same tick. The rules impose no health or range cap.
- A single gift can transfer any positive whole amount of health or AP to a tank in range. If the donor can afford only part of the amount, transfer that part. A health gift must leave the donor with at least 1 health. Health received on a tick counts for combat immediately and can be given away starting on the next tick.
- Only living tanks can send or receive AP gifts. The engine credits those gifts together at the end of the tick. The recipient can spend the AP from the next tick onward, which prevents same-tick gift chains.

## Match outcomes and peace agreements

- A player can declare peace with a named group of players and keep several agreements active. Each declaration remains active until revoked and can be public or private at the declaring player's choice.
- For the first version, an agreement uses an explicit list of members, including the player who declares it. After combat, the engine checks for a shared win when the group matches the entire surviving roster and each member has declared peace with that group.
- Agreements only affect the match outcome. Members can still act against each other while the match continues.
- The match has a configurable limit, defaulting to 30 regular AP rounds, or 3,000 ticks at the starting grant interval. If no win has resolved by the limit, the match ends in a draw.
- The final result is for observers. Match start is handled by the initial player calls. Neither uses the player event feed.

## Action points and jury

- All living players receive one action point at the same recurring tick. The default interval is 100 ticks. Players use the grant interval and their current observation to work out when they can afford an action.
- Eliminated players form a jury. Each juror may cast one vote for a living player per grant interval and change it until the next grant.
- Every living player with at least three votes receives one bonus action point at the grant. Ballots and totals are private. Bonus recipients receive a private notification.
- Regular and jury grants resolve at the start of the tick. That AP is available to actions already scheduled for the same tick.

## Match setup and observation

- Configure and launch matches from a local browser. The browser also provides live observation and step-through replays.
- Completed replays are published atomically as versioned JSON in `replays/` by default. Each frame stores the current observer state and appends new activity. Replays can reopen after a server restart. In-progress matches live in memory.
- The first observer view exposes the full match state, including private player information.
- Selecting a living tank highlights the cells within its Manhattan attack and gift range at the displayed tick. The highlight follows the selected tank's position and range during live play and in replays.
- Ollama is installed independently of the game. The dashboard discovers installed local models via `/api/tags` and uses the same `/api/chat` endpoint as manual tests. The default endpoint is `http://127.0.0.1:11434`. Setup prefers `qwen3:4b-instruct` when installed, with a configurable model and starting prompt per slot.
- Send a fresh system and user message with each non-streaming chat request. The system message supplies the rules and starting prompt. The user message contains the player-safe observation and retry feedback. The generated schema goes in `format`, and the engine still validates `message.content` and retries invalid replies once. HTTP or Ollama service errors stop the match and surface in the observer.
- Submit model requests one at a time to limit CPU contention. The engine commits replies only when the entire wake-up batch settles. The live observer shows the active request and replays include selected models and validation retries.

## Open decisions

- Spawn fallback if a later version supports custom board sizes. The current board formula guarantees the default separation.
- Revisit the default memory limit and public event feed during playtesting.
- Measure model latency and reply quality in longer competitive matches.
