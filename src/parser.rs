use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::time::SystemTime;

use crate::session::SessionInfo;

// --- JSONL schema (defensive: all fields optional with defaults) ---

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawLine {
    #[serde(rename = "type")]
    msg_type: String,
    message: Option<RawMessage>,
    uuid: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "parentUuid")]
    parent_uuid: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    // User-specific
    version: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawMessage {
    role: String,
    content: serde_json::Value,
    model: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct RawUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

// --- Parsed session output ---

#[derive(Debug, Clone)]
pub struct ParsedSession {
    pub session_id: String,
    pub project_name: String,
    pub model: String,
    pub git_branch: String,
    pub cwd: String,
    pub version: String,

    // Conversation
    pub user_messages: Vec<ConversationTurn>,
    pub assistant_messages: Vec<ConversationTurn>,
    pub tool_uses: Vec<ToolUse>,

    // Tokens
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read: u64,
    pub total_cache_create: u64,
    pub current_context_tokens: u64, // Last turn's input + cache_read
    pub context_history: Vec<u64>,   // Per-turn context sizes
    pub compaction_count: u32,       // Drops >30%

    // Timing
    pub turn_count: u32,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub age_secs: u64,

    // Files touched (from tool_use Edit/Write/Read)
    pub files_touched: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub content: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolUse {
    pub name: String,
    pub input_summary: String,
    pub timestamp: Option<String>,
}

/// Parse an entire session JSONL file
pub fn parse_session(session: &SessionInfo) -> Result<ParsedSession> {
    let file = std::fs::File::open(&session.jsonl_path)
        .context(format!("Cannot open {}", session.jsonl_path.display()))?;
    let reader = BufReader::new(file);

    let mut parsed = ParsedSession {
        session_id: session.session_id.clone(),
        project_name: session.project_name.clone(),
        model: String::new(),
        git_branch: String::new(),
        cwd: String::new(),
        version: String::new(),
        user_messages: Vec::new(),
        assistant_messages: Vec::new(),
        tool_uses: Vec::new(),
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        current_context_tokens: 0,
        context_history: Vec::new(),
        compaction_count: 0,
        turn_count: 0,
        first_timestamp: None,
        last_timestamp: None,
        age_secs: 0,
        files_touched: Vec::new(),
    };

    let mut last_context = 0u64;
    let mut seen_files = std::collections::HashSet::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let raw: RawLine = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue, // Defensive: skip malformed lines
        };

        // Track timestamps
        if let Some(ref ts) = raw.timestamp {
            if parsed.first_timestamp.is_none() {
                parsed.first_timestamp = Some(ts.clone());
            }
            parsed.last_timestamp = Some(ts.clone());
        }

        // Skip sidechains
        if raw.is_sidechain.unwrap_or(false) {
            continue;
        }

        match raw.msg_type.as_str() {
            "user" => {
                if let Some(ref msg) = raw.message {
                    // Extract metadata from user messages
                    if let Some(ref v) = raw.version {
                        parsed.version = v.clone();
                    }
                    if let Some(ref b) = raw.git_branch {
                        parsed.git_branch = b.clone();
                    }
                    if let Some(ref c) = raw.cwd {
                        parsed.cwd = c.clone();
                    }

                    let content = extract_text_content(&msg.content);
                    // Only track real user messages (not tool results)
                    if msg.role == "user" && !content.is_empty() && raw.parent_uuid.is_none()
                        || (raw.parent_uuid.is_some()
                            && content.len() > 5
                            && !content.starts_with('['))
                    {
                        // Heuristic: real user prompts are typically longer and at root
                        if content.len() > 2 {
                            parsed.user_messages.push(ConversationTurn {
                                content,
                                timestamp: raw.timestamp.clone(),
                            });
                        }
                    }
                }
            }
            "assistant" => {
                if let Some(ref msg) = raw.message {
                    // Model
                    if let Some(ref m) = msg.model {
                        if m.starts_with("claude") {
                            parsed.model = m.clone();
                        }
                    }

                    // Usage
                    if let Some(ref u) = msg.usage {
                        parsed.total_input_tokens += u.input_tokens;
                        parsed.total_output_tokens += u.output_tokens;
                        parsed.total_cache_read += u.cache_read_input_tokens;
                        parsed.total_cache_create += u.cache_creation_input_tokens;

                        // Current context = input + cache_read (exclude cache_create to avoid double-count)
                        let ctx = u.input_tokens + u.cache_read_input_tokens;
                        if ctx > 0 {
                            parsed.current_context_tokens = ctx;
                            parsed.context_history.push(ctx);

                            // Compaction detection: >30% drop
                            if last_context > 0 && ctx < last_context * 70 / 100 {
                                parsed.compaction_count += 1;
                            }
                            last_context = ctx;
                        }

                        if u.output_tokens > 0 {
                            parsed.turn_count += 1;
                        }
                    }

                    // Extract text + tool_use
                    let text = extract_text_content(&msg.content);
                    if !text.is_empty() {
                        parsed.assistant_messages.push(ConversationTurn {
                            content: text,
                            timestamp: raw.timestamp.clone(),
                        });
                    }

                    // Tool uses
                    extract_tool_uses(&msg.content, &raw.timestamp, &mut parsed.tool_uses, &mut seen_files);
                }
            }
            _ => {}
        }
    }

    // Compute age
    let now = SystemTime::now();
    parsed.age_secs = now
        .duration_since(session.modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    parsed.files_touched = seen_files.into_iter().collect();
    parsed.files_touched.sort();

    Ok(parsed)
}

fn extract_text_content(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("type") {
                    if t == "text" {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn extract_tool_uses(
    content: &serde_json::Value,
    timestamp: &Option<String>,
    tool_uses: &mut Vec<ToolUse>,
    seen_files: &mut std::collections::HashSet<String>,
) {
    if let serde_json::Value::Array(arr) = content {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let name = item
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let input = item.get("input").cloned().unwrap_or_default();

                // Track files from Edit, Write, Read
                if matches!(name.as_str(), "Edit" | "Write" | "Read" | "Glob" | "Grep") {
                    if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
                        seen_files.insert(path.to_string());
                    }
                    if let Some(path) = input.get("path").and_then(|p| p.as_str()) {
                        seen_files.insert(path.to_string());
                    }
                }

                let summary = summarize_tool_input(&name, &input);

                tool_uses.push(ToolUse {
                    name,
                    input_summary: summary,
                    timestamp: timestamp.clone(),
                });
            }
        }
    }
}

fn summarize_tool_input(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Edit" | "Write" | "Read" => input
            .get("file_path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string(),
        "Bash" => input
            .get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.chars().take(80).collect())
            .unwrap_or_default(),
        "Glob" => input
            .get("pattern")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string(),
        "Grep" => input
            .get("pattern")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string(),
        "Agent" => input
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}
