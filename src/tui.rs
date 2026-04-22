use std::collections::{HashMap, HashSet};
use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};

use crate::config::Config;
use crate::git;
use crate::handoff;
use crate::hooks;
use crate::parser::{self, ParsedSession};
use crate::session::{self, SessionInfo};
use crate::statusline::{self, SessionStatus};
use crate::storage;
use crate::util;

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

// ── Palette ─────────────────────────────────────────────────────────────────

// Design: classic dark — #0a0a0a bg, green phosphor accents
const GREEN: Color = Color::Rgb(127, 209, 127); // --green: #7fd17f
const GREEN_DIM: Color = Color::Rgb(74, 122, 74); // --green-dim: #4a7a4a
const GREEN_GLOW: Color = Color::Rgb(168, 232, 168); // --green-glow: #a8e8a8
const AMBER: Color = Color::Rgb(212, 179, 106); // --amber: #d4b36a
const RED: Color = Color::Rgb(209, 127, 127); // --red: #d17f7f
const CYAN: Color = Color::Rgb(127, 200, 209); // --cyan: #7fc8d1
const VIOLET: Color = Color::Rgb(179, 156, 209); // --violet: #b39cd1
const FG: Color = Color::Rgb(215, 215, 215); // --fg: #d7d7d7
const DIM: Color = Color::Rgb(138, 138, 138); // --fg-dim: #8a8a8a
const DIMMER: Color = Color::Rgb(85, 85, 85); // --fg-mute: #555555
const BORDER: Color = Color::Rgb(36, 36, 36); // --line: #242424
const BORDER_SOFT: Color = Color::Rgb(26, 26, 26); // --line-soft: #1a1a1a
const BG: Color = Color::Rgb(10, 10, 10); // --bg: #0a0a0a
const BG_ALT: Color = Color::Rgb(15, 15, 15); // --bg-alt: #0f0f0f
const BG_SELECT: Color = Color::Rgb(22, 34, 26); // --bg-sel: #16221a

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Sessions,
    Handoffs,
}

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Left,
    Center,
    Right,
}

struct HandoffEntry {
    id: String,
    preview: String,
    content: String,
    size: u64,
}

struct PendingRestart {
    cwd: String,
    restart_at: Instant,
}

struct App {
    tab: Tab,
    focus: Focus,
    sessions: Vec<(SessionInfo, ParsedSession)>,
    handoffs: Vec<HandoffEntry>,
    table_state: TableState,
    handoff_state: ListState,
    show_detail: bool,
    hide_idle: bool,
    should_quit: bool,
    last_refresh: Instant,
    status_msg: Option<(String, Instant)>,
    // Auto-handoff
    config: Config,
    // Runtime overrides for toggleable settings (persist across config reloads)
    auto_handoff_override: Option<bool>,
    auto_commit_override: Option<bool>,
    triggered_sessions: HashSet<String>,
    // Sessions pending handoff (waiting for Stop event before triggering)
    pending_handoffs: HashSet<String>,
    // Live data from statusLine hook (keyed by session_id)
    statuses: HashMap<String, SessionStatus>,
    // Process liveness cache: session IDs confirmed dead
    dead_sessions: HashSet<String>,
    // Process snapshot (rebuilt each refresh tick)
    pmap: session::ProcessMap,
    // Pending restart: fallback spawn if wrapper doesn't pick up next_prompt
    pending_restart: Option<PendingRestart>,
}

// ── App ─────────────────────────────────────────────────────────────────────

impl App {
    fn new() -> Self {
        let config = crate::config::load().unwrap_or_default();
        // Install statusLine hook if not already configured, clean stale data
        statusline::ensure_hook();
        statusline::cleanup_stale();
        // Install Stop hook for auto-commit signaling
        hooks::ensure_hooks();
        let mut app = Self {
            tab: Tab::Sessions,
            focus: Focus::Center,
            sessions: Vec::new(),
            handoffs: Vec::new(),
            table_state: TableState::default(),
            handoff_state: ListState::default(),
            show_detail: true,
            hide_idle: true,
            should_quit: false,
            last_refresh: Instant::now() - Duration::from_secs(100),
            status_msg: None,
            config,
            auto_handoff_override: None,
            auto_commit_override: None,
            triggered_sessions: HashSet::new(),
            pending_handoffs: HashSet::new(),
            statuses: HashMap::new(),
            dead_sessions: HashSet::new(),
            pmap: session::ProcessMap::discover(),
            pending_restart: None,
        };
        app.refresh();
        app
    }

    /// Effective auto_handoff value (runtime override > config file)
    fn auto_handoff(&self) -> bool {
        self.auto_handoff_override
            .unwrap_or(self.config.auto_handoff)
    }

    /// Effective auto_commit value (runtime override > config file)
    fn auto_commit(&self) -> bool {
        self.auto_commit_override.unwrap_or(self.config.auto_commit)
    }

    fn refresh(&mut self) {
        // Hot-reload config from disk (runtime overrides are preserved separately)
        if let Ok(new_config) = crate::config::load() {
            self.config = new_config;
        }

        // Single snapshot of all process data (ps + lsof/proc, once per tick)
        let pmap = session::ProcessMap::discover();
        self.pmap = pmap.clone();

        if let Ok(sessions) = session::discover_sessions() {
            self.sessions = sessions
                .into_iter()
                .filter_map(|s| parser::parse_session(&s).ok().map(|p| (s, p)))
                .collect();

            // Detect processes with no JSONL yet (starting sessions)
            let existing_infos: Vec<_> =
                self.sessions.iter().map(|(info, _)| info.clone()).collect();
            for starting in session::discover_starting_sessions(&existing_infos, &pmap) {
                let empty_parsed = parser::empty_parsed(&starting);
                self.sessions.push((starting, empty_parsed));
            }

            // Resurrect sessions that show new activity: recent JSONL write,
            // active subagents (skill running), or process detected alive again.
            self.dead_sessions.retain(|sid| {
                let Some((info, parsed)) = self.sessions.iter().find(|(i, _)| i.session_id == *sid)
                else {
                    return false; // session gone → remove from dead list
                };
                // Keep in dead_sessions only if ALL of these hold:
                parsed.age_secs >= 5
                    && !session::has_active_subagents(&info.jsonl_path, 30)
                    && !pmap.is_session_alive(&info.jsonl_path)
            });

            // Check process liveness for recent sessions (skip starting sessions).
            // Don't mark as dead if the JSONL was just written to (age < 10s)
            // or if subagents are actively running (skill/slash command).
            for (info, parsed) in &self.sessions {
                if !info.session_id.starts_with("starting-")
                    && parsed.age_secs < 300
                    && parsed.age_secs >= 10
                    && !self.dead_sessions.contains(&info.session_id)
                    && !pmap.is_session_alive(&info.jsonl_path)
                    && !session::has_active_subagents(&info.jsonl_path, 30)
                {
                    self.dead_sessions.insert(info.session_id.clone());
                }
            }

            let dead = &self.dead_sessions;
            self.sessions.sort_by_key(|(info, p)| {
                if info.session_id.starts_with("starting-") {
                    return session::SessionState::Starting.sort_key();
                }
                let process_dead = dead.contains(&info.session_id);
                let cpu_active = pmap.is_cpu_active(&info.jsonl_path);
                session::classify_state(p.age_secs, process_dead, cpu_active).sort_key()
            });
        }
        self.handoffs = load_handoffs();
        self.statuses = statusline::read_all();

        let vis_count = self.visible_count();
        if self.table_state.selected().is_none_or(|i| i >= vis_count) {
            self.table_state
                .select(if vis_count > 0 { Some(0) } else { None });
        }
        if self.handoff_state.selected().is_none() && !self.handoffs.is_empty() {
            self.handoff_state.select(Some(0));
        }
        self.last_refresh = Instant::now();
    }

