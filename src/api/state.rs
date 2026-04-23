use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

use crate::config::Config;
use crate::orchestrator::{OrchestrationSnapshot, Orchestrator, Plan};
use crate::parser;
use crate::prompt_runner::{
    ConvSnapshot, ConvSummary, ConversationManager, CreateConversationRequest, SendMessageRequest,
};
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
    pub sessions: Vec<SessionSummary>,
    pub handoffs: Vec<HandoffEntry>,
    pub config: Config,
    pub config_overrides: HashMap<String, bool>,
    pub last_refresh: Instant,
    pub orchestrator: Option<OrchestrationHandle>,
    pub conversations: ConversationManager,
}

pub struct OrchestrationHandle {
    pub orchestrator: Arc<std::sync::Mutex<Orchestrator>>,
    pub join_handle: tokio::task::JoinHandle<()>,
}

/// Lightweight session data for list views and WebSocket pushes.
/// Excludes messages, tool_uses, context_history to keep payload small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
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
    pub cwd: String,
    pub version: String,
    pub compaction_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read: u64,
    pub total_cache_create: u64,
    pub current_context_tokens: u64,
    // From SessionStatus (statusline)
    pub lines_added: u64,
    pub lines_removed: u64,
    pub context_window_size: u64,
    pub five_hour_used_pct: Option<f64>,
    pub five_hour_resets_at: Option<u64>,
    pub seven_day_used_pct: Option<f64>,
    pub seven_day_resets_at: Option<u64>,
    pub duration_ms: u64,
    // Counts only (detail fetched on demand)
    pub tool_use_count: usize,
    pub user_message_count: usize,
    pub assistant_message_count: usize,
}

