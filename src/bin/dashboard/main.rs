//! Loopback-only browser dashboard for the Rust match engine.

mod demo;
mod matches;
mod ollama;

use std::{
    env, fs, io,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use ai_turn_tactics::{Engine, MatchConfig};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use matches::{ActiveMatch, MatchRun, Players, ReplayInfo, Status};
use ollama::Ollama;

#[derive(Clone)]
struct Dashboard {
    active: Arc<Mutex<Option<ActiveMatch>>>,
    next_id: Arc<AtomicU64>,
    directory: PathBuf,
    ollama: Ollama,
}

type ApiError = (StatusCode, Json<Value>);
type ApiResult = Result<Json<Value>, ApiError>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = match env::var("AI_TACTICS_PORT") {
        Ok(value) => value.parse()?,
        Err(env::VarError::NotPresent) => 8080,
        Err(error) => return Err(error.into()),
    };
    let directory = env::var_os("AI_TACTICS_REPLAY_DIR")
        .map_or_else(|| PathBuf::from("replays"), PathBuf::from);
    let ollama = Ollama::new(
        &env::var("AI_TACTICS_OLLAMA_URL").unwrap_or_else(|_| ollama::DEFAULT_URL.into()),
    )?;
    let dashboard = Dashboard {
        active: Arc::new(Mutex::new(None)),
        next_id: Arc::new(AtomicU64::new(0)),
        directory,
        ollama,
    };
    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../../../web/index.html")) }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../../../web/style.css"),
                )
            }),
        )
        .route(
            "/dashboard.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../../../web/dashboard.js"),
                )
            }),
        )
        .route(
            "/api/config",
            get(|| async { Json(MatchConfig::default()) }),
        )
        .route("/api/live", get(live).post(launch))
        .route("/api/models", get(models))
        .route("/api/replays", get(archive))
        .route("/api/replays/{id}", get(replay))
        .layer(middleware::from_fn(local_host))
        .with_state(dashboard);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    println!("AI Turn Tactics · http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn local_host(request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    let allowed = host
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
        .is_some_and(|host| matches!(host.host(), "127.0.0.1" | "localhost"));
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> ApiError {
    (status, Json(json!({ "error": message.to_string() })))
}

#[derive(Deserialize)]
struct Cursor {
    id: Option<String>,
    #[serde(default)]
    after: usize,
}

async fn live(State(state): State<Dashboard>, Query(cursor): Query<Cursor>) -> Json<Value> {
    let active = state.active.lock().unwrap().clone();
    Json(active.map_or(Value::Null, |run| {
        run.lock()
            .unwrap()
            .read_from(cursor.id.as_deref(), cursor.after)
    }))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LaunchRequest {
    Settings(LaunchSettings),
    Demo(MatchConfig),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchSettings {
    config: MatchConfig,
    player_models: Option<Vec<String>>,
}

async fn installed_models(ollama: Ollama) -> Result<Vec<String>, ApiError> {
    tokio::task::spawn_blocking(move || ollama.models())
        .await
        .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?
        .map_err(|err| error(StatusCode::SERVICE_UNAVAILABLE, err))
}

async fn models(State(state): State<Dashboard>) -> ApiResult {
    Ok(Json(json!(installed_models(state.ollama).await?)))
}

async fn launch(State(state): State<Dashboard>, Json(request): Json<LaunchRequest>) -> ApiResult {
    let (config, player_models) = match request {
        LaunchRequest::Settings(request) => (request.config, request.player_models),
        LaunchRequest::Demo(config) => (config, None),
    };
    let players = if let Some(models) = &player_models {
        if models.len() != config.players.len() {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Choose a model for each player.",
            ));
        }
        let installed = installed_models(state.ollama.clone()).await?;
        if let Some(missing) = models.iter().find(|name| !installed.contains(name)) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                format!("Local Ollama model is unavailable: {missing}"),
            ));
        }
        Players::Ollama {
            server: state.ollama,
            models: models.clone(),
        }
    } else {
        Players::Demo
    };
    let mut current = state.active.lock().unwrap();
    if current
        .as_ref()
        .is_some_and(|run| run.lock().unwrap().status == Status::Running)
    {
        return Err(error(StatusCode::CONFLICT, "A match is already running."));
    }
    let engine = Engine::new(config.clone()).map_err(|err| error(StatusCode::BAD_REQUEST, err))?;
    let created_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let sequence = state.next_id.fetch_add(1, Ordering::Relaxed);
    let info = ReplayInfo {
        id: format!("{created_ms}-{sequence}"),
        created_ms,
        config,
        player_models,
    };
    let active = Arc::new(Mutex::new(MatchRun::new(info.clone())));
    let runner = active.clone();
    let directory = state.directory;
    std::thread::Builder::new()
        .name("tactics-match".into())
        .spawn(move || matches::run(engine, &runner, &directory, players))
        .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    *current = Some(active);
    Ok(Json(json!(info)))
}

async fn archive(State(state): State<Dashboard>) -> ApiResult {
    let archive = tokio::task::spawn_blocking(move || matches::archive(&state.directory))
        .await
        .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?
        .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(json!(
        archive
            .into_iter()
            .map(|(id, created_ms)| json!({ "id": id, "created_ms": created_ms }))
            .collect::<Vec<_>>()
    )))
}

async fn replay(
    State(state): State<Dashboard>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if matches::replay_time(&id).is_none() {
        return Err(error(StatusCode::NOT_FOUND, "Replay not found."));
    }
    let bytes =
        tokio::task::spawn_blocking(move || fs::read(state.directory.join(format!("{id}.json"))))
            .await
            .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => error(StatusCode::NOT_FOUND, "Replay not found."),
                _ => error(StatusCode::INTERNAL_SERVER_ERROR, err),
            })?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}