    fn visible_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|(_, p)| !self.hide_idle || p.age_secs < 300)
            .count()
    }

    fn selected_session_data(&self) -> Option<(SessionInfo, ParsedSession)> {
        let visible: Vec<&(SessionInfo, ParsedSession)> = self
            .sessions
            .iter()
            .filter(|(_, p)| !self.hide_idle || p.age_secs < 300)
            .collect();
        self.table_state
            .selected()
            .and_then(|i| visible.get(i))
            .map(|(s, p)| ((*s).clone(), (*p).clone()))
    }

    fn save_selected(&mut self) {
        let data = self.selected_session_data();
        if let Some((info, parsed)) = data {
            self.status_msg = Some(("generating handoff...".to_string(), Instant::now()));
            match handoff::generate_summary(&parsed) {
                Ok(content) => match storage::save(&content, &info) {
                    Ok(id) => {
                        self.status_msg = Some((format!("✓ Saved: {id}"), Instant::now()));
                        self.handoffs = load_handoffs();
                    }
                    Err(e) => {
                        self.status_msg = Some((format!("✗ {e}"), Instant::now()));
                    }
                },
                Err(e) => {
                    self.status_msg = Some((format!("✗ {e}"), Instant::now()));
                }
            }
        }
    }

    fn move_up(&mut self) {
        match self.tab {
            Tab::Sessions => {
                let i = self.table_state.selected().unwrap_or(0);
                if i > 0 {
                    self.table_state.select(Some(i - 1));
                }
            }
            Tab::Handoffs => {
                let i = self.handoff_state.selected().unwrap_or(0);
                if i > 0 {
                    self.handoff_state.select(Some(i - 1));
                }
            }
        }
    }

    fn move_down(&mut self) {
        match self.tab {
            Tab::Sessions => {
                let max = self.visible_count().saturating_sub(1);
                let i = self.table_state.selected().unwrap_or(0);
                if i < max {
                    self.table_state.select(Some(i + 1));
                }
            }
            Tab::Handoffs => {
                let max = self.handoffs.len().saturating_sub(1);
                let i = self.handoff_state.selected().unwrap_or(0);
                if i < max {
                    self.handoff_state.select(Some(i + 1));
                }
            }
        }
    }

    fn next_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Sessions => Tab::Handoffs,
            Tab::Handoffs => Tab::Sessions,
        };
    }

    fn toggle_idle(&mut self) {
        self.hide_idle = !self.hide_idle;
        let vis = self.visible_count();
        if let Some(i) = self.table_state.selected() {
            if i >= vis {
                self.table_state
                    .select(if vis > 0 { Some(vis - 1) } else { None });
            }
        }
    }

    fn session_state(&self, info: &SessionInfo, parsed: &ParsedSession) -> session::SessionState {
        if info.session_id.starts_with("starting-") {
            return session::SessionState::Starting;
        }
        let process_dead = self.dead_sessions.contains(&info.session_id);
        let cpu_active = self.pmap.is_cpu_active(&info.jsonl_path);
        session::classify_state(parsed.age_secs, process_dead, cpu_active)
    }

    fn check_auto_handoff(&mut self) {
        if !self.auto_handoff() {
            return;
        }

        // Mark sessions that hit the threshold as pending (wait for Stop event)
        let pending: Vec<String> = self
            .sessions
            .iter()
            .filter(|(info, parsed)| {
                !self.triggered_sessions.contains(&info.session_id)
                    && !self.pending_handoffs.contains(&info.session_id)
                    && self.session_state(info, parsed).is_active()
                    && parsed.turn_count > 2
            })
            .filter(|(_, parsed)| {
                let pct = context_pct(parsed);
                let by_context = pct >= self.config.threshold;
                let by_turns =
                    self.config.max_turns > 0 && parsed.turn_count >= self.config.max_turns;
                by_context || by_turns
            })
            .map(|(info, _)| info.session_id.clone())
            .collect();

        for sid in pending {
            self.pending_handoffs.insert(sid.clone());
            let pct_msg = self
                .sessions
                .iter()
                .find(|(i, _)| i.session_id == sid)
                .map(|(_, p)| context_pct(p))
                .unwrap_or(0);
            self.status_msg = Some((
                format!("threshold {}% — waiting for turn to finish...", pct_msg),
                Instant::now(),
            ));
        }
    }

    fn check_pending_handoffs(&mut self) {
        if self.pending_handoffs.is_empty() {
            return;
        }

        let events = hooks::read_stop_events();
        for event in &events {
            if self.pending_handoffs.remove(&event.session_id) {
                // Stop event arrived for a pending session — trigger handoff now
                let session_data = self
                    .sessions
                    .iter()
                    .find(|(info, _)| info.session_id == event.session_id)
                    .map(|(info, parsed)| (info.clone(), parsed.clone()));

                if let Some((info, parsed)) = session_data {
                    self.trigger_handoff(&info, &parsed);
                }

                hooks::clear_event(&event.session_id);
            }
        }
    }

    fn trigger_handoff(&mut self, info: &SessionInfo, parsed: &ParsedSession) {
        // Auto-commit before handoff if enabled
        if self.auto_commit() && self.config.commit_before_handoff {
            let status = self.statuses.get(&info.session_id).cloned();
            let cwd = status
                .as_ref()
                .map(|s| s.cwd.as_str())
                .filter(|c| !c.is_empty())
                .unwrap_or(&parsed.cwd)
                .to_string();
            if !cwd.is_empty() {
                self.try_auto_commit(&cwd, parsed, status.as_ref(), "before handoff");
            }
        }

        let pct = context_pct(parsed);
        let reason = if pct >= self.config.threshold {
            format!("context at {}%", pct)
        } else {
            format!("{} turns", parsed.turn_count)
        };

        self.status_msg = Some((format!("{reason} — generating handoff..."), Instant::now()));

        // Generate structured handoff
        let hoff = match handoff::generate_summary(parsed) {
            Ok(h) => h,
            Err(e) => {
                self.status_msg = Some((format!("handoff failed: {e}"), Instant::now()));
                return;
            }
        };

        // Save handoff
        let id = match storage::save(&hoff, info) {
            Ok(id) => id,
            Err(e) => {
                self.status_msg = Some((format!("save failed: {e}"), Instant::now()));
                return;
            }
        };

        // Write next_prompt for the shell wrapper
        let prompt = format!(
            "Context from a previous session (auto-handoff by relay, {}). Resume where we left off:\n\n---\n\n{}",
            reason, hoff
        );
        let next_prompt_path = match dirs::home_dir() {
            Some(h) => h.join(".relay").join("next_prompt"),
            None => {
                self.status_msg = Some(("no home directory".to_string(), Instant::now()));
                return;
            }
        };
        if let Err(e) = util::write_atomic(&next_prompt_path, prompt.as_bytes()) {
            self.status_msg = Some((format!("write next_prompt: {e}"), Instant::now()));
            return;
        }
        util::set_private_permissions(&next_prompt_path);

        // Find and kill Claude
        let killed = if let Some(pid) = find_claude_pid(&info.jsonl_path) {
            kill_process(pid);
            true
        } else {
            false
        };

        // Terminal bell
        if self.config.sound {
            print!("\x07");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }

        // Notify
        if self.config.notify {
            #[cfg(feature = "notifications")]
            {
                let body = if killed {
                    format!("{reason} — restarting")
                } else {
                    format!("{reason} — handoff saved (could not find claude pid)")
                };
                let _ = notify_rust::Notification::new()
                    .summary("relay — auto-handoff")
                    .body(&body)
                    .show();
            }
        }

        // Webhook notifications (Discord, Slack)
        crate::notify::send_handoff(&self.config, &reason, killed);

        self.triggered_sessions.insert(info.session_id.clone());
        self.handoffs = load_handoffs();

        // Schedule fallback restart: if the wrapper doesn't consume next_prompt
        // within cooldown+10s, relay spawns claude directly in a new terminal
        let cwd = self
            .statuses
            .get(&info.session_id)
            .map(|s| s.cwd.as_str())
            .filter(|c| !c.is_empty())
            .unwrap_or(&parsed.cwd)
            .to_string();
        self.pending_restart = Some(PendingRestart {
            cwd,
            restart_at: Instant::now() + Duration::from_secs(self.config.cooldown + 10),
        });

        let status = if killed {
            format!("auto-handoff: {} — {id}", reason)
        } else {
            format!("handoff saved: {} — pid not found", reason)
        };
        self.status_msg = Some((status, Instant::now()));
    }

    // ── Git auto-commit ────────────────────────────────────────────────────

    fn check_auto_commit(&mut self) {
        if !self.auto_commit() {
            return;
        }

        let events = hooks::read_stop_events();
        for event in events {
            // Find the matching session
            let session_data = self
                .sessions
                .iter()
                .find(|(info, _)| info.session_id == event.session_id);

            if let Some((_, parsed)) = session_data {
                let status = self.statuses.get(&event.session_id).cloned();
                let cwd = if !event.cwd.is_empty() {
                    event.cwd.clone()
                } else if !event.project_dir.is_empty() {
                    event.project_dir.clone()
                } else {
                    parsed.cwd.clone()
                };

                let parsed = parsed.clone();
                self.try_auto_commit(&cwd, &parsed, status.as_ref(), "stop");
            }

            // Always clean up the event file
            hooks::clear_event(&event.session_id);
        }
    }

    fn check_pending_restart(&mut self) {
        let pending = match &self.pending_restart {
            Some(p) => p,
            None => return,
        };

        let next_prompt = match dirs::home_dir() {
            Some(h) => h.join(".relay").join("next_prompt"),
            None => return,
        };

        // Check if wrapper already consumed next_prompt (before timeout)
        if !next_prompt.exists() {
            self.pending_restart = None;
            self.status_msg = Some(("wrapper restarted session".to_string(), Instant::now()));
            return;
        }

        // Not yet timed out — show countdown
        if Instant::now() < pending.restart_at {
            let remaining = pending.restart_at.duration_since(Instant::now()).as_secs();
            self.status_msg = Some((
                format!(
                    "waiting for wrapper to restart... {}s (source ~/.relay/claude-wrapper.sh)",
                    remaining
                ),
                Instant::now(),
            ));
            return;
        }

        // Timed out — wrapper didn't handle it, try fallback
        let cwd = pending.cwd.clone();
        self.pending_restart = None;

        if spawn_claude_in_terminal(&cwd) {
            self.status_msg = Some((
                "wrapper not active — spawned claude in new terminal".to_string(),
                Instant::now(),
            ));
        } else {
            self.status_msg = Some((
                "restart failed — run: source ~/.relay/claude-wrapper.sh && relay restore"
                    .to_string(),
                Instant::now(),
            ));
        }
    }

    fn try_auto_commit(
        &mut self,
        cwd: &str,
        parsed: &ParsedSession,
        status: Option<&SessionStatus>,
        reason: &str,
    ) {
        if cwd.is_empty() {
            return;
        }
        if !git::is_git_repo(cwd) {
            return;
        }
        if !git::has_uncommitted_changes(cwd) {
            return;
        }

        let message =
            git::generate_commit_message(cwd, parsed, status, reason, &self.config.commit_prefix);

        match git::auto_commit(cwd, &message) {
            Ok(hash) => {
                if self.config.sound {
                    print!("\x07");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                self.status_msg = Some((format!("git: committed {hash}"), Instant::now()));
            }
            Err(e) => {
                self.status_msg = Some((format!("git: {e}"), Instant::now()));
            }
        }
    }
}

