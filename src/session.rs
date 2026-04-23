use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::parser;
use crate::util;

// ── Session state classification ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Starting, // process alive, no JSONL data yet
    Working,  // age < 30s OR cpu active, process alive
    Waiting,  // age < 300s, process alive, not cpu active
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

/// Classify session state from age, process liveness, and CPU activity.
///
/// `cpu_active` upgrades Waiting → Working when the process (or a descendant)
/// is consuming CPU despite no recent JSONL writes.
pub fn classify_state(age_secs: u64, process_dead: bool, cpu_active: bool) -> SessionState {
    if age_secs >= 300 {
        SessionState::Idle
    } else if process_dead {
        SessionState::Ended
    } else if age_secs < 30 || cpu_active {
        SessionState::Working
    } else {
        SessionState::Waiting
    }
}

// ── Process map ─────────────────────────────────────────────────────────────

/// Aggregated process data for robust Claude session detection.
///
/// Collects process info, CPU usage, child trees, and open file descriptors
/// in a single snapshot. Designed to be built once per refresh tick and
/// queried multiple times.
#[derive(Debug, Clone)]
pub struct ProcessMap {
    /// Encoded project dir name → list of PIDs
    pub by_project: HashMap<String, Vec<u32>>,
    /// Specific JSONL path → PID (from open FD matching)
    by_jsonl: HashMap<PathBuf, u32>,
    /// PID → CPU percentage
    cpu: HashMap<u32, f32>,
    /// PID → list of direct child PIDs
    children: HashMap<u32, Vec<u32>>,
}

impl ProcessMap {
    /// Build a process map by discovering all running Claude processes.
    ///
    /// Performs a single `ps` call to enumerate processes and build the child
    /// tree, then uses platform-specific methods to discover cwds and open
    /// JSONL files.
    pub fn discover() -> Self {
        let (all_procs, claude_pids) = enumerate_processes();

        // Build children map from ALL processes (needed for descendant CPU check)
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for proc in &all_procs {
            if proc.ppid > 0 {
                children.entry(proc.ppid).or_default().push(proc.pid);
            }
        }

        // Build CPU map for Claude PIDs + all their descendants
        let mut cpu: HashMap<u32, f32> = HashMap::new();
        for proc in &all_procs {
            cpu.insert(proc.pid, proc.cpu_pct);
        }

        // Discover cwds for Claude processes → project mapping
        let by_project = discover_claude_cwds(&claude_pids);

        // Discover open JSONL files for exact session matching
        let by_jsonl = discover_open_jsonls(&claude_pids);

        ProcessMap {
            by_project,
            by_jsonl,
            cpu,
            children,
        }
    }

    /// Check if a session's JSONL file has a running Claude process.
    ///
    /// Prefers exact FD-based matching; falls back to project-level matching.
    pub fn is_session_alive(&self, jsonl_path: &Path) -> bool {
        if self.by_jsonl.contains_key(jsonl_path) {
            return true;
        }
        project_dir_name(jsonl_path)
            .map(|name| self.by_project.contains_key(&name))
            .unwrap_or(false)
    }

    /// Find the PID that owns a session JSONL file.
    pub fn find_session_pid(&self, jsonl_path: &Path) -> Option<u32> {
        if let Some(&pid) = self.by_jsonl.get(jsonl_path) {
            return Some(pid);
        }
        project_dir_name(jsonl_path)
            .and_then(|name| self.by_project.get(&name))
            .and_then(|pids| pids.first().copied())
    }

    /// Check if a session's process (or any descendant) is consuming CPU.
    ///
    /// Returns true if the main process CPU > 1% or any descendant CPU > 5%.
    /// Thresholds follow abtop's heuristic to avoid false positives from idle
    /// watcher processes.
    pub fn is_cpu_active(&self, jsonl_path: &Path) -> bool {
        let Some(pid) = self.find_session_pid(jsonl_path) else {
            return false;
        };
        if self.cpu.get(&pid).copied().unwrap_or(0.0) > 1.0 {
            return true;
        }
        self.has_active_descendant(pid, 5.0)
    }

    /// Iterative DFS through the process tree to find any descendant above the
    /// CPU threshold.
    fn has_active_descendant(&self, pid: u32, threshold: f32) -> bool {
        let mut stack: Vec<u32> = self.children.get(&pid).cloned().unwrap_or_default();
        while let Some(cpid) = stack.pop() {
            if self.cpu.get(&cpid).copied().unwrap_or(0.0) > threshold {
                return true;
            }
            if let Some(grandchildren) = self.children.get(&cpid) {
                stack.extend(grandchildren);
            }
        }
        false
    }
}

// ── Process enumeration (single ps call) ────────────────────────────────────

struct ProcEntry {
    pid: u32,
    ppid: u32,
    cpu_pct: f32,
}

