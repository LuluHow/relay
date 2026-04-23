use std::sync::OnceLock;
use std::time::Instant;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::state::{
    AppState, ConfigToggleRequest, HandoffEntry, MessageSnapshot, SessionSnapshot, SessionSummary,
    ToolUseSnapshot,
};
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
    workspaces: Vec<String>,
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

#[derive(Serialize)]
pub struct ProjectResponse {
    pub project_path: String,
    pub project_name: String,
    pub session_count: usize,
}

pub async fn list_projects(State(state): State<AppState>) -> Json<Vec<ProjectResponse>> {
    let config = state.config().await;

    if config.workspaces.is_empty() {
        // Fallback: discover from Claude Code's internal project data
        let projects = session::discover_projects().unwrap_or_default();
        return Json(
            projects
                .into_iter()
                .map(|p| ProjectResponse {
                    project_path: p.project_path,
                    project_name: p.project_name,
                    session_count: p.session_count,
                })
                .collect(),
        );
    }

    // Scan configured workspace directories for real project folders
    let mut results = Vec::new();
    for ws in &config.workspaces {
        let ws_path = std::path::Path::new(ws);
        if !ws_path.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(ws_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Skip hidden directories and worktree dirs
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "worktrees" {
                continue;
            }
            results.push(ProjectResponse {
                project_path: path.to_string_lossy().to_string(),
                project_name: name,
                session_count: 0,
            });
        }
    }
    results.sort_by(|a, b| a.project_name.cmp(&b.project_name));
    Json(results)
}

pub async fn list_sessions(
    State(state): State<AppState>,
    Query(params): Query<SessionsQuery>,
) -> Json<Vec<SessionSummary>> {
    let mut sessions = state.sessions().await;
    if params.active == Some(true) {
        sessions.retain(|s| matches!(s.state.as_str(), "starting" | "working" | "waiting"));
    }
    Json(sessions)
}

pub async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    // Check that the session exists in our cached summaries
    let sessions = state.sessions().await;
    let summary = match sessions.into_iter().find(|s| s.session_id == id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "session not found",
                }),
            )
                .into_response()
        }
    };

    // Parse the full session JSONL on demand for detail view
    let id_clone = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let sessions_info = session::discover_sessions_since(86400).unwrap_or_default();
        let info = sessions_info
            .into_iter()
            .find(|s| s.session_id == id_clone)?;
        parser::parse_session(&info).ok()
    })
    .await;

    match result {
        Ok(Some(parsed)) => {
            let tool_uses = parsed
                .tool_uses
                .iter()
                .map(|t| ToolUseSnapshot {
                    name: t.name.clone(),
                    input_summary: t.input_summary.clone(),
                    timestamp: t.timestamp.clone(),
                })
                .collect();
            let user_messages = parsed
                .user_messages
                .iter()
                .map(|m| MessageSnapshot {
                    content: m.content.clone(),
                    timestamp: m.timestamp.clone(),
                })
                .collect();
            let assistant_messages = parsed
                .assistant_messages
                .iter()
                .map(|m| MessageSnapshot {
                    content: m.content.clone(),
                    timestamp: m.timestamp.clone(),
                })
                .collect();

            let snapshot = SessionSnapshot {
                summary,
                tool_uses,
                files_touched_paths: parsed.files_touched,
                user_messages,
                assistant_messages,
                context_history: parsed.context_history,
            };
            Json(snapshot).into_response()
        }
        _ => {
            // Fall back to returning the summary if parsing fails
            Json(summary).into_response()
        }
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
        workspaces: c.workspaces,
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
            Ok(plan) => {
                if let Err(e) = orchestrator::validate_plan(&plan) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(DynErrorResponse {
                            error: format!("invalid plan: {e}"),
                        }),
                    )
                        .into_response();
                }
                plan
            }
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
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });

    let plan_name = plan.plan.name.clone();

    match state.start_orchestration(plan, project_root).await {
        Ok(()) => Json(serde_json::json!({
            "status": "started",
            "plan_name": plan_name,
        }))
        .into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(DynErrorResponse { error: e })).into_response(),
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
        Err(e) => (StatusCode::BAD_REQUEST, Json(DynErrorResponse { error: e })).into_response(),
    }
}

pub async fn create_orchestration_pr(State(state): State<AppState>) -> Response {
    match state.pr_orchestration().await {
        Ok(url) => Json(serde_json::json!({ "status": "created", "url": url })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(DynErrorResponse { error: e })).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Plan history handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PlanHistoryResponse {
    pub id: String,
    pub plan_name: String,
    pub state: String,
    pub task_count: usize,
    pub done_count: usize,
    pub failed_count: usize,
    pub elapsed_secs: u64,
    pub completed_at: String,
}

pub async fn list_plan_history() -> Json<Vec<PlanHistoryResponse>> {
    let entries = orchestrator::list_plan_history().unwrap_or_default();
    Json(
        entries
            .into_iter()
            .map(|e| PlanHistoryResponse {
                id: e.id,
                plan_name: e.plan_name,
                state: e.state,
                task_count: e.task_count,
                done_count: e.done_count,
                failed_count: e.failed_count,
                elapsed_secs: e.elapsed_secs,
                completed_at: e.completed_at,
            })
            .collect(),
    )
}

pub async fn get_plan_history(Path(id): Path<String>) -> Response {
    match orchestrator::get_plan_history(&id) {
        Ok(Some(entry)) => Json(entry).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "plan not found",
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(DynErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}