// ── Auto-handoff helpers ────────────────────────────────────────────────────

fn context_pct(parsed: &ParsedSession) -> u8 {
    let window = util::context_window(&parsed.model, parsed.current_context_tokens);
    if window > 0 {
        (parsed.current_context_tokens as f64 / window as f64 * 100.0) as u8
    } else {
        0
    }
}

/// Find the Claude Code process that owns a session JSONL file.
fn find_claude_pid(jsonl_path: &std::path::Path) -> Option<u32> {
    // Use ProcessMap for exact FD-based matching, then cwd fallback
    let pmap = session::ProcessMap::discover();
    if let Some(pid) = pmap.find_session_pid(jsonl_path) {
        return Some(pid);
    }

    // Fallback: pgrep for exact process name "claude", excluding our own PID
    let my_pid = std::process::id();
    let output = Command::new("pgrep")
        .args(["-x", "claude"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .find(|&pid| pid != my_pid)
}

fn kill_process(pid: u32) {
    let _ = Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Spawn claude in a new terminal window with the handoff prompt.
/// Fallback when the shell wrapper isn't sourced.
fn spawn_claude_in_terminal(cwd: &str) -> bool {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return false,
    };
    let next_prompt = home.join(".relay").join("next_prompt");
    let restart_script = home.join(".relay").join("restart.sh");

    let next_prompt_escaped = next_prompt.display().to_string().replace('\'', "'\\''");
    let script_content = format!(
        "#!/bin/bash\ncd '{}'\nstty sane 2>/dev/null\nprompt=$(cat '{}')\nrm -f '{}'\nprintf '\\n  \\033[36mrelay:\\033[0m restarting claude session...\\n\\n'\nsleep 2\nexec claude \"$prompt\"\n",
        cwd.replace('\'', "'\\''"),
        next_prompt_escaped,
        next_prompt_escaped,
    );

    if std::fs::write(&restart_script, &script_content).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&restart_script, std::fs::Permissions::from_mode(0o700));
    }

    #[cfg(target_os = "macos")]
    {
        let script_path_escaped = restart_script
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let applescript = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"bash '{}'\"\nend tell",
            script_path_escaped
        );
        return Command::new("osascript")
            .arg("-e")
            .arg(&applescript)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }

    #[allow(unreachable_code)]
    false
}

// ── Data loading ────────────────────────────────────────────────────────────

fn load_handoffs() -> Vec<HandoffEntry> {
    let dir = match dirs::home_dir() {
        Some(h) => h.join(".relay").join("handoffs"),
        None => return Vec::new(),
    };
    if !dir.exists() {
        return Vec::new();
    }

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();

    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

    entries
        .iter()
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name.trim_end_matches(".md").to_string();
            let content = std::fs::read_to_string(e.path()).unwrap_or_default();
            let preview = content
                .lines()
                .find(|l| l.starts_with("**Context:**"))
                .unwrap_or("")
                .to_string();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            HandoffEntry {
                id,
                preview,
                content,
                size,
            }
        })
        .collect()
}

// ── Entry point ─────────────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let result = run_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if app.last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh();
            app.check_auto_handoff();
            app.check_pending_handoffs();
            app.check_auto_commit();
        }
        app.check_pending_restart();

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                handle_key(app, key);
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

// ── Event handling ──────────────────────────────────────────────────────────

fn handle_key(app: &mut App, key: event::KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Char('h') | KeyCode::Left => {
            app.focus = match app.focus {
                Focus::Right => Focus::Center,
                Focus::Center => Focus::Left,
                Focus::Left => Focus::Left,
            };
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.focus = match app.focus {
                Focus::Left => Focus::Center,
                Focus::Center => Focus::Right,
                Focus::Right => Focus::Right,
            };
        }
        KeyCode::Tab | KeyCode::BackTab => app.next_tab(),
        KeyCode::Char('s') => app.save_selected(),
        KeyCode::Char('r') => {
            app.refresh();
            app.status_msg = Some(("✓ Refreshed".to_string(), Instant::now()));
        }
        KeyCode::Char('a') => {
            let new_val = !app.auto_handoff();
            app.auto_handoff_override = Some(new_val);
            let state = if new_val { "ON" } else { "OFF" };
            app.status_msg = Some((
                format!("auto-handoff {state} (runtime override)"),
                Instant::now(),
            ));
        }
        KeyCode::Char('g') => {
            let new_val = !app.auto_commit();
            app.auto_commit_override = Some(new_val);
            let state = if new_val { "ON" } else { "OFF" };
            app.status_msg = Some((
                format!("git auto-commit {state} (runtime override)"),
                Instant::now(),
            ));
        }
        KeyCode::Char('d') => app.show_detail = !app.show_detail,
        KeyCode::Char('i') => app.toggle_idle(),
        KeyCode::Char('1') => app.tab = Tab::Sessions,
        KeyCode::Char('2') => app.tab = Tab::Handoffs,
        _ => {}
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &mut App) {
    // Fill background
    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(5),    // body (3 columns)
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    render_header(f, app, chunks[0]);

    match app.tab {
        Tab::Sessions => {
            // 3-column layout
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(32), // left: sessions list
                    Constraint::Min(40),    // center: session detail
                    Constraint::Length(38), // right: metrics + logs
                ])
                .margin(0)
                .split(chunks[1]);

            render_sessions_list(f, app, body[0]);
            render_center_panel(f, app, body[1]);
            render_right_panel(f, app, body[2]);
        }
        Tab::Handoffs => render_handoffs(f, app, chunks[1]),
    }

    render_footer(f, app, chunks[2]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let mut running = 0u16;
    let mut waiting = 0u16;
    let mut idle_n = 0u16;
    let mut err_n = 0u16;
    for (info, p) in &app.sessions {
        match app.session_state(info, p) {
            session::SessionState::Starting | session::SessionState::Working => running += 1,
            session::SessionState::Waiting => waiting += 1,
            session::SessionState::Ended => err_n += 1,
            session::SessionState::Idle => idle_n += 1,
        }
    }
    let agg_cost: f64 = app
        .sessions
        .iter()
        .map(|(info, p)| {
            app.statuses
                .get(&info.session_id)
                .map(|s| s.cost_usd)
                .unwrap_or_else(|| estimate_cost(p))
        })
        .sum();
    let total_tokens: u64 = app
        .sessions
        .iter()
        .map(|(_, p)| p.total_input_tokens + p.total_output_tokens + p.total_cache_read)
        .sum();

    let now = chrono::Local::now().format("%H:%M:%S").to_string();

    let (auto_indicator, auto_color) = if app.auto_handoff() {
        (format!("auto {}%", app.config.threshold), GREEN)
    } else {
        ("auto OFF".to_string(), RED)
    };

    let line = Line::from(vec![
        Span::styled(
            "▞ relay",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(DIMMER),
        ),
        Span::styled(" │ ", Style::default().fg(DIMMER)),
        Span::styled("session monitor", Style::default().fg(DIM)),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("{}", running),
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" running  ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}", waiting),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" waiting  ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}", idle_n),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" idle  ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}", err_n),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ended", Style::default().fg(DIM)),
        Span::styled("    ", Style::default()),
        Span::styled(auto_indicator, Style::default().fg(auto_color)),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("Σ {} tok", format_tokens(total_tokens)),
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!("  ${:.2} total", agg_cost),
            Style::default().fg(DIM),
        ),
    ]);

    // Right-align clock
    let line_len: usize = line.spans.iter().map(|s| s.content.len()).sum();
    let pad = (area.width as usize).saturating_sub(line_len + now.len() + 2);
    let mut spans = line.spans;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(now, Style::default().fg(DIM)));

    let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_ALT));
    f.render_widget(header, area);
}

