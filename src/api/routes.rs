use std::sync::OnceLock;
use std::time::Instant;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::state::{AppState, ConfigToggleRequest, HandoffEntry, SessionSnapshot};
use crate::orchestrator;
use crate::{handoff, parser, session, storage};

static START_TIME: OnceLock<Instant> = OnceLock::new();

fn start_time() -> &'static Instant {
    START_TIME.get_or_init(Instant::now)
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
    version: &'static str,
    uptime_secs: u64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

#[derive(Serialize)]
struct DynErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct HandoffCreatedResponse {
    id: String,
    message: &'static str,
}

#[derive(Serialize)]
pub struct ConfigResponse {
    threshold: u8,
    max_turns: u32,
    interval: u64,
    cooldown: u64,
    notify: bool,
    auto_handoff: bool,
    auto_commit: bool,
    commit_before_handoff: bool,
    commit_prefix: String,
    sound: bool,
    discord_webhook: Option<String>,
    slack_webhook: Option<String>,
    api_port: u16,
    api_bind: String,
    api_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: start_time().elapsed().as_secs(),
    })
}

#[derive(Deserialize)]
pub struct SessionsQuery {
    active: Option<bool>,
}

pub async fn list_sessions(
    State(state): State<AppState>,
    Query(params): Query<SessionsQuery>,
) -> Json<Vec<SessionSnapshot>> {
    let mut sessions = state.sessions().await;
    if params.active == Some(true) {
        sessions.retain(|s| matches!(s.state.as_str(), "starting" | "working" | "waiting"));
    }
    Json(sessions)
}

pub async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let sessions = state.sessions().await;
    match sessions.into_iter().find(|s| s.session_id == id) {
        Some(s) => Json(s).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "session not found",
            }),
        )
            .into_response(),
    }
}

pub async fn create_handoff(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    // Fast 404: check cached sessions before paying for a disk scan
    let sessions = state.sessions().await;
    if !sessions.iter().any(|s| s.session_id == id) {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "session not found",
            }),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        let sessions_info = session::discover_sessions()?;
        let Some(info) = sessions_info.into_iter().find(|s| s.session_id == id) else {
            return Ok(None);
        };
        let parsed = parser::parse_session(&info)?;
        let summary = handoff::generate_summary(&parsed)?;
        let handoff_id = storage::save(&summary, &info)?;
        Ok(Some(handoff_id))
    })
    .await;

    match result {
        Ok(Ok(Some(handoff_id))) => {
            state.notify_handoff_created(handoff_id.clone());
            Json(HandoffCreatedResponse {
                id: handoff_id,
                message: "handoff saved",
            })
            .into_response()
        }
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "session not found",
            }),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(DynErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "internal error",
            }),
        )
            .into_response(),
    }
}

pub async fn list_handoffs(State(state): State<AppState>) -> Json<Vec<HandoffEntry>> {
    Json(state.handoffs().await)
}

pub async fn get_handoff(Path(id): Path<String>) -> Response {
    let path = match dirs::home_dir() {
        Some(h) => h.join(".relay").join("handoffs").join(format!("{id}.md")),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "no home directory",
                }),
            )
                .into_response()
        }
    };

    match std::fs::read_to_string(&path) {
        Ok(content) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/markdown"),
            )],
            content,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "handoff not found",
            }),
        )
            .into_response(),
    }
}

pub async fn get_config(State(state): State<AppState>) -> Json<ConfigResponse> {
    let c = state.config().await;
    let overrides = state.config_overrides().await;
    let auto_handoff = overrides
        .get("auto_handoff")
        .copied()
        .unwrap_or(c.auto_handoff);
    let auto_commit = overrides
        .get("auto_commit")
        .copied()
        .unwrap_or(c.auto_commit);
    Json(ConfigResponse {
        threshold: c.threshold,
        max_turns: c.max_turns,
        interval: c.interval,
        cooldown: c.cooldown,
        notify: c.notify,
        auto_handoff,
        auto_commit,
        commit_before_handoff: c.commit_before_handoff,
        commit_prefix: c.commit_prefix,
        sound: c.sound,
        discord_webhook: c.discord_webhook,
        slack_webhook: c.slack_webhook,
        api_port: c.api_port,
        api_bind: c.api_bind,
        api_token: c.api_token.map(|_| "***".to_string()),
    })
}

pub async fn toggle_config(
    State(state): State<AppState>,
    Json(req): Json<ConfigToggleRequest>,
) -> Response {
    const VALID_KEYS: &[&str] = &["auto_handoff", "auto_commit"];
    if !VALID_KEYS.contains(&req.key.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(DynErrorResponse {
                error: format!("invalid key '{}', valid keys: {:?}", req.key, VALID_KEYS),
            }),
        )
            .into_response();
    }
    state.set_config_override(req.key.clone(), req.value).await;
    Json(serde_json::json!({
        "key": req.key,
        "value": req.value,
        "message": "override applied"
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Orchestration handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct StartOrchestrationRequest {
    pub plan_path: Option<String>,
    pub plan_toml: Option<String>,
    pub project_root: Option<String>,
}

pub async fn start_orchestration(
    State(state): State<AppState>,
    Json(req): Json<StartOrchestrationRequest>,
) -> Response {
    // Parse plan from path or inline TOML
    let plan = if let Some(path) = &req.plan_path {
        let p = std::path::PathBuf::from(path);
        match orchestrator::load_plan(&p) {
            Ok(plan) => plan,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(DynErrorResponse {
                        error: format!("failed to load plan: {e}"),
                    }),
                )
                    .into_response()
            }
        }
    } else if let Some(toml_str) = &req.plan_toml {
        match toml::from_str::<orchestrator::Plan>(toml_str) {
            Ok(plan) => plan,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(DynErrorResponse {
                        error: format!("invalid plan TOML: {e}"),
                    }),
                )
                    .into_response()
            }
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "provide plan_path or plan_toml",
            }),
        )
            .into_response();
    };

    let project_root = req
        .project_root
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));

    let plan_name = plan.plan.name.clone();

    match state.start_orchestration(plan, project_root).await {
        Ok(()) => Json(serde_json::json!({
            "status": "started",
            "plan_name": plan_name,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(DynErrorResponse { error: e }),
        )
            .into_response(),
    }
}

pub async fn orchestration_status(State(state): State<AppState>) -> Response {
    match state.orchestration_snapshot().await {
        Some(snapshot) => Json(snapshot).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "no orchestration running",
            }),
        )
            .into_response(),
    }
}

pub async fn abort_orchestration(State(state): State<AppState>) -> Response {
    if state.orchestration_snapshot().await.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "no orchestration running",
            }),
        )
            .into_response();
    }
    state.abort_orchestration().await;
    Json(serde_json::json!({ "status": "aborted" })).into_response()
}

pub async fn merge_orchestration(State(state): State<AppState>) -> Response {
    match state.merge_orchestration().await {
        Ok(msg) => Json(serde_json::json!({ "status": msg })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(DynErrorResponse { error: e }),
        )
            .into_response(),
    }
}

pub async fn create_orchestration_pr(State(state): State<AppState>) -> Response {
    match state.pr_orchestration().await {
        Ok(url) => Json(serde_json::json!({ "status": "created", "url": url })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(DynErrorResponse { error: e }),
        )
            .into_response(),
    }
}
