use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{broadcast, RwLock};

use crate::config::Config;
use crate::parser;
use crate::session;
use crate::statusline;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<AppStateInner>>,
    event_tx: broadcast::Sender<Event>,
}

pub struct AppStateInner {
    pub sessions: Vec<SessionSnapshot>,
    pub handoffs: Vec<HandoffEntry>,
    pub config: Config,
    pub last_refresh: Instant,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub project_name: String,
    pub model: String,
    pub git_branch: String,
    pub state: String,
    pub turn_count: u32,
    pub context_pct: f64,
    pub cost_usd: f64,
    pub age_secs: u64,
    pub files_touched: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct HandoffEntry {
    pub id: String,
    pub preview: String,
    pub size_bytes: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    SessionsUpdated,
    HandoffCreated { id: String },
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl AppState {
    pub fn new(config: Config) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RwLock::new(AppStateInner {
                sessions: Vec::new(),
                handoffs: Vec::new(),
                config,
                last_refresh: Instant::now(),
            })),
            event_tx,
        }
    }

    /// Refresh sessions and handoffs by calling sync code in spawn_blocking.
    pub async fn refresh(&self) {
        let result = tokio::task::spawn_blocking(|| -> anyhow::Result<(Vec<SessionSnapshot>, Vec<HandoffEntry>)> {
            let sessions_info = session::discover_sessions().unwrap_or_default();
            let statuses = statusline::read_all();

            let snapshots: Vec<SessionSnapshot> = sessions_info
                .iter()
                .filter_map(|info| {
                    let parsed = parser::parse_session(info).ok()?;
                    let status = statuses.get(&info.session_id);

                    let context_pct = status
                        .and_then(|s| s.context_used_pct)
                        .unwrap_or(0.0);

                    let cost_usd = status.map(|s| s.cost_usd).unwrap_or(0.0);

                    let state = session::classify_state(
                        parsed.age_secs,
                        false, // cannot cheaply detect dead processes here
                    );
                    let state_str = match state {
                        session::SessionState::Starting => "starting",
                        session::SessionState::Working => "working",
                        session::SessionState::Waiting => "waiting",
                        session::SessionState::Ended => "ended",
                        session::SessionState::Idle => "idle",
                    }
                    .to_string();

                    Some(SessionSnapshot {
                        session_id: info.session_id.clone(),
                        project_name: info.project_name.clone(),
                        model: parsed.model.clone(),
                        git_branch: parsed.git_branch.clone(),
                        state: state_str,
                        turn_count: parsed.turn_count,
                        context_pct,
                        cost_usd,
                        age_secs: parsed.age_secs,
                        files_touched: parsed.files_touched.len(),
                    })
                })
                .collect();

            let handoffs = collect_handoffs().unwrap_or_default();

            Ok((snapshots, handoffs))
        })
        .await;

        match result {
            Ok(Ok((sessions, handoffs))) => {
                let mut inner = self.inner.write().await;
                inner.sessions = sessions;
                inner.handoffs = handoffs;
                inner.last_refresh = Instant::now();
                let _ = self.event_tx.send(Event::SessionsUpdated);
            }
            Ok(Err(e)) => {
                let _ = self.event_tx.send(Event::Error {
                    message: e.to_string(),
                });
            }
            Err(e) => {
                let _ = self.event_tx.send(Event::Error {
                    message: format!("spawn_blocking join error: {e}"),
                });
            }
        }
    }

    pub async fn sessions(&self) -> Vec<SessionSnapshot> {
        self.inner.read().await.sessions.clone()
    }

    pub async fn handoffs(&self) -> Vec<HandoffEntry> {
        self.inner.read().await.handoffs.clone()
    }

    pub async fn config(&self) -> Config {
        self.inner.read().await.config.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    pub fn notify_handoff_created(&self, id: String) {
        let _ = self.event_tx.send(Event::HandoffCreated { id });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read handoff files from ~/.relay/handoffs/ and return structured entries.
fn collect_handoffs() -> anyhow::Result<Vec<HandoffEntry>> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("No home directory"))?
        .join(".relay")
        .join("handoffs");

    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<HandoffEntry> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let id = name.trim_end_matches(".md").to_string();
            let meta = entry.metadata().ok()?;
            let size_bytes = meta.len();
            let created_at = meta
                .modified()
                .ok()
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.to_rfc3339()
                })
                .unwrap_or_default();

            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            let preview = content
                .lines()
                .find(|l| l.starts_with("**Context:**"))
                .unwrap_or("")
                .to_string();

            Some(HandoffEntry {
                id,
                preview,
                size_bytes,
                created_at,
            })
        })
        .collect();

    entries.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(entries)
}
