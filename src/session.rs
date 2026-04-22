use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::parser;
use crate::util;

// ── Session state classification ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Starting, // process alive, no JSONL data yet
    Working,  // age < 30s, process alive
    Waiting,  // age < 300s, process alive
    Ended,    // age < 300s, process NOT alive
    Idle,     // age >= 300s
}

impl SessionState {
    pub fn sort_key(self) -> u8 {
        match self {
            SessionState::Starting => 0,
            SessionState::Working => 1,
            SessionState::Waiting => 2,
            SessionState::Ended => 3,
            SessionState::Idle => 4,
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            SessionState::Starting | SessionState::Working | SessionState::Waiting
        )
    }
}

/// Classify session state from age and process liveness.
pub fn classify_state(age_secs: u64, process_dead: bool) -> SessionState {
    if age_secs >= 300 {
        SessionState::Idle
    } else if process_dead {
        SessionState::Ended
    } else if age_secs < 30 {
        SessionState::Working
    } else {
        SessionState::Waiting
    }
}

// ── Process detection ───────────────────────────────────────────────────────

use std::collections::HashMap;

/// Discover running Claude processes.
/// Uses `lsof` on macOS, falls back to `/proc` on Linux.
/// Returns a map of encoded project directory name → list of PIDs.
/// The key matches the directory names under `~/.claude/projects/`.
pub fn discover_claude_pids() -> HashMap<String, Vec<u32>> {
    // Try /proc first (Linux), fall back to lsof (macOS/BSD)
    if Path::new("/proc").exists() {
        discover_claude_pids_proc()
    } else {
        discover_claude_pids_lsof()
    }
}

/// Linux: scan /proc/*/cmdline for "claude" and read /proc/*/cwd symlink
fn discover_claude_pids_proc() -> HashMap<String, Vec<u32>> {
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return map;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };

        // Check if this is a claude process
        let cmdline_path = format!("/proc/{pid}/cmdline");
        let Ok(cmdline) = std::fs::read_to_string(&cmdline_path) else {
            continue;
        };
        if !cmdline.contains("claude") {
            continue;
        }

        // Read cwd symlink
        let cwd_path = format!("/proc/{pid}/cwd");
        if let Ok(cwd) = std::fs::read_link(&cwd_path) {
            let encoded = cwd.to_string_lossy().replace('/', "-");
            map.entry(encoded).or_default().push(pid);
        }
    }
    map
}

/// macOS/BSD: use `lsof -c claude -a -d cwd -Fn`
fn discover_claude_pids_lsof() -> HashMap<String, Vec<u32>> {
    let output = match Command::new("lsof")
        .args(["-c", "claude", "-a", "-d", "cwd", "-Fn"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return HashMap::new(),
    };

    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut current_pid: Option<u32> = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current_pid = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix('n') {
            if let Some(pid) = current_pid {
                let encoded = rest.replace('/', "-");
                map.entry(encoded).or_default().push(pid);
            }
        }
    }
    map
}

/// Check if a session's project has a running Claude process.
pub fn is_session_alive(jsonl_path: &Path, claude_pids: &HashMap<String, Vec<u32>>) -> bool {
    project_dir_name(jsonl_path)
        .map(|name| claude_pids.contains_key(&name))
        .unwrap_or(false)
}

/// Find a Claude PID for a session's project (for killing).
pub fn find_session_pid(jsonl_path: &Path, claude_pids: &HashMap<String, Vec<u32>>) -> Option<u32> {
    project_dir_name(jsonl_path)
        .and_then(|name| claude_pids.get(&name))
        .and_then(|pids| pids.first().copied())
}

fn project_dir_name(jsonl_path: &Path) -> Option<String> {
    jsonl_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
}

// ── Session discovery ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    #[allow(dead_code)]
    pub project_path: String,
    pub project_name: String,
    pub jsonl_path: PathBuf,
    pub modified: SystemTime,
}

/// Discover all Claude Code sessions from ~/.claude/projects/
pub fn discover_sessions() -> Result<Vec<SessionInfo>> {
    let claude_dir = dirs::home_dir()
        .context("No home directory")?
        .join(".claude")
        .join("projects");

    if !claude_dir.exists() {
        bail!(
            "No Claude Code projects directory found at {}",
            claude_dir.display()
        );
    }

    let mut sessions = Vec::new();

    for project_entry in std::fs::read_dir(&claude_dir)? {
        let project_entry = project_entry?;
        let project_dir = project_entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let project_encoded = project_entry.file_name().to_string_lossy().to_string();
        // Decode: -Users-foo-bar -> /Users/foo/bar
        let project_path = project_encoded.replace('-', "/");
        let project_name = project_path
            .rsplit('/')
            .next()
            .unwrap_or(&project_path)
            .to_string();

        for file_entry in std::fs::read_dir(&project_dir)? {
            let file_entry = file_entry?;
            let path = file_entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            // Skip subagent directories
            if path.to_string_lossy().contains("subagents") {
                continue;
            }

            let session_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            let modified = file_entry.metadata()?.modified()?;

            sessions.push(SessionInfo {
                session_id,
                project_path: project_path.clone(),
                project_name: project_name.clone(),
                jsonl_path: path,
                modified,
            });
        }
    }

    // Sort by most recently modified first
    sessions.sort_by_key(|b| std::cmp::Reverse(b.modified));
    Ok(sessions)
}