// ── Left column: sessions list ──────────────────────────────────────────────

fn render_sessions_list(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Left;
    let border_color = if focused { GREEN_DIM } else { BORDER };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(vec![
            Span::styled("[1]", Style::default().fg(DIMMER)),
            Span::raw(" "),
            Span::styled(
                format!("sessions · {}", app.visible_count()),
                Style::default().fg(if focused { GREEN } else { DIM }),
            ),
        ]))
        .style(Style::default().bg(BG_ALT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 || inner.width < 10 {
        return;
    }

    let dead_sessions = &app.dead_sessions;
    let hide_idle = app.hide_idle;
    let selected_idx = app.table_state.selected().unwrap_or(0);

    let visible: Vec<(&SessionInfo, &ParsedSession)> = app
        .sessions
        .iter()
        .filter(|(_, p)| !hide_idle || p.age_secs < 300)
        .map(|(i, p)| (i, p))
        .collect();

    // Each session entry is 3 lines + 1 separator = 4 lines per item
    let lines_per = 4u16;
    let scroll_offset = {
        let max_visible = inner.height / lines_per;
        if max_visible == 0 {
            0
        } else if selected_idx as u16 >= max_visible {
            (selected_idx as u16 - max_visible + 1) as usize
        } else {
            0
        }
    };

    let mut lines: Vec<Line> = Vec::new();

    for (idx, (info, parsed)) in visible.iter().enumerate() {
        if idx < scroll_offset {
            continue;
        }
        if lines.len() as u16 >= inner.height {
            break;
        }

        let is_selected = idx == selected_idx;
        let state = if info.session_id.starts_with("starting-") {
            session::SessionState::Starting
        } else {
            let process_dead = dead_sessions.contains(&info.session_id);
            let cpu_active = app.pmap.is_cpu_active(&info.jsonl_path);
            session::classify_state(parsed.age_secs, process_dead, cpu_active)
        };
        let (dot, dot_color) = match state {
            session::SessionState::Starting => ("◌", CYAN),
            session::SessionState::Working => ("●", GREEN),
            session::SessionState::Waiting => ("◐", AMBER),
            session::SessionState::Ended => ("✕", RED),
            session::SessionState::Idle => ("○", DIMMER),
        };

        let name_color = if is_selected { GREEN_GLOW } else { FG };
        let bg = if is_selected { BG_SELECT } else { BG_ALT };

        // Row 1: dot + name + pid
        let pid_label = if info.session_id.starts_with("starting-") {
            "…".to_string()
        } else {
            format!("#{}", util::short_session_id(&info.session_id))
        };
        let max_name = (inner.width as usize).saturating_sub(pid_label.len() + 5);
        let name: String = info.project_name.chars().take(max_name).collect();

        let sel_marker = if is_selected { "▸" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(sel_marker, Style::default().fg(GREEN).bg(bg)),
            Span::styled(format!("{} ", dot), Style::default().fg(dot_color).bg(bg)),
            Span::styled(
                name,
                Style::default()
                    .fg(name_color)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", pid_label),
                Style::default().fg(DIMMER).bg(bg),
            ),
        ]));

        // Row 2: status label + cwd
        let status_label = match state {
            session::SessionState::Starting => "starting",
            session::SessionState::Working => "running",
            session::SessionState::Waiting => "waiting",
            session::SessionState::Ended => "ended",
            session::SessionState::Idle => "idle",
        };
        let status_color = match state {
            session::SessionState::Starting => CYAN,
            session::SessionState::Working => GREEN,
            session::SessionState::Waiting => AMBER,
            session::SessionState::Ended => RED,
            session::SessionState::Idle => DIMMER,
        };
        let cwd_display = if !parsed.cwd.is_empty() {
            let c = parsed.cwd.replace(
                &dirs::home_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                "~",
            );
            let max_cwd = (inner.width as usize).saturating_sub(status_label.len() + 6);
            if c.len() > max_cwd {
                format!("…{}", &c[c.len().saturating_sub(max_cwd)..])
            } else {
                c
            }
        } else {
            "—".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().bg(bg)),
            Span::styled(status_label, Style::default().fg(status_color).bg(bg)),
            Span::styled(" · ", Style::default().fg(DIMMER).bg(bg)),
            Span::styled(cwd_display, Style::default().fg(DIMMER).bg(bg)),
        ]));

        // Row 3: elapsed + sparkline + tokens
        let age = util::format_duration(parsed.age_secs);
        let spark_w = (inner.width as usize).saturating_sub(age.len() + 10);
        let spark = mini_sparkline(&parsed.context_history, spark_w.min(12));
        let total_tok =
            parsed.total_input_tokens + parsed.total_output_tokens + parsed.total_cache_read;
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().bg(bg)),
            Span::styled(age, Style::default().fg(DIM).bg(bg)),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                spark,
                Style::default()
                    .fg(if is_selected { GREEN } else { GREEN_DIM })
                    .bg(bg),
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                format!("{}k", total_tok / 1000),
                Style::default().fg(DIM).bg(bg),
            ),
        ]));

        // Separator
        if lines.len() < inner.height as usize {
            lines.push(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(BORDER_SOFT),
            )));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// ── Center column: session detail ────────────────────────────────────────────

fn render_center_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Center;
    let border_color = if focused { GREEN_DIM } else { BORDER };

    let selected = app.selected_session_data();
    let (info, parsed) = match selected {
        Some((i, p)) => (i, p),
        None => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Line::from(vec![
                    Span::styled("[2]", Style::default().fg(DIMMER)),
                    Span::raw(" "),
                    Span::styled("session", Style::default().fg(DIM)),
                ]))
                .style(Style::default().bg(BG_ALT));
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(
                Paragraph::new("  no session selected").style(Style::default().fg(DIMMER)),
                inner,
            );
            return;
        }
    };

    let status = app.statuses.get(&info.session_id);
    let state = app.session_state(&info, &parsed);

    // Model display
    let model_display = status
        .filter(|s| !s.model_name.is_empty())
        .map(|s| s.model_name.clone())
        .unwrap_or_else(|| parsed.model.replace("claude-", ""));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(vec![
            Span::styled("[2]", Style::default().fg(DIMMER)),
            Span::raw(" "),
            Span::styled(
                format!("session · {}", info.project_name),
                Style::default().fg(if focused { GREEN } else { DIM }),
            ),
            Span::styled(
                format!(
                    "   {} · {}",
                    util::short_session_id(&info.session_id),
                    model_display
                ),
                Style::default().fg(DIMMER),
            ),
        ]))
        .style(Style::default().bg(BG_ALT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 6 || inner.width < 20 {
        return;
    }

    // Subdivide center: detail head (3) + KPIs (3) + tool timeline (flex) + files/messages (flex)
    let has_messages = !parsed.user_messages.is_empty() || !parsed.assistant_messages.is_empty();
    let bottom_h = if has_messages && inner.height > 20 {
        (inner.height / 3).max(5)
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),        // detail head
            Constraint::Length(3),        // KPIs strip
            Constraint::Min(4),           // tool timeline
            Constraint::Length(bottom_h), // files/messages
        ])
        .split(inner);

    render_detail_head(f, &info, &parsed, status, state, chunks[0]);
    render_kpis(f, &parsed, status, chunks[1]);
    render_tool_timeline(f, &parsed, chunks[2]);
    if bottom_h > 0 {
        render_files_messages(f, &parsed, chunks[3]);
    }
}