/// Enumerate all system processes in a single `ps` call.
/// Returns (all processes, filtered Claude PIDs).
fn enumerate_processes() -> (Vec<ProcEntry>, Vec<u32>) {
    let output = match Command::new("ps")
        .args(["-ww", "-eo", "pid,ppid,%cpu,comm"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut all = Vec::new();
    let mut claude_pids = Vec::new();

    for line in text.lines().skip(1) {
        // skip header
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let Some(pid) = parts[0].parse::<u32>().ok() else {
            continue;
        };
        let ppid = parts[1].parse::<u32>().unwrap_or(0);
        let cpu_pct = parts[2].parse::<f32>().unwrap_or(0.0);
        // comm may contain spaces if path; we need the last segment (binary name)
        let comm = parts[3..].join(" ");
        let binary_name = comm.rsplit('/').next().unwrap_or(&comm);

        let is_claude = binary_name == "claude";

        all.push(ProcEntry { pid, ppid, cpu_pct });

        if is_claude {
            // Exclude `claude --print` and similar non-interactive invocations
            if !comm.contains("--print") {
                claude_pids.push(pid);
            }
        }
    }

    (all, claude_pids)
}

/// Discover cwds of Claude processes and return encoded project dir → PIDs.
fn discover_claude_cwds(claude_pids: &[u32]) -> HashMap<String, Vec<u32>> {
    if claude_pids.is_empty() {
        return HashMap::new();
    }

    if Path::new("/proc").exists() {
        discover_cwds_proc(claude_pids)
    } else {
        discover_cwds_lsof(claude_pids)
    }
}

/// Linux: read /proc/{pid}/cwd symlink
fn discover_cwds_proc(pids: &[u32]) -> HashMap<String, Vec<u32>> {
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    for &pid in pids {
        let cwd_path = format!("/proc/{pid}/cwd");
        if let Ok(cwd) = std::fs::read_link(&cwd_path) {
            let encoded = encode_path(&cwd);
            map.entry(encoded).or_default().push(pid);
        }
    }
    map
}

/// macOS/BSD: use lsof to read cwds for specific PIDs
fn discover_cwds_lsof(pids: &[u32]) -> HashMap<String, Vec<u32>> {
    let pid_list: String = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let output = match Command::new("lsof")
        .args(["-p", &pid_list, "-a", "-d", "cwd", "-Fn"])
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
                let encoded = encode_path(Path::new(rest));
                map.entry(encoded).or_default().push(pid);
            }
        }
    }
    map
}

/// Discover which JSONL files are open by which Claude PIDs.
fn discover_open_jsonls(claude_pids: &[u32]) -> HashMap<PathBuf, u32> {
    if claude_pids.is_empty() {
        return HashMap::new();
    }

    if Path::new("/proc").exists() {
        discover_jsonls_proc(claude_pids)
    } else {
        discover_jsonls_lsof(claude_pids)
    }
}

/// Linux: read /proc/{pid}/fd symlinks and filter for .jsonl under ~/.claude/projects
fn discover_jsonls_proc(pids: &[u32]) -> HashMap<PathBuf, u32> {
    let claude_projects = dirs::home_dir()
        .map(|h| h.join(".claude").join("projects"))
        .unwrap_or_default();
    let prefix = claude_projects.to_string_lossy().to_string();

    let mut map = HashMap::new();
    for &pid in pids {
        let fd_dir = format!("/proc/{pid}/fd");
        let Ok(entries) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                let target_str = target.to_string_lossy();
                if target_str.starts_with(&prefix) && target_str.ends_with(".jsonl") {
                    map.insert(target, pid);
                }
            }
        }
    }
    map
}

/// macOS: use lsof to find open .jsonl files for specific PIDs
fn discover_jsonls_lsof(pids: &[u32]) -> HashMap<PathBuf, u32> {
    let claude_projects = dirs::home_dir()
        .map(|h| h.join(".claude").join("projects"))
        .unwrap_or_default();
    let prefix = claude_projects.to_string_lossy().to_string();

    let pid_list: String = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let output = match Command::new("lsof")
        .args(["-p", &pid_list, "-Fn"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return HashMap::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut map = HashMap::new();
    let mut current_pid: Option<u32> = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current_pid = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix('n') {
            if let Some(pid) = current_pid {
                if rest.starts_with(&prefix) && rest.ends_with(".jsonl") {
                    map.insert(PathBuf::from(rest), pid);
                }
            }
        }
    }
    map
}

// ── Path encoding / decoding ────────────────────────────────────────────────

/// Encode a filesystem path to the format used by ~/.claude/projects/.
/// `/Users/foo/bar` → `-Users-foo-bar`
fn encode_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

/// Decode an encoded project directory name back to a real path.
///
/// Uses greedy filesystem resolution: tries the longest possible segment
/// at each step, matching against directories that actually exist on disk.
/// This correctly handles hyphens in directory names (e.g.
/// `-Users-foo-my-project` → `/Users/foo/my-project` not `/Users/foo/my/project`).
pub fn decode_project_path(encoded: &str) -> (String, String) {
    let parts: Vec<&str> = encoded.split('-').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return (encoded.to_string(), encoded.to_string());
    }

    let mut path = PathBuf::from("/");
    let mut i = 0;

    while i < parts.len() {
        // Try longest possible segment first (greedy)
        let mut matched = false;
        for end in (i + 1..=parts.len()).rev() {
            let segment = parts[i..end].join("-");
            let candidate = path.join(&segment);
            if candidate.exists() {
                path = candidate;
                i = end;
                matched = true;
                break;
            }
        }
        if !matched {
            // No match on disk — use single part as fallback
            path.push(parts[i]);
            i += 1;
        }
    }

    let project_path = path.to_string_lossy().to_string();
    let project_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| project_path.clone());

    (project_path, project_name)
}