/// Full session data including messages, tool_uses, context_history.
/// Returned only by the single-session detail endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    #[serde(flatten)]
    pub summary: SessionSummary,
    pub tool_uses: Vec<ToolUseSnapshot>,
    pub files_touched_paths: Vec<String>,
    pub user_messages: Vec<MessageSnapshot>,
    pub assistant_messages: Vec<MessageSnapshot>,
    pub context_history: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseSnapshot {
    pub name: String,
    pub input_summary: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSnapshot {
    pub content: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigToggleRequest {
    pub key: String,
    pub value: bool,
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
    OrchestrationUpdated,
    PromptUpdated,
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
                config_overrides: HashMap::new(),
                last_refresh: Instant::now(),
                orchestrator: None,
                conversations: ConversationManager::new(),
            })),
            event_tx,
        }
    }

    /// Refresh sessions and handoffs by calling sync code in spawn_blocking.
    /// Only discovers sessions modified within the last 24 hours.
    pub async fn refresh(&self) {
        const MAX_AGE_SECS: u64 = 86400; // 24 hours

        let result = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(Vec<SessionSummary>, Vec<HandoffEntry>)> {
                let sessions_info =
                    session::discover_sessions_since(MAX_AGE_SECS).unwrap_or_default();
                let pmap = session::ProcessMap::discover();
                let statuses = statusline::read_all();

                let summaries: Vec<SessionSummary> = sessions_info
                    .iter()
                    .filter_map(|info| {
                        let parsed = parser::parse_session(info).ok()?;
                        let status = statuses.get(&info.session_id);

                        let context_pct = status.and_then(|s| s.context_used_pct).unwrap_or(0.0);
                        let cost_usd = status.map(|s| s.cost_usd).unwrap_or(0.0);

                        let process_dead = if parsed.age_secs < 300 {
                            !pmap.is_session_alive(&info.jsonl_path)
                        } else {
                            false
                        };
                        let cpu_active = pmap.is_cpu_active(&info.jsonl_path);
                        let state =
                            session::classify_state(parsed.age_secs, process_dead, cpu_active);
                        let state_str = match state {
                            session::SessionState::Starting => "starting",
                            session::SessionState::Working => "working",
                            session::SessionState::Waiting => "waiting",
                            session::SessionState::Ended => "ended",
                            session::SessionState::Idle => "idle",
                        }
                        .to_string();

                        Some(SessionSummary {
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
                            cwd: parsed.cwd.clone(),
                            version: parsed.version.clone(),
                            compaction_count: parsed.compaction_count,
                            total_input_tokens: parsed.total_input_tokens,
                            total_output_tokens: parsed.total_output_tokens,
                            total_cache_read: parsed.total_cache_read,
                            total_cache_create: parsed.total_cache_create,
                            current_context_tokens: parsed.current_context_tokens,
                            lines_added: status.map(|s| s.lines_added).unwrap_or(0),
                            lines_removed: status.map(|s| s.lines_removed).unwrap_or(0),
                            context_window_size: status
                                .and_then(|s| s.context_window_size)
                                .unwrap_or(0),
                            five_hour_used_pct: status.and_then(|s| s.five_hour_used_pct),
                            five_hour_resets_at: status.and_then(|s| s.five_hour_resets_at),
                            seven_day_used_pct: status.and_then(|s| s.seven_day_used_pct),
                            seven_day_resets_at: status.and_then(|s| s.seven_day_resets_at),
                            duration_ms: status.map(|s| s.duration_ms).unwrap_or(0),
                            tool_use_count: parsed.tool_uses.len(),
                            user_message_count: parsed.user_messages.len(),
                            assistant_message_count: parsed.assistant_messages.len(),
                        })
                    })
                    .collect();

                let handoffs = collect_handoffs().unwrap_or_default();

                Ok((summaries, handoffs))
            },
        )
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

    pub async fn sessions(&self) -> Vec<SessionSummary> {
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

    pub async fn set_config_override(&self, key: String, value: bool) {
        // Load existing file overrides, merge our change, then save back
        let mut overrides = crate::config::load_overrides();
        overrides.insert(key.clone(), value);
        crate::config::save_overrides(&overrides);
        // Also keep in-memory copy (fallback if file I/O fails, e.g. in tests)
        let mut inner = self.inner.write().await;
        inner.config_overrides.insert(key, value);
    }

    pub async fn config_overrides(&self) -> HashMap<String, bool> {
        // Merge: file values (picks up TUI changes) + in-memory (wins on conflict)
        let mut merged = crate::config::load_overrides();
        let inner = self.inner.read().await;
        for (k, v) in &inner.config_overrides {
            merged.insert(k.clone(), *v);
        }
        merged
    }

    /// Start an orchestration from a parsed Plan. Returns Err if one is already running.
    pub async fn start_orchestration(
        &self,
        plan: Plan,
        project_root: PathBuf,
    ) -> Result<(), String> {
        let mut inner = self.inner.write().await;

        // Reject if an orchestration is already active
        if let Some(handle) = &inner.orchestrator {
            if !handle.join_handle.is_finished() {
                return Err("an orchestration is already running".to_string());
            }
        }

        let mut orchestrator = Orchestrator::new(plan, project_root);
        orchestrator.setup().map_err(|e| e.to_string())?;

        let orch = Arc::new(std::sync::Mutex::new(orchestrator));
        let orch_tick = Arc::clone(&orch);
        let event_tx = self.event_tx.clone();

        let join_handle = tokio::spawn(async move {
            loop {
                let done = {
                    let orch_ref = Arc::clone(&orch_tick);
                    match tokio::task::spawn_blocking(move || {
                        let mut o = orch_ref
                            .lock()
                            .map_err(|e| {
                                eprintln!("[relay] orchestrator lock poisoned: {e}");
                            })
                            .ok()?;
                        Some(o.tick())
                    })
                    .await
                    {
                        Ok(Some(done)) => done,
                        Ok(None) => {
                            // Lock poisoned — skip this tick, retry next
                            eprintln!("[relay] orchestrator tick skipped (lock error)");
                            false
                        }
                        Err(e) => {
                            eprintln!("[relay] orchestrator tick panicked: {e}");
                            true // stop the loop on panic
                        }
                    }
                };

                let _ = event_tx.send(Event::OrchestrationUpdated);

                if done {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }

            // Save history + cleanup worktree after loop ends
            {
                let orch_ref = Arc::clone(&orch_tick);
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(o) = orch_ref.lock() {
                        let snapshot = o.snapshot();
                        if let Err(e) = crate::orchestrator::save_plan_history(
                            &snapshot,
                            &o.plan,
                            &o.project_root,
                        ) {
                            eprintln!("[relay] failed to save plan history: {e}");
                        }
                        o.cleanup_worktree();
                    }
                })
                .await;
            }
        });

        inner.orchestrator = Some(OrchestrationHandle {
            orchestrator: orch,
            join_handle,
        });

        Ok(())
    }

    /// Get a snapshot of the current orchestration, if any.
    pub async fn orchestration_snapshot(&self) -> Option<OrchestrationSnapshot> {
        let inner = self.inner.read().await;
        let handle = inner.orchestrator.as_ref()?;
        let orch = handle.orchestrator.lock().ok()?;
        Some(orch.snapshot())
    }

    /// Abort the running orchestration.
    pub async fn abort_orchestration(&self) {
        let inner = self.inner.read().await;
        if let Some(handle) = &inner.orchestrator {
            if let Ok(mut orch) = handle.orchestrator.lock() {
                orch.abort();
            }
        }
    }

    /// Merge the orchestration branch.
    pub async fn merge_orchestration(&self) -> Result<String, String> {
        let inner = self.inner.read().await;
        let handle = inner.orchestrator.as_ref().ok_or("no orchestration")?;
        let orch = handle.orchestrator.lock().map_err(|e| e.to_string())?;
        orch.merge_branch()
    }

    /// Create a PR for the orchestration branch.
    pub async fn pr_orchestration(&self) -> Result<String, String> {
        let inner = self.inner.read().await;
        let handle = inner.orchestrator.as_ref().ok_or("no orchestration")?;
        let orch = handle.orchestrator.lock().map_err(|e| e.to_string())?;
        orch.create_pull_request()
    }

    // ── Conversations ───────────────────────────────────────────────────

    pub async fn create_conversation(
        &self,
        req: CreateConversationRequest,
    ) -> Result<String, String> {
        let mut inner = self.inner.write().await;
        let id = inner.conversations.create(req)?;
        let _ = self.event_tx.send(Event::PromptUpdated);
        Ok(id)
    }

    pub async fn send_message(&self, conv_id: &str, req: SendMessageRequest) -> Result<(), String> {
        let mut inner = self.inner.write().await;
        inner.conversations.send_message(conv_id, req)?;
        let _ = self.event_tx.send(Event::PromptUpdated);
        Ok(())
    }

    pub async fn poll_conversations(&self) {
        let mut inner = self.inner.write().await;
        if inner.conversations.poll() {
            let _ = self.event_tx.send(Event::PromptUpdated);
        }
    }

    pub async fn list_conversations(&self) -> Vec<ConvSummary> {
        let inner = self.inner.read().await;
        inner.conversations.list()
    }

    pub async fn conversation_snapshot(&self, id: &str) -> Option<ConvSnapshot> {
        let inner = self.inner.read().await;
        inner.conversations.snapshot(id)
    }

    pub async fn conversation_stream(&self, id: &str, offset: usize) -> Option<Vec<String>> {
        let inner = self.inner.read().await;
        inner.conversations.streaming_output(id, offset)
    }

    pub async fn abort_conversation(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.write().await;
        inner.conversations.abort(id)?;
        let _ = self.event_tx.send(Event::PromptUpdated);
        Ok(())
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