fn render_detail_head(
    f: &mut Frame,
    info: &SessionInfo,
    parsed: &ParsedSession,
    status: Option<&SessionStatus>,
    state: session::SessionState,
    area: Rect,
) {
    let (dot, dot_color) = match state {
        session::SessionState::Starting => ("◌", CYAN),
        session::SessionState::Working => ("●", GREEN),
        session::SessionState::Waiting => ("◐", AMBER),
        session::SessionState::Ended => ("✕", RED),
        session::SessionState::Idle => ("○", DIMMER),
    };
    let status_label = match state {
        session::SessionState::Starting => "starting",
        session::SessionState::Working => "running",
        session::SessionState::Waiting => "waiting",
        session::SessionState::Ended => "ended",
        session::SessionState::Idle => "idle",
    };
    let status_color = match state {
        session::SessionState::Starting => CYAN,
        session::SessionState::Working => GREEN,
        session::SessionState::Waiting => AMBER,
        session::SessionState::Ended => RED,
        session::SessionState::Idle => DIMMER,
    };

    let mut lines = Vec::new();

    // Name + status
    lines.push(Line::from(vec![
        Span::styled(format!(" {} ", dot), Style::default().fg(dot_color)),
        Span::styled(
            info.project_name.clone(),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(DIMMER)),
        Span::styled(status_label, Style::default().fg(status_color)),
    ]));

    // Path + branch + msg count + warnings
    let cwd_display = parsed.cwd.replace(
        &dirs::home_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        "~",
    );
    let has_branch = !parsed.git_branch.is_empty() && parsed.git_branch != "HEAD";
    let mut path_spans = vec![Span::styled(
        format!(" {}", cwd_display),
        Style::default().fg(DIMMER),
    )];
    if has_branch {
        path_spans.push(Span::styled(" on ", Style::default().fg(DIMMER)));
        path_spans.push(Span::styled(
            format!("⎇ {}", parsed.git_branch),
            Style::default().fg(AMBER),
        ));
    }
    path_spans.push(Span::styled(
        format!(" · msg #{}", parsed.turn_count),
        Style::default().fg(DIMMER),
    ));

    // Warnings & errors from status
    let lines_added = status.map(|s| s.lines_added).unwrap_or(0);
    let lines_removed = status.map(|s| s.lines_removed).unwrap_or(0);
    if lines_added > 0 || lines_removed > 0 {
        path_spans.push(Span::styled(
            format!(" · +{}/-{}", lines_added, lines_removed),
            Style::default().fg(DIM),
        ));
    }

    lines.push(Line::from(path_spans));

    f.render_widget(Paragraph::new(lines), area);
}

fn render_kpis(f: &mut Frame, parsed: &ParsedSession, status: Option<&SessionStatus>, area: Rect) {
    if area.width < 20 || area.height < 2 {
        return;
    }

    // Compute KPI values
    let (_pct, window) = if let Some(s) = status {
        let p = s.context_used_pct.unwrap_or(0.0);
        let w = s.context_window_size.unwrap_or(200_000);
        (p, w)
    } else {
        let w = util::context_window(&parsed.model, parsed.current_context_tokens);
        let p = if w > 0 {
            parsed.current_context_tokens as f64 / w as f64 * 100.0
        } else {
            0.0
        };
        (p, w)
    };

    let cost = status
        .map(|s| s.cost_usd)
        .unwrap_or_else(|| estimate_cost(parsed));

    let age = util::format_duration(parsed.age_secs);

    let tools_count = parsed.tool_uses.len();
    let tools_min = if parsed.age_secs > 0 {
        tools_count as f64 / (parsed.age_secs as f64 / 60.0)
    } else {
        0.0
    };

    // Current context tokens (not cumulative total)
    let current_ctx = status
        .map(|s| s.current_input + s.cache_read + s.cache_create)
        .filter(|&t| t > 0)
        .unwrap_or(parsed.current_context_tokens);

    // Separator line at top
    let sep_line = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(BORDER),
    ));
    f.render_widget(
        Paragraph::new(vec![sep_line.clone()]),
        Rect { height: 1, ..area },
    );

    let kpi_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    let kpi_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(kpi_area);

    // Tokens KPI
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(" TOKENS", Style::default().fg(DIMMER))),
            Line::from(vec![
                Span::styled(
                    format!(" {}", format_tokens(current_ctx)),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" / {}", format_tokens(window)),
                    Style::default().fg(DIM),
                ),
            ]),
        ]),
        kpi_cols[0],
    );

    // Cost KPI
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(" COST", Style::default().fg(DIMMER))),
            Line::from(Span::styled(
                format!(" ${:.2}", cost),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )),
        ]),
        kpi_cols[1],
    );

    // Elapsed KPI
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(" ELAPSED", Style::default().fg(DIMMER))),
            Line::from(Span::styled(
                format!(" {}", age),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )),
        ]),
        kpi_cols[2],
    );

    // Tools/min KPI
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(" TOOLS/MIN", Style::default().fg(DIMMER))),
            Line::from(Span::styled(
                format!(" {:.1}", tools_min),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )),
        ]),
        kpi_cols[3],
    );
}

fn render_tool_timeline(f: &mut Frame, parsed: &ParsedSession, area: Rect) {
    if area.height < 2 {
        return;
    }

    // Title line
    let title = Line::from(vec![Span::styled(
        " TOOL CALLS · LIVE",
        Style::default().fg(DIM),
    )]);
    f.render_widget(Paragraph::new(vec![title]), Rect { height: 1, ..area });

    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    let max_tools = body.height as usize;
    let mut lines: Vec<Line> = Vec::new();

    for tool in parsed.tool_uses.iter().rev().take(max_tools) {
        let tool_color = match tool.name.as_str() {
            "Edit" | "Write" => AMBER,
            "Bash" => GREEN,
            "Read" => DIM,
            "Grep" => CYAN,
            "Glob" => VIOLET,
            _ => DIM,
        };
        let icon = match tool.name.as_str() {
            "Read" => "•",
            "Grep" => "◈",
            "Glob" => "✦",
            "Edit" => "✎",
            "Write" => "＋",
            "Bash" => "$",
            _ => "·",
        };

        let max_arg = (body.width as usize).saturating_sub(14);
        let summary: String = tool.input_summary.chars().take(max_arg).collect();
        let short = summary.rsplit('/').next().unwrap_or(&summary).to_string();

        let ts = tool
            .timestamp
            .as_deref()
            .and_then(|t| t.get(t.len().saturating_sub(8)..))
            .unwrap_or("        ");

        lines.push(Line::from(vec![
            Span::styled(" · ", Style::default().fg(DIMMER)),
            Span::styled(format!("{} ", ts), Style::default().fg(DIMMER)),
            Span::styled(format!("{:<5}", tool.name), Style::default().fg(tool_color)),
            Span::styled(format!(" {} ", icon), Style::default().fg(DIMMER)),
            Span::styled(short, Style::default().fg(FG)),
        ]));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " no tool calls yet",
            Style::default().fg(DIMMER),
        )));
    }

    f.render_widget(Paragraph::new(lines), body);
}