fn project_dir_name(jsonl_path: &Path) -> Option<String> {
    jsonl_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
}

/// Check if a session has recently-active subagent files.
///
/// When a skill or slash command runs, Claude Code writes to
/// `{project}/{session_id}/subagents/{subagent_id}.jsonl` instead of the
/// main session JSONL.  This function detects that activity so the parent
/// session is not incorrectly marked as ended.
pub fn has_active_subagents(jsonl_path: &Path, max_age_secs: u64) -> bool {
    let session_id = match jsonl_path.file_stem() {
        Some(s) => s.to_string_lossy().to_string(),
        None => return false,
    };
    let parent = match jsonl_path.parent() {
        Some(p) => p,
        None => return false,
    };

    let subagents_dir = parent.join(&session_id).join("subagents");
    let entries = match std::fs::read_dir(&subagents_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    let now = SystemTime::now();
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|e| e == "jsonl") {
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    let age = now
                        .duration_since(modified)
                        .map(|d| d.as_secs())
                        .unwrap_or(u64::MAX);
                    if age < max_age_secs {
                        return true;
                    }
                }
            }
        }
    }
    false
}

// ── Project discovery ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub project_path: String,
    pub project_name: String,
    pub session_count: usize,
    pub last_modified: SystemTime,
}

/// Discover all Claude Code projects from ~/.claude/projects/.
/// Returns a list of projects sorted by most recently active.
pub fn discover_projects() -> Result<Vec<ProjectInfo>> {
    let claude_dir = dirs::home_dir()
        .context("No home directory")?
        .join(".claude")
        .join("projects");

    if !claude_dir.exists() {
        return Ok(Vec::new());
    }

    let mut projects = Vec::new();

    for project_entry in std::fs::read_dir(&claude_dir)? {
        let project_entry = project_entry?;
        let project_dir = project_entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let project_encoded = project_entry.file_name().to_string_lossy().to_string();
        let (project_path, project_name) = decode_project_path(&project_encoded);

        let mut session_count = 0usize;
        let mut last_modified = SystemTime::UNIX_EPOCH;

        for file_entry in std::fs::read_dir(&project_dir).into_iter().flatten() {
            let file_entry = match file_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = file_entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if path.to_string_lossy().contains("subagents") {
                continue;
            }
            session_count += 1;
            if let Ok(meta) = file_entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if modified > last_modified {
                        last_modified = modified;
                    }
                }
            }
        }

        if session_count > 0 {
            projects.push(ProjectInfo {
                project_path,
                project_name,
                session_count,
                last_modified,
            });
        }
    }

    projects.sort_by_key(|p| std::cmp::Reverse(p.last_modified));
    Ok(projects)
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
        let (project_path, project_name) = decode_project_path(&project_encoded);

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
pub fn discover_starting_sessions(existing: &[SessionInfo], pmap: &ProcessMap) -> Vec<SessionInfo> {
    let now = SystemTime::now();

    // Count recently-active sessions per project directory.
    // A session is active if: recently modified, has a live process, or has
    // active subagents (skill/slash command running).
    let mut active_per_project: HashMap<String, usize> = HashMap::new();
    for s in existing {
        let age = now
            .duration_since(s.modified)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        let active = age < 300
            || pmap.is_session_alive(&s.jsonl_path)
            || has_active_subagents(&s.jsonl_path, 60);
        if active {
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
    for project_encoded in pmap.by_project.keys() {
        // Only consider processes whose CWD matches a known project directory.
        // Subagent / skill processes often have a different CWD that doesn't
        // correspond to any real project — skip them.
        let project_dir = claude_dir.join(project_encoded);
        if !project_dir.is_dir() {
            continue;
        }

        // Project has at least one active session → no starting entry needed
        if active_per_project
            .get(project_encoded)
            .copied()
            .unwrap_or(0)
            > 0
        {
            continue;
        }

        let (project_path, project_name) = decode_project_path(project_encoded);

        result.push(SessionInfo {
            session_id: format!("starting-{project_encoded}"),
            project_path,
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
    let pmap = ProcessMap::discover();

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
            !pmap.is_session_alive(&s.jsonl_path)
        } else {
            false
        };
        let cpu_active = pmap.is_cpu_active(&s.jsonl_path);
        let state = classify_state(parsed.age_secs, process_dead, cpu_active);
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
