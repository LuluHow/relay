use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

const MAX_OUTPUT_LINES: usize = 10_000;

// ── Request types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CreateConversationRequest {
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_skip_permissions")]
    pub skip_permissions: bool,
    /// When set, resume an existing Claude Code session instead of starting fresh.
    #[serde(default)]
    pub resume_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub allowed_tools: Option<String>,
}

fn default_skip_permissions() -> bool {
    true
}

// ── Conversation state ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvStatus {
    /// Waiting for user input
    Idle,
    /// Claude is generating a response
    Responding,
    /// Conversation ended (error or explicit close)
    Ended,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String, // "user" or "assistant"
    pub content: String,
    pub timestamp_secs: u64, // seconds since conversation start
}

/// A multi-turn conversation backed by a Claude Code session.
pub struct Conversation {
    pub id: String,
    /// Claude Code session UUID — used with --session-id / --resume
    pub session_id: String,
    pub project_path: String,
    pub model: Option<String>,
    pub skip_permissions: bool,
    pub status: ConvStatus,
    pub messages: Vec<ChatMessage>,
    pub created_at: Instant,
    /// Active response (while Claude is running)
    active_child: Option<Child>,
    active_buffer: Arc<Mutex<Vec<String>>>,
    turn_count: u32,
}

/// Serializable snapshot for API list views.
#[derive(Debug, Clone, Serialize)]
pub struct ConvSummary {
    pub id: String,
    pub project_path: String,
    pub model: Option<String>,
    pub status: String,
    pub message_count: usize,
    pub last_message_preview: String,
    pub elapsed_secs: u64,
}

/// Full conversation snapshot for detail views.
#[derive(Debug, Clone, Serialize)]
pub struct ConvSnapshot {
    pub id: String,
    pub session_id: String,
    pub project_path: String,
    pub model: Option<String>,
    pub status: String,
    pub messages: Vec<ChatMessage>,
    pub elapsed_secs: u64,
    /// Current streaming response lines (empty if idle)
    pub streaming_output: Vec<String>,
}

// ── ConversationManager ───────────────────────────────────────────────────

pub struct ConversationManager {
    conversations: Vec<Conversation>,
    next_id: u32,
}

impl Default for ConversationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationManager {
    pub fn new() -> Self {
        Self {
            conversations: Vec::new(),
            next_id: 1,
        }
    }

    /// Create a new conversation. Returns the conversation ID.
    pub fn create(&mut self, req: CreateConversationRequest) -> Result<String, String> {
        let id = format!("conv-{}", self.next_id);
        self.next_id += 1;

        let cwd = req.project_path.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

        let cwd_path = std::path::Path::new(&cwd);
        if !cwd_path.is_dir() {
            return Err(format!("directory does not exist: {cwd}"));
        }

        let (session_id, turn_count) = match req.resume_session_id {
            Some(sid) => (sid, 1), // start at turn 1 so --resume is used
            None => (uuid_v4(), 0),
        };

        self.conversations.push(Conversation {
            id: id.clone(),
            session_id,
            project_path: cwd,
            model: req.model,
            skip_permissions: req.skip_permissions,
            status: ConvStatus::Idle,
            messages: Vec::new(),
            created_at: Instant::now(),
            active_child: None,
            active_buffer: Arc::new(Mutex::new(Vec::new())),
            turn_count,
        });

        Ok(id)
    }

    /// Send a user message in a conversation. Spawns claude and starts streaming.
    pub fn send_message(&mut self, conv_id: &str, req: SendMessageRequest) -> Result<(), String> {
        let conv = self
            .conversations
            .iter_mut()
            .find(|c| c.id == conv_id)
            .ok_or_else(|| format!("conversation '{conv_id}' not found"))?;

        if conv.status == ConvStatus::Responding {
            return Err("conversation is busy — wait for the current response".to_string());
        }
        if conv.status == ConvStatus::Ended {
            return Err("conversation has ended".to_string());
        }

        if req.content.trim().is_empty() {
            return Err("message cannot be empty".to_string());
        }

        // Record user message
        conv.messages.push(ChatMessage {
            role: "user".to_string(),
            content: req.content.clone(),
            timestamp_secs: conv.created_at.elapsed().as_secs(),
        });

        // Build claude command
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(&req.content)
            .arg("--output-format")
            .arg("text");

        if conv.turn_count == 0 {
            // First turn: create session
            cmd.arg("--session-id").arg(&conv.session_id);
        } else {
            // Subsequent turns: resume session
            cmd.arg("--resume").arg(&conv.session_id);
        }

        cmd.current_dir(&conv.project_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if conv.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }
        if let Some(m) = &conv.model {
            cmd.arg("--model").arg(m);
        }
        if let Some(tools) = &req.allowed_tools {
            cmd.arg("--allowedTools").arg(tools);
        }
        if let Some(turns) = req.max_turns {
            cmd.arg("--max-turns").arg(turns.to_string());
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn claude: {e}"))?;

        // Non-blocking output capture
        let buffer = Arc::new(Mutex::new(Vec::<String>::new()));

        if let Some(stdout) = child.stdout.take() {
            let buf = Arc::clone(&buffer);
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(mut v) = buf.lock() {
                        if v.len() < MAX_OUTPUT_LINES {
                            v.push(line);
                        }
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let buf = Arc::clone(&buffer);
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(mut v) = buf.lock() {
                        if v.len() < MAX_OUTPUT_LINES {
                            v.push(format!("[stderr] {line}"));
                        }
                    }
                }
            });
        }

