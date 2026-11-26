# AI Turn Tactics

AI Turn Tactics is a tick-based tactics engine for matches between LLM players. Players schedule actions, negotiate over messages, and compete to survive.

## Getting started

Install Rust 1.86 or later with Cargo, then run these commands from the project root.

```sh
cargo build --locked
cargo test --locked
```

Cargo downloads dependencies on the first build. The tests use fixed inputs and require no model or API credentials.

## Browser dashboard

```sh
cargo run --locked --bin dashboard
```

Open [127.0.0.1:8080](http://127.0.0.1:8080), select **New match**, and choose **Local Ollama** or **Demo players**. Ollama mode uses the default model for every slot unless you choose an override under **Player settings**. Set each player's starting prompt there.

## Local model

Run Ollama as a separate local service. Verify it is running and download the model if needed.

```sh
ollama list
ollama pull qwen3:4b-instruct
```

If the service is stopped, run `ollama serve` in a separate terminal and reopen match setup. The dashboard discovers installed local models through `/api/tags` and sends each player call to `/api/chat`. Set `AI_TACTICS_OLLAMA_URL` before dashboard startup if Ollama uses a different loopback HTTP origin. The default is `http://127.0.0.1:11434`.

The dashboard shows which player is thinking or retrying. Requests run one at a time, so a tick can take several model calls to settle. For a short first match, set the AP round limit to one.

## Live viewing and replays

Select a tank to inspect its private state and highlight its attack and gift range. Use the timeline to follow live play or review earlier frames. Completed matches are saved as JSON in `replays/` and reopen from **Replays**, including after restarting the dashboard. The replay contains the same private information available to the live observer.

Set `AI_TACTICS_PORT` or `AI_TACTICS_REPLAY_DIR` before launch to change the port or replay directory. The server listens on loopback.

## Driving a match

Create an `Engine` with a `MatchConfig`, send each pending `PlayerCall` to its player, and submit the player's JSON reply. Use `reply_schema()` for the reply contract. When the batch settles, call `advance_tick()` to resolve one tick or `advance_to_next_event()` to skip idle ticks. `observer_snapshot()` exposes private and public state for a trusted observer.

Run `cargo doc --locked --no-deps` for the library API and a working example.

For the current project state and open decisions, see [project_context.md](project_context.md). Contributor instructions are in [AGENTS.md](AGENTS.md).