fn render_files_messages(f: &mut Frame, parsed: &ParsedSession, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Files panel
    {
        let title = Line::from(vec![Span::styled(
            format!(" FILES CHANGED · {}", parsed.files_touched.len()),
            Style::default().fg(DIM),
        )]);
        let body_area = Rect {
            y: cols[0].y + 1,
            height: cols[0].height.saturating_sub(1),
            ..cols[0]
        };
        f.render_widget(
            Paragraph::new(vec![title]),
            Rect {
                height: 1,
                ..cols[0]
            },
        );

        let max_files = body_area.height as usize;
        let mut file_lines: Vec<Line> = Vec::new();
        for path in parsed.files_touched.iter().rev().take(max_files) {
            let display = if !parsed.cwd.is_empty() && path.starts_with(&parsed.cwd) {
                path.strip_prefix(&parsed.cwd)
                    .unwrap_or(path)
                    .trim_start_matches('/')
                    .to_string()
            } else {
                path.rsplit('/').next().unwrap_or(path).to_string()
            };
            let max_w = (body_area.width as usize).saturating_sub(5);
            let display: String = display.chars().take(max_w).collect();

            // Determine file action from tool uses
            let is_new = parsed
                .tool_uses
                .iter()
                .any(|t| t.name == "Write" && t.input_summary.contains(&display));
            let marker = if is_new { "A" } else { "M" };
            let marker_color = if is_new { GREEN } else { AMBER };

            file_lines.push(Line::from(vec![
                Span::styled(format!(" {} ", marker), Style::default().fg(marker_color)),
                Span::styled(display, Style::default().fg(FG)),
            ]));
        }
        if file_lines.is_empty() {
            file_lines.push(Line::from(Span::styled(
                " no files yet",
                Style::default().fg(DIMMER),
            )));
        }
        f.render_widget(Paragraph::new(file_lines), body_area);
    }

    // Messages panel
    {
        let msg_count = parsed.user_messages.len() + parsed.assistant_messages.len();
        let title = Line::from(vec![Span::styled(
            format!(" PROMPTS · {}", msg_count),
            Style::default().fg(DIM),
        )]);
        let body_area = Rect {
            y: cols[1].y + 1,
            height: cols[1].height.saturating_sub(1),
            ..cols[1]
        };
        f.render_widget(
            Paragraph::new(vec![title]),
            Rect {
                height: 1,
                ..cols[1]
            },
        );

        let max_msgs = body_area.height as usize;
        let mut msg_lines: Vec<Line> = Vec::new();

        // Pair user[i] / assistant[i] — each user prompt followed by its response
        let max_pairs = parsed
            .user_messages
            .len()
            .max(parsed.assistant_messages.len());
        let mut all_msgs: Vec<(bool, &str)> = Vec::new();
        for i in 0..max_pairs {
            if let Some(msg) = parsed.user_messages.get(i) {
                all_msgs.push((true, &msg.content));
            }
            if let Some(msg) = parsed.assistant_messages.get(i) {
                all_msgs.push((false, &msg.content));
            }
        }

        // Show the last N, oldest on top
        let start = all_msgs.len().saturating_sub(max_msgs);
        for (is_user, content) in &all_msgs[start..] {
            let max_w = (body_area.width as usize).saturating_sub(10);
            let short: String = content.chars().take(max_w).collect();
            let (label, color, text_color) = if *is_user {
                ("user  ", CYAN, DIM)
            } else {
                ("claude", GREEN, FG)
            };
            msg_lines.push(Line::from(vec![
                Span::styled(format!(" {} ", label), Style::default().fg(color)),
                Span::styled(short, Style::default().fg(text_color)),
            ]));
        }
        if msg_lines.is_empty() {
            msg_lines.push(Line::from(Span::styled(
                " no messages yet",
                Style::default().fg(DIMMER),
            )));
        }
        f.render_widget(Paragraph::new(msg_lines), body_area);
    }
}

// ── Right column: metrics + budget + logs ───────────────────────────────────

fn render_right_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Right;
    let border_color = if focused { GREEN_DIM } else { BORDER };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(vec![
            Span::styled("[3]", Style::default().fg(DIMMER)),
            Span::raw(" "),
            Span::styled(
                "metrics · live",
                Style::default().fg(if focused { GREEN } else { DIM }),
            ),
        ]))
        .style(Style::default().bg(BG_ALT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 4 || inner.width < 10 {
        return;
    }

    let selected = app.selected_session_data();
    let (info, parsed) = match &selected {
        Some((i, p)) => (i, p),
        None => {
            f.render_widget(
                Paragraph::new("  no session").style(Style::default().fg(DIMMER)),
                inner,
            );
            return;
        }
    };

    let status = app.statuses.get(&info.session_id);

    // Layout: metrics (6) + budget (6) + logs (rest)
    let metrics_h = 8u16.min(inner.height / 3);
    let budget_h = 6u16.min(inner.height / 3);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(metrics_h), // metrics
            Constraint::Length(budget_h),  // budget
            Constraint::Min(3),            // activity log
        ])
        .split(inner);

    render_metrics_section(f, parsed, status, chunks[0]);
    render_budget_section(f, parsed, status, chunks[1]);
    render_activity_log(f, parsed, chunks[2]);
}

fn render_metrics_section(
    f: &mut Frame,
    parsed: &ParsedSession,
    status: Option<&SessionStatus>,
    area: Rect,
) {
    let mut lines: Vec<Line> = Vec::new();

    // tokens/s
    let total_tok =
        parsed.total_input_tokens + parsed.total_output_tokens + parsed.total_cache_read;
    let tok_sec = total_tok.checked_div(parsed.age_secs).unwrap_or(0);

    lines.push(Line::from(vec![
        Span::styled(
            " tokens / s",
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  throughput", Style::default().fg(DIMMER)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", tok_sec),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" tok/s", Style::default().fg(DIM)),
    ]));

    // Sparkline for context history
    let spark_w = (area.width as usize).saturating_sub(2);
    let spark = mini_sparkline(&parsed.context_history, spark_w);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(spark, Style::default().fg(GREEN_DIM)),
    ]));

    lines.push(Line::from(""));

    // Usage (quota bars)
    lines.push(Line::from(vec![Span::styled(
        " Usage",
        Style::default().fg(FG).add_modifier(Modifier::BOLD),
    )]));

    if let Some(s) = status {
        let bar_w = (area.width as usize).saturating_sub(14).min(12);
        if let Some(used) = s.five_hour_used_pct {
            let color = usage_color(used);
            let bar = usage_bar(used, bar_w);
            let reset = s.five_hour_resets_at.map(format_reset).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(" 5h  ", Style::default().fg(DIM)),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(format!(" {:>3.0}%", used), Style::default().fg(color)),
                Span::styled(reset, Style::default().fg(DIMMER)),
            ]));
        }
        if let Some(used) = s.seven_day_used_pct {
            let color = usage_color(used);
            let bar = usage_bar(used, bar_w);
            let reset = s.seven_day_resets_at.map(format_reset).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(" 7d  ", Style::default().fg(DIM)),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(format!(" {:>3.0}%", used), Style::default().fg(color)),
                Span::styled(reset, Style::default().fg(DIMMER)),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            " no live data",
            Style::default().fg(DIMMER),
        )));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn render_budget_section(
    f: &mut Frame,
    parsed: &ParsedSession,
    status: Option<&SessionStatus>,
    area: Rect,
) {
    let mut lines: Vec<Line> = Vec::new();

    // Context window bar
    let (pct, window) = if let Some(s) = status {
        let p = s.context_used_pct.unwrap_or(0.0);
        let w = s.context_window_size.unwrap_or(200_000);
        (p, w)
    } else {
        let w = util::context_window(&parsed.model, parsed.current_context_tokens);
        let p = if w > 0 {
            parsed.current_context_tokens as f64 / w as f64 * 100.0
        } else {
            0.0
        };
        (p, w)
    };

    // Current context tokens (what's actually in the window now)
    let current_ctx = status
        .map(|s| s.current_input + s.cache_read + s.cache_create)
        .filter(|&t| t > 0)
        .unwrap_or(parsed.current_context_tokens);

    let pct_color = if pct > 80.0 {
        RED
    } else if pct > 60.0 {
        AMBER
    } else {
        GREEN
    };

    lines.push(Line::from(vec![
        Span::styled(" context window", Style::default().fg(DIM)),
        Span::styled(
            format!("  {}k / {}k", current_ctx / 1000, window / 1000),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {:.0}%", pct),
            Style::default().fg(pct_color).add_modifier(Modifier::BOLD),
        ),
    ]));

    // Bar
    let bar_w = (area.width as usize).saturating_sub(4).min(30);
    let filled = ((pct / 100.0) * bar_w as f64).round() as usize;
    let empty = bar_w.saturating_sub(filled);
    let bar_color = if pct > 80.0 {
        RED
    } else if pct > 60.0 {
        AMBER
    } else {
        GREEN_DIM
    };
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
        Span::styled("░".repeat(empty), Style::default().fg(BORDER)),
    ]));

    // Cost bar
    let cost = status
        .map(|s| s.cost_usd)
        .unwrap_or_else(|| estimate_cost(parsed));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" session cost", Style::default().fg(DIM)),
        Span::styled(
            format!("  ${:.2}", cost),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
    ]));

    // Compactions
    if parsed.compaction_count > 0 {
        lines.push(Line::from(Span::styled(
            format!(" {} compactions", parsed.compaction_count),
            Style::default().fg(AMBER),
        )));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn render_activity_log(f: &mut Frame, parsed: &ParsedSession, area: Rect) {
    // Title
    let title = Line::from(vec![
        Span::styled(" stream · activity", Style::default().fg(DIM)),
        Span::styled("  ●", Style::default().fg(GREEN)),
        Span::styled(" live", Style::default().fg(DIMMER)),
    ]);
    f.render_widget(Paragraph::new(vec![title]), Rect { height: 1, ..area });

    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    let max_lines = body.height as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Show tool activity as log entries
    for tool in parsed.tool_uses.iter().rev().take(max_lines) {
        let ts = tool
            .timestamp
            .as_deref()
            .and_then(|t| t.get(t.len().saturating_sub(8)..))
            .unwrap_or("        ");

        let level_color = match tool.name.as_str() {
            "Edit" | "Write" => AMBER,
            "Bash" => GREEN,
            _ => CYAN,
        };

        let max_msg = (body.width as usize).saturating_sub(16);
        let summary: String = tool.input_summary.chars().take(max_msg).collect();

        lines.push(Line::from(vec![
            Span::styled(format!(" {}", ts), Style::default().fg(DIMMER)),
            Span::styled(
                format!(
                    " {:<4}",
                    tool.name.to_uppercase().get(..4).unwrap_or(&tool.name)
                ),
                Style::default().fg(level_color),
            ),
            Span::styled(format!(" {}", summary), Style::default().fg(FG)),
        ]));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " waiting for activity…",
            Style::default().fg(DIMMER),
        )));
    }

    f.render_widget(Paragraph::new(lines), body);
}

