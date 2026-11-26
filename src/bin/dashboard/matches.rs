//! Match driver and local replay archive.

use std::{
    collections::BTreeMap,
    fs, io,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use ai_turn_tactics::{
    Engine, MatchConfig, ObserverSnapshot, PlayerCall, PlayerId, ReplyOutcome, Tick,
};
use serde::Serialize;
use serde_json::{Value, json};

use super::{demo, ollama::Ollama};

pub type ActiveMatch = Arc<Mutex<MatchRun>>;

pub enum Players {
    Demo,
    Ollama { server: Ollama, models: Vec<String> },
}

impl Players {
    fn model(&self, player: PlayerId) -> Option<&str> {
        match self {
            Self::Demo => None,
            Self::Ollama { models, .. } => Some(&models[(player - 1) as usize]),
        }
    }

    fn reply(&self, call: &PlayerCall) -> Result<String, String> {
        match self {
            Self::Demo => {
                Ok(serde_json::to_string(&demo::reply(&call.observation)).expect("typed reply"))
            }
            Self::Ollama { server, models } => {
                server.reply(&models[(call.observation.player - 1) as usize], call)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Complete,
    Failed,
}

#[derive(Clone, Serialize)]
pub struct ReplayInfo {
    pub id: String,
    pub created_ms: u64,
    pub config: MatchConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player_models: Option<Vec<String>>,
}

#[derive(Serialize)]
struct Inference {
    player: PlayerId,
    tick: Tick,
    retry: bool,
}

pub struct MatchRun {
    pub info: ReplayInfo,
    pub status: Status,
    pub error: Option<String>,
    pub frames: Vec<Value>,
    inference: Option<Inference>,
}

impl MatchRun {
    pub fn new(info: ReplayInfo) -> Self {
        Self {
            info,
            status: Status::Running,
            error: None,
            frames: Vec::new(),
            inference: None,
        }
    }

    /// The cursor is valid only for the match ID supplied by the browser.
    pub fn read_from(&self, id: Option<&str>, after: usize) -> Value {
        let start = if id == Some(&self.info.id) {
            after.min(self.frames.len())
        } else {
            0
        };
        json!({
            "info": self.info,
            "status": self.status,
            "error": self.error,
            "inference": self.inference,
            "offset": start,
            "frames": &self.frames[start..],
        })
    }
}

#[derive(Default)]
struct Capture {
    messages: usize,
    public_events: usize,
    results: BTreeMap<PlayerId, usize>,
    damage: BTreeMap<PlayerId, usize>,
}

/// Record complete current player state and just the new history entries.
/// The observer can scrub frames without replaying game rules.
fn frame(
    snapshot: ObserverSnapshot<'_>,
    capture: &mut Capture,
    wakeups: &[PlayerId],
    mut log: Vec<Value>,
) -> Value {
    for message in &snapshot.messages[capture.messages..] {
        log.push(json!({ "kind": "message", "data": message }));
    }
    for event in &snapshot.public_events[capture.public_events..] {
        log.push(json!({ "kind": "public_event", "data": event }));
    }
    capture.messages = snapshot.messages.len();
    capture.public_events = snapshot.public_events.len();

    let players: Vec<_> = snapshot.players.into_iter().map(|player| {
        let orders = capture.results.entry(player.id).or_default();
        let damage = capture.damage.entry(player.id).or_default();
        for result in &player.order_results[*orders..] {
            log.push(json!({ "kind": "order", "player": player.id, "data": result }));
        }
        for report in &player.damage[*damage..] {
            log.push(json!({ "kind": "damage", "player": player.id, "data": report }));
        }
        *orders = player.order_results.len();
        *damage = player.damage.len();
        json!({
            "id": player.id, "position": player.position, "health": player.health,
            "action_points": player.action_points, "range": player.range, "memory": player.memory,
            "vote": player.vote, "peace": player.peace, "pending_orders": player.pending_orders,
            "jury_bonus_ticks": player.jury_bonus_ticks, "wake_at": player.wake_at,
            "wake_events": player.wake_events,
        })
    }).collect();
    json!({
        "tick": snapshot.tick, "board_size": snapshot.board_size, "outcome": snapshot.outcome,
        "players": players, "rooms": snapshot.rooms, "wakeups": wakeups, "log": log,
    })
}

pub fn run(mut engine: Engine, active: &ActiveMatch, directory: &Path, players: Players) {
    let result = play(&mut engine, active, &players).and_then(|()| {
        let record = active.lock().unwrap().read_from(None, 0);
        save(directory, record).map_err(|error| format!("Could not save replay: {error}"))
    });
    let mut current = active.lock().unwrap();
    current.inference = None;
    match result {
        Ok(()) => current.status = Status::Complete,
        Err(error) => {
            current.status = Status::Failed;
            current.error = Some(error);
        }
    }
}

fn play(engine: &mut Engine, active: &ActiveMatch, players: &Players) -> Result<(), String> {
    let mut capture = Capture::default();
    loop {
        let wakeups: Vec<_> = engine
            .pending_calls()
            .map(|call| call.observation.player)
            .collect();
        if !wakeups.is_empty() && matches!(players, Players::Ollama { .. }) {
            // Publish the resolved board while the batch waits for model replies.
            let current = frame(
                engine.observer_snapshot(),
                &mut capture,
                &wakeups,
                Vec::new(),
            );
            active.lock().unwrap().frames.push(current);
        }
        let log = collect_replies(engine, active, players, &wakeups)?;
        let current = frame(engine.observer_snapshot(), &mut capture, &wakeups, log);
        {
            let mut active = active.lock().unwrap();
            active.frames.push(current);
            active.inference = None;
        }
        if engine.outcome().is_some() {
            return Ok(());
        }
        // Slow state changes for live viewing without changing the simulated tick order.
        if matches!(players, Players::Demo) {
            thread::sleep(Duration::from_millis(120));
        }
        engine
            .advance_to_next_event()
            .map_err(|error| error.to_string())?;
    }
}

fn collect_replies(
    engine: &mut Engine,
    active: &ActiveMatch,
    players: &Players,
    wakeups: &[PlayerId],
) -> Result<Vec<Value>, String> {
    let tick = engine.tick();
    let mut log = Vec::new();
    for &player in wakeups {
        loop {
            let call = engine
                .pending_calls()
                .find(|call| call.observation.player == player)
                .expect("awaiting player reply");
            let model = players.model(player);
            if model.is_some() {
                active.lock().unwrap().inference = Some(Inference {
                    player,
                    tick,
                    retry: call.validation_error.is_some(),
                });
            }
            let reply = players.reply(call).map_err(|error| {
                format!(
                    "Tank {player} at tick {tick} ({}): {error}",
                    model.unwrap_or("demo")
                )
            })?;
            let outcome = engine
                .submit_reply(player, &reply)
                .map_err(|error| error.to_string())?;
            let (status, error) = match outcome {
                ReplyOutcome::Accepted => break,
                ReplyOutcome::Retry { error } => ("retry", error),
                ReplyOutcome::Passed { error } => ("passed", error),
            };
            log.push(json!({
                "kind": "reply", "player": player,
                "data": { "tick": tick, "status": status, "error": error },
            }));
            if status == "passed" {
                break;
            }
        }
    }
    Ok(log)
}

fn save(directory: &Path, mut record: Value) -> io::Result<()> {
    let id = record["info"]["id"].as_str().expect("replay ID");
    fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".{id}.tmp"));
    let destination = directory.join(format!("{id}.json"));
    record["status"] = json!(Status::Complete);
    record["format"] = json!(1);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, &record)?;
        file.sync_all()?;
        fs::rename(&temporary, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Archive filenames encode time so listing requires no replay deserialization.
pub fn archive(directory: &Path) -> io::Result<Vec<(String, u64)>> {
    let entries = match directory.read_dir() {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) {
            if let Some(time) = replay_time(id) {
                files.push((id.to_owned(), time));
            }
        }
    }
    files.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    Ok(files)
}

pub fn replay_time(id: &str) -> Option<u64> {
    let (time, suffix) = id.split_once('-')?;
    suffix.parse::<u64>().ok()?;
    time.parse().ok()
}