        conv.active_child = Some(child);
        conv.active_buffer = buffer;
        conv.status = ConvStatus::Responding;
        conv.turn_count += 1;

        Ok(())
    }

    /// Poll all conversations for completed responses. Returns true if any changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;

        for conv in &mut self.conversations {
            if conv.status != ConvStatus::Responding {
                continue;
            }
            if let Some(child) = &mut conv.active_child {
                match child.try_wait() {
                    Ok(Some(exit_status)) => {
                        let code = exit_status.code().unwrap_or(-1);
                        // Collect the response text
                        let response_text = conv
                            .active_buffer
                            .lock()
                            .map(|v| {
                                v.iter()
                                    .filter(|l| !l.starts_with("[stderr] "))
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();

                        if !response_text.is_empty() || code == 0 {
                            conv.messages.push(ChatMessage {
                                role: "assistant".to_string(),
                                content: response_text,
                                timestamp_secs: conv.created_at.elapsed().as_secs(),
                            });
                        }

                        // Clear active state
                        conv.active_child = None;
                        conv.active_buffer = Arc::new(Mutex::new(Vec::new()));
                        conv.status = if code == 0 {
                            ConvStatus::Idle
                        } else {
                            // On error, still allow retrying
                            ConvStatus::Idle
                        };
                        changed = true;
                    }
                    Ok(None) => {} // still running
                    Err(_) => {
                        conv.active_child = None;
                        conv.active_buffer = Arc::new(Mutex::new(Vec::new()));
                        conv.status = ConvStatus::Ended;
                        changed = true;
                    }
                }
            }
        }

        changed
    }

    /// Abort the current response in a conversation.
    pub fn abort(&mut self, conv_id: &str) -> Result<(), String> {
        let conv = self
            .conversations
            .iter_mut()
            .find(|c| c.id == conv_id)
            .ok_or_else(|| format!("conversation '{conv_id}' not found"))?;

        if conv.status != ConvStatus::Responding {
            return Err("conversation is not responding".to_string());
        }

        if let Some(child) = &mut conv.active_child {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Save partial response if any
        let partial = conv
            .active_buffer
            .lock()
            .map(|v| {
                v.iter()
                    .filter(|l| !l.starts_with("[stderr] "))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        if !partial.is_empty() {
            conv.messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: format!("{partial}\n\n[aborted]"),
                timestamp_secs: conv.created_at.elapsed().as_secs(),
            });
        }

        conv.active_child = None;
        conv.active_buffer = Arc::new(Mutex::new(Vec::new()));
        conv.status = ConvStatus::Idle;
        Ok(())
    }

    /// Get streaming output lines for the current response.
    pub fn streaming_output(&self, conv_id: &str, offset: usize) -> Option<Vec<String>> {
        let conv = self.conversations.iter().find(|c| c.id == conv_id)?;
        Some(
            conv.active_buffer
                .lock()
                .map(|v| {
                    if offset >= v.len() {
                        Vec::new()
                    } else {
                        v[offset..].to_vec()
                    }
                })
                .unwrap_or_default(),
        )
    }

    /// Get full snapshot of a conversation.
    pub fn snapshot(&self, conv_id: &str) -> Option<ConvSnapshot> {
        let conv = self.conversations.iter().find(|c| c.id == conv_id)?;
        let streaming_output = conv
            .active_buffer
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();
        Some(ConvSnapshot {
            id: conv.id.clone(),
            session_id: conv.session_id.clone(),
            project_path: conv.project_path.clone(),
            model: conv.model.clone(),
            status: status_label(conv.status),
            messages: conv.messages.clone(),
            elapsed_secs: conv.created_at.elapsed().as_secs(),
            streaming_output,
        })
    }

    /// List all conversations.
    pub fn list(&self) -> Vec<ConvSummary> {
        self.conversations
            .iter()
            .rev()
            .map(|c| {
                let last = c
                    .messages
                    .last()
                    .map(|m| truncate(&m.content, 100))
                    .unwrap_or_default();
                ConvSummary {
                    id: c.id.clone(),
                    project_path: c.project_path.clone(),
                    model: c.model.clone(),
                    status: status_label(c.status),
                    message_count: c.messages.len(),
                    last_message_preview: last,
                    elapsed_secs: c.created_at.elapsed().as_secs(),
                }
            })
            .collect()
    }
}

fn status_label(s: ConvStatus) -> String {
    match s {
        ConvStatus::Idle => "idle",
        ConvStatus::Responding => "responding",
        ConvStatus::Ended => "ended",
    }
    .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() <= max {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..max])
    }
}

/// Generate a v4-style UUID without pulling in the uuid crate.
fn uuid_v4() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut hasher = DefaultHasher::new();
    nanos.hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    let h1 = hasher.finish();

    // Second hash with different seed for more bits
    let mut hasher2 = DefaultHasher::new();
    (nanos.wrapping_add(h1 as u128)).hash(&mut hasher2);
    let h2 = hasher2.finish();

    let bits = ((h1 as u128) << 64) | (h2 as u128);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (bits >> 96) as u32,
        (bits >> 80) as u16,
        (bits >> 64) as u16 & 0x0FFF,
        ((bits >> 48) as u16 & 0x3FFF) | 0x8000,
        bits as u64 & 0xFFFF_FFFF_FFFF,
    )
}