/// Discover running Claude processes that don't have a matching active session yet.
/// Compares PID count per project with recently-active session count.
/// Returns synthetic SessionInfo entries for the excess (starting) processes.
pub fn discover_starting_sessions(
    existing: &[SessionInfo],
    claude_pids: &HashMap<String, Vec<u32>>,
) -> Vec<SessionInfo> {
    let now = SystemTime::now();

    // Count recently-active sessions per project directory
    let mut active_per_project: HashMap<String, usize> = HashMap::new();
    for s in existing {
        let age = now
            .duration_since(s.modified)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        // A session modified in the last 5 minutes is considered active
        if age < 300 {
            if let Some(name) = project_dir_name(&s.jsonl_path) {
                *active_per_project.entry(name).or_default() += 1;
            }
        }
    }

    let claude_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("projects"),
        None => return Vec::new(),
    };

    let mut result = Vec::new();
    for project_encoded in claude_pids.keys() {
        // Process running but no active session → one starting entry
        if active_per_project
            .get(project_encoded)
            .copied()
            .unwrap_or(0)
            > 0
        {
            continue;
        }

        // Decode: -Users-foo-bar -> /Users/foo/bar (same as discover_sessions)
        let project_path = project_encoded.replace('-', "/");
        let project_name = project_path
            .rsplit('/')
            .next()
            .unwrap_or(&project_path)
            .to_string();

        result.push(SessionInfo {
            session_id: format!("starting-{project_encoded}"),
            project_path: project_path.clone(),
            project_name,
            jsonl_path: claude_dir.join(project_encoded).join("_starting.jsonl"),
            modified: SystemTime::now(),
        });
    }
    result
}

/// Pick a session: by explicit ID, by project filter, or the most recent
pub fn pick_session(
    sessions: &[SessionInfo],
    session_id: Option<&str>,
    project: Option<&str>,
) -> Result<SessionInfo> {
    if sessions.is_empty() {
        bail!("No Claude Code sessions found");
    }

    if let Some(id) = session_id {
        return sessions
            .iter()
            .find(|s| s.session_id.starts_with(id))
            .cloned()
            .context(format!("No session matching '{id}'"));
    }

    if let Some(proj) = project {
        let proj_lower = proj.to_lowercase();
        return sessions
            .iter()
            .find(|s| s.project_name.to_lowercase().contains(&proj_lower))
            .cloned()
            .context(format!("No session for project matching '{proj}'"));
    }

    // Default: most recent
    Ok(sessions[0].clone())
}

/// Print status of all recent sessions
pub fn print_status(sessions: &[SessionInfo]) -> Result<()> {
    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    let now = SystemTime::now();
    let limit = 10.min(sessions.len());
    let claude_pids = discover_claude_pids();

    println!(
        "{}",
        format!(
            " {:^8} {:^14} {:^10} {:^7} {:^6} {:>7}  {}",
            "STATUS", "PROJECT", "MODEL", "CONTEXT", "TURNS", "AGE", "SESSION"
        )
        .bold()
    );

    for s in &sessions[..limit] {
        let parsed = parser::parse_session(s)?;

        let age = now
            .duration_since(s.modified)
            .map(|d| util::format_duration(d.as_secs()))
            .unwrap_or_else(|_| "?".into());

        let window = util::context_window(&parsed.model, parsed.current_context_tokens);
        let pct = if window > 0 {
            (parsed.current_context_tokens as f64 / window as f64 * 100.0) as u8
        } else {
            0
        };

        let process_dead = if parsed.age_secs < 300 {
            !is_session_alive(&s.jsonl_path, &claude_pids)
        } else {
            false
        };
        let state = classify_state(parsed.age_secs, process_dead);
        let status = match state {
            SessionState::Starting => "starting".cyan(),
            SessionState::Working => "working".green(),
            SessionState::Waiting => "waiting".yellow(),
            SessionState::Ended => "ended".red(),
            SessionState::Idle => "idle".dimmed(),
        };

        let context_display = format!("{}%", pct);
        let context_colored = if pct >= 90 {
            context_display.red().bold()
        } else if pct >= 70 {
            context_display.yellow()
        } else {
            context_display.normal()
        };

        let model_short = parsed.model.replace("claude-", "").replace("-20", "");

        println!(
            " {:>8} {:>14} {:>10} {:>7} {:>6} {:>7}  {}",
            status,
            s.project_name.chars().take(14).collect::<String>(),
            model_short.chars().take(10).collect::<String>(),
            context_colored,
            parsed.turn_count,
            age,
            util::short_session_id(&s.session_id),
        );
    }

    if sessions.len() > limit {
        println!(" ... and {} more sessions", sessions.len() - limit);
    }

    Ok(())
}