// ── Handoffs tab ────────────────────────────────────────────────────────────

fn render_handoffs(f: &mut Frame, app: &mut App, area: Rect) {
    if app.handoffs.is_empty() {
        let empty =
            Paragraph::new("  No handoffs saved yet. Press 's' in Sessions tab to create one.")
                .style(Style::default().fg(DIM))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(BORDER))
                        .title(" handoffs "),
                );
        f.render_widget(empty, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    let items: Vec<ListItem> = app
        .handoffs
        .iter()
        .map(|h| {
            ListItem::new(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(&h.id, Style::default().fg(CYAN)),
                Span::raw("  "),
                Span::styled(
                    format!("{:.1}kb", h.size as f64 / 1024.0),
                    Style::default().fg(DIM),
                ),
                Span::raw("  "),
                Span::styled(&h.preview, Style::default().fg(DIM)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .title(format!(" {} handoffs ", app.handoffs.len())),
        )
        .highlight_style(Style::default().bg(BG_SELECT).add_modifier(Modifier::BOLD))
        .highlight_symbol(" ▸");

    f.render_stateful_widget(list, chunks[0], &mut app.handoff_state);

    let preview_content = app
        .handoff_state
        .selected()
        .and_then(|i| app.handoffs.get(i))
        .map(|h| h.content.clone())
        .unwrap_or_default();

    let preview = Paragraph::new(preview_content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .title(" preview "),
        )
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(FG));

    f.render_widget(preview, chunks[1]);
}

// ── Footer ──────────────────────────────────────────────────────────────────

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some((ref msg, at)) = app.status_msg {
        if at.elapsed() < Duration::from_secs(4) {
            let line = Line::from(Span::styled(format!(" {msg}"), Style::default().fg(GREEN)));
            f.render_widget(
                Paragraph::new(line).style(Style::default().bg(BG_ALT)),
                area,
            );
            return;
        }
    }

    let keys: &[(&str, &str)] = &[
        ("j/k", "select"),
        ("h/l", "panel"),
        ("s", "save"),
        ("a", "auto"),
        ("g", "git"),
        ("i", "idle"),
        ("tab", "switch"),
        ("r", "refresh"),
        ("q", "quit"),
    ];

    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(
            format!(" {} ", key),
            Style::default().fg(FG).bg(Color::Rgb(20, 20, 20)),
        ));
        spans.push(Span::styled(
            format!("{} ", label),
            Style::default().fg(DIM),
        ));
    }

    let right_text = format!("v{} │ github.com/LuluHow/relay ", env!("CARGO_PKG_VERSION"));
    let spans_len: usize = spans.iter().map(|s| s.content.len()).sum();
    let pad = (area.width as usize).saturating_sub(spans_len + right_text.len());
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right_text, Style::default().fg(DIMMER)));

    let line = Line::from(spans);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(BG_ALT)),
        area,
    );
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn usage_color(used: f64) -> Color {
    if used < 50.0 {
        GREEN
    } else if used < 80.0 {
        AMBER
    } else {
        RED
    }
}

fn usage_bar(used: f64, width: usize) -> String {
    let w = width.clamp(2, 20);
    let filled = ((used / 100.0) * w as f64).round() as usize;
    let empty = w.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

#[allow(dead_code)]
fn format_reset(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if ts <= now {
        return String::new();
    }
    let diff = ts - now;
    if diff < 60 {
        format!(" {}s", diff)
    } else if diff < 3600 {
        format!(" {}m", diff / 60)
    } else {
        format!(" {}h{}m", diff / 3600, (diff % 3600) / 60)
    }
}

fn mini_sparkline(data: &[u64], width: usize) -> String {
    if data.is_empty() {
        return "·".repeat(width);
    }
    let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let points: Vec<u64> = if data.len() > width {
        data[data.len() - width..].to_vec()
    } else {
        let mut v = vec![0u64; width - data.len()];
        v.extend_from_slice(data);
        v
    };
    let max = *points.iter().max().unwrap_or(&1);
    let max = max.max(1);
    points
        .iter()
        .map(|&v| {
            if v == 0 {
                ' '
            } else {
                let idx = ((v as f64 / max as f64) * 7.0) as usize;
                bars[idx.min(7)]
            }
        })
        .collect()
}

fn estimate_cost(parsed: &ParsedSession) -> f64 {
    let m = parsed.model.to_lowercase();
    let (in_rate, out_rate, cache_rate) = if m.contains("opus") {
        (15.0, 75.0, 1.5)
    } else if m.contains("haiku") {
        (0.80, 4.0, 0.08)
    } else {
        (3.0, 15.0, 0.30)
    };

    let in_cost = parsed.total_input_tokens as f64 / 1_000_000.0 * in_rate;
    let out_cost = parsed.total_output_tokens as f64 / 1_000_000.0 * out_rate;
    let cache_cost = parsed.total_cache_read as f64 / 1_000_000.0 * cache_rate;
    let create_cost = parsed.total_cache_create as f64 / 1_000_000.0 * in_rate * 1.25;

    in_cost + out_cost + cache_cost + create_cost
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

// ════════════════════════════════════════════════════════════════════════════
// ── Orchestration TUI ──────────────────────────────────────────────────────
// ════════════════════════════════════════════════════════════════════════════

use crate::orchestrator::{Orchestrator, Plan, TaskStatus};

struct OrchApp {
    orchestrator: Orchestrator,
    selected_task: usize,
    scroll_offset: u16,
    should_quit: bool,
    status_msg: Option<(String, Instant)>,
    finished: bool,
}

impl OrchApp {
    fn new(plan: Plan, project_root: std::path::PathBuf) -> Result<Self> {
        let mut orchestrator = Orchestrator::new(plan, project_root);
        orchestrator.setup()?;
        Ok(Self {
            orchestrator,
            selected_task: 0,
            scroll_offset: 0,
            should_quit: false,
            status_msg: None,
            finished: false,
        })
    }
}

pub fn run_orchestrate(plan: Plan, project_root: std::path::PathBuf) -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = OrchApp::new(plan, project_root)?;
    let result = run_orch_loop(&mut terminal, &mut app);

    // Cleanup on exit
    if !app.finished {
        app.orchestrator.abort();
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Post-exit: print summary
    if app.finished {
        let summary = app.orchestrator.generate_summary();
        println!("{summary}");

        let on_complete = &app.orchestrator.plan.plan.on_complete;
        match on_complete.as_str() {
            "merge" => {
                println!("Merging branch '{}'...", app.orchestrator.branch_name);
                match app.orchestrator.merge_branch() {
                    Ok(msg) => println!("  \u{2713} {msg}"),
                    Err(e) => println!("  \u{2717} {e}"),
                }
            }
            "pr" => {
                println!("Creating pull request...");
                match app.orchestrator.create_pull_request() {
                    Ok(url) => println!("  \u{2713} {url}"),
                    Err(e) => println!("  \u{2717} {e}"),
                }
            }
            _ => {
                println!(
                    "Branch '{}' ready. Use `git checkout {}` to review, then merge manually.",
                    app.orchestrator.branch_name, app.orchestrator.branch_name
                );
            }
        }

        // Always cleanup worktree — branch remains accessible via git checkout
        app.orchestrator.cleanup_worktree();
    } else {
        app.orchestrator.cleanup_worktree();
        println!("Orchestration aborted. Worktree cleaned up.");
    }

    result
}

fn run_orch_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut OrchApp,
) -> Result<()> {
    loop {
        terminal.draw(|f| orch_ui(f, app))?;

        if !app.finished {
            let all_done = app.orchestrator.tick();
            if all_done {
                app.finished = true;
                app.status_msg = Some(("all tasks completed".to_string(), Instant::now()));
            }
        }

        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(key) = event::read()? {
                orch_handle_key(app, key);
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn orch_handle_key(app: &mut OrchApp, key: event::KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Up | KeyCode::Char('k') if app.selected_task > 0 => {
            app.selected_task -= 1;
            app.scroll_offset = 0;
        }
        KeyCode::Down | KeyCode::Char('j')
            if app.selected_task + 1 < app.orchestrator.tasks.len() =>
        {
            app.selected_task += 1;
            app.scroll_offset = 0;
        }
        KeyCode::Char('a') if !app.finished => {
            app.orchestrator.abort();
            app.finished = true;
            app.status_msg = Some(("aborted all tasks".to_string(), Instant::now()));
        }
        _ => {}
    }
}

fn orch_ui(f: &mut Frame, app: &mut OrchApp) {
    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(5),    // body
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    orch_render_header(f, app, chunks[0]);

    // Body: left (task list) + right (task detail/output)
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(40), // task list
            Constraint::Min(40),    // task detail
        ])
        .split(chunks[1]);

    orch_render_task_list(f, app, body[0]);
    orch_render_task_detail(f, app, body[1]);
    orch_render_footer(f, app, chunks[2]);
}

fn orch_render_header(f: &mut Frame, app: &OrchApp, area: Rect) {
    let (pending, blocked, running, done, failed) = app.orchestrator.counts();
    let elapsed = crate::util::format_duration(app.orchestrator.elapsed().as_secs());

    let perms_indicator = if app.orchestrator.plan.plan.skip_permissions {
        Span::styled(" skip-perms ", Style::default().fg(RED))
    } else {
        Span::raw("")
    };

    let line = Line::from(vec![
        Span::styled(
            "▞ relay",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(DIMMER)),
        Span::styled("orchestrate", Style::default().fg(VIOLET)),
        Span::styled(
            format!(" · {}", app.orchestrator.plan.plan.name),
            Style::default().fg(FG),
        ),
        Span::styled(
            format!(" ({})", app.orchestrator.branch_name),
            Style::default().fg(DIMMER),
        ),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("{done}"),
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" done  ", Style::default().fg(DIM)),
        Span::styled(
            format!("{running}"),
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" running  ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}", pending + blocked),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" pending  ", Style::default().fg(DIM)),
        if failed > 0 {
            Span::styled(
                format!("{failed} failed  "),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        },
        perms_indicator,
        Span::styled(format!("  {elapsed}"), Style::default().fg(DIM)),
    ]);

    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(BG_ALT)),
        area,
    );
}

fn orch_render_task_list(f: &mut Frame, app: &OrchApp, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Line::from(vec![Span::styled(
            format!(" tasks · {}", app.orchestrator.tasks.len()),
            Style::default().fg(DIM),
        )]))
        .style(Style::default().bg(BG_ALT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    for (idx, task) in app.orchestrator.tasks.iter().enumerate() {
        let is_selected = idx == app.selected_task;
        let bg = if is_selected { BG_SELECT } else { BG_ALT };

        let (dot_color, status_color) = match task.status {
            TaskStatus::Pending => (DIM, DIM),
            TaskStatus::Blocked => (DIMMER, DIMMER),
            TaskStatus::Running => (CYAN, CYAN),
            TaskStatus::Done => (GREEN, GREEN),
            TaskStatus::Failed => (RED, RED),
        };

        let sel = if is_selected { "▸" } else { " " };

        // Elapsed for this task
        let task_elapsed = task
            .started_at
            .map(|s| {
                let dur = task
                    .finished_at
                    .unwrap_or_else(Instant::now)
                    .duration_since(s);
                crate::util::format_duration(dur.as_secs())
            })
            .unwrap_or_else(|| "—".to_string());

        // Row 1: status dot + name
        lines.push(Line::from(vec![
            Span::styled(sel, Style::default().fg(GREEN).bg(bg)),
            Span::styled(
                format!("{} ", task.status.symbol()),
                Style::default().fg(dot_color).bg(bg),
            ),
            Span::styled(
                task.def.name.clone(),
                Style::default()
                    .fg(if is_selected { GREEN_GLOW } else { FG })
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        // Row 2: status label + deps + elapsed
        let deps_str = if task.def.depends_on.is_empty() {
            String::new()
        } else {
            format!(" ← {}", task.def.depends_on.join(", "))
        };
        let max_deps = (inner.width as usize).saturating_sub(task_elapsed.len() + 14);
        let deps_display: String = deps_str.chars().take(max_deps).collect();

        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().bg(bg)),
            Span::styled(
                task.status.label().to_string(),
                Style::default().fg(status_color).bg(bg),
            ),
            Span::styled(deps_display, Style::default().fg(DIMMER).bg(bg)),
            Span::styled(
                format!("  {}", task_elapsed),
                Style::default().fg(DIM).bg(bg),
            ),
        ]));

        // Separator
        if (lines.len() as u16) < inner.height {
            lines.push(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(BORDER_SOFT),
            )));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn orch_render_task_detail(f: &mut Frame, app: &mut OrchApp, area: Rect) {
    let task = match app.orchestrator.tasks.get(app.selected_task) {
        Some(t) => t,
        None => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .title(" task detail ")
                .style(Style::default().bg(BG_ALT));
            f.render_widget(block, area);
            return;
        }
    };

    let title_color = match task.status {
        TaskStatus::Running => CYAN,
        TaskStatus::Done => GREEN,
        TaskStatus::Failed => RED,
        _ => DIM,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Line::from(vec![
            Span::styled(
                format!(" {} ", task.status.symbol()),
                Style::default().fg(title_color),
            ),
            Span::styled(
                task.def.name.clone(),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(Style::default().bg(BG_ALT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 4 {
        return;
    }

    // Split: prompt (3 lines) + separator + output (rest)
    let prompt_h = 3u16.min(inner.height / 3);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(prompt_h), // prompt preview
            Constraint::Length(1),        // separator
            Constraint::Min(3),           // stdout output
        ])
        .split(inner);

    // Prompt preview
    let prompt_lines: Vec<Line> = task
        .def
        .prompt
        .lines()
        .take(prompt_h as usize)
        .map(|l| {
            let max_w = chunks[0].width as usize;
            let display: String = l.chars().take(max_w).collect();
            Line::from(Span::styled(display, Style::default().fg(DIM)))
        })
        .collect();
    f.render_widget(Paragraph::new(prompt_lines), chunks[0]);

    // Separator
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(chunks[1].width as usize),
            Style::default().fg(BORDER),
        ))),
        chunks[1],
    );

    // Output (scrolled to bottom)
    let output_h = chunks[2].height as usize;
    let total_lines = task.output_lines.len();
    let start = total_lines.saturating_sub(output_h + app.scroll_offset as usize);
    let end = start + output_h;

    let output_lines: Vec<Line> = task
        .output_lines
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| {
            let max_w = chunks[2].width as usize;
            let display: String = l.chars().take(max_w).collect();
            let color = if l.starts_with("[stderr]") || l.starts_with("[relay error]") {
                RED
            } else {
                FG
            };
            Line::from(Span::styled(display, Style::default().fg(color)))
        })
        .collect();

    if output_lines.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                match task.status {
                    TaskStatus::Pending | TaskStatus::Blocked => " waiting to start...",
                    TaskStatus::Running => " running... output will appear here",
                    TaskStatus::Done => " completed (no output captured)",
                    TaskStatus::Failed => " failed (no output captured)",
                },
                Style::default().fg(DIMMER),
            ))),
            chunks[2],
        );
    } else {
        f.render_widget(Paragraph::new(output_lines), chunks[2]);
    }
}

fn orch_render_footer(f: &mut Frame, app: &OrchApp, area: Rect) {
    if let Some((ref msg, at)) = app.status_msg {
        if at.elapsed() < Duration::from_secs(4) {
            let line = Line::from(Span::styled(format!(" {msg}"), Style::default().fg(GREEN)));
            f.render_widget(
                Paragraph::new(line).style(Style::default().bg(BG_ALT)),
                area,
            );
            return;
        }
    }

    let keys: &[(&str, &str)] = if app.finished {
        &[("q", "quit")]
    } else {
        &[("j/k", "select"), ("a", "abort all"), ("q", "quit")]
    };

    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(Span::styled(
            format!(" {} ", key),
            Style::default().fg(FG).bg(Color::Rgb(20, 20, 20)),
        ));
        spans.push(Span::styled(
            format!("{} ", label),
            Style::default().fg(DIM),
        ));
    }

    let on_complete = &app.orchestrator.plan.plan.on_complete;
    let right = format!("on_complete: {on_complete}");
    let spans_len: usize = spans.iter().map(|s| s.content.len()).sum();
    let pad = (area.width as usize).saturating_sub(spans_len + right.len());
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, Style::default().fg(DIMMER)));

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_ALT)),
        area,
    );
}
