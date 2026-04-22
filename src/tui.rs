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

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

// ── Palette ─────────────────────────────────────────────────────────────────

const CYAN: Color = Color::Rgb(80, 200, 220);
const MAGENTA: Color = Color::Rgb(190, 120, 220);
const GREEN: Color = Color::Rgb(80, 220, 120);
const YELLOW: Color = Color::Rgb(230, 200, 80);
const RED: Color = Color::Rgb(240, 80, 80);
const WHITE: Color = Color::Rgb(220, 220, 230);
const DIM: Color = Color::Rgb(100, 100, 120);
const DIMMER: Color = Color::Rgb(60, 60, 75);
const BORDER: Color = Color::Rgb(55, 55, 70);
const BG_SELECT: Color = Color::Rgb(35, 35, 55);

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Sessions,
    Handoffs,
}

struct HandoffEntry {
    id: String,
    preview: String,
    content: String,
    size: u64,
}

struct App {
    tab: Tab,
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
    triggered_sessions: HashSet<String>,
    // Live data from statusLine hook (keyed by session_id)
    statuses: HashMap<String, SessionStatus>,
    // Process liveness cache: session IDs confirmed dead
    dead_sessions: HashSet<String>,
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
            triggered_sessions: HashSet::new(),
            statuses: HashMap::new(),
            dead_sessions: HashSet::new(),
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        if let Ok(sessions) = session::discover_sessions() {
            self.sessions = sessions
                .into_iter()
                .filter_map(|s| parser::parse_session(&s).ok().map(|p| (s, p)))
                .collect();

            // Discover running Claude processes (single lsof call)
            let claude_pids = session::discover_claude_pids();

            // Resurrect sessions that show new activity
            self.dead_sessions.retain(|sid| {
                self.sessions
                    .iter()
                    .any(|(info, parsed)| info.session_id == *sid && parsed.age_secs >= 5)
            });

            // Check process liveness for recent sessions
            for (info, parsed) in &self.sessions {
                if parsed.age_secs < 300 && !self.dead_sessions.contains(&info.session_id) {
                    if !session::is_session_alive(&info.jsonl_path, &claude_pids) {
                        self.dead_sessions.insert(info.session_id.clone());
                    }
                }
            }

            let dead = &self.dead_sessions;
            self.sessions.sort_by_key(|(info, p)| {
                let process_dead = dead.contains(&info.session_id);
                session::classify_state(p.age_secs, process_dead).sort_key()
            });
        }
        self.handoffs = load_handoffs();
        self.statuses = statusline::read_all();

        let vis_count = self.visible_count();
        if self.table_state.selected().map_or(true, |i| i >= vis_count) {
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
        let process_dead = self.dead_sessions.contains(&info.session_id);
        session::classify_state(parsed.age_secs, process_dead)
    }

    fn check_auto_handoff(&mut self) {
        if !self.config.auto_handoff {
            return;
        }

        // Find the first active session that triggers
        let trigger = self
            .sessions
            .iter()
            .filter(|(info, parsed)| {
                !self.triggered_sessions.contains(&info.session_id)
                    && self.session_state(info, parsed) == session::SessionState::Working
                    && parsed.turn_count > 2
            })
            .find(|(_, parsed)| {
                let pct = context_pct(parsed);
                let by_context = pct >= self.config.threshold;
                let by_turns =
                    self.config.max_turns > 0 && parsed.turn_count >= self.config.max_turns;
                by_context || by_turns
            })
            .map(|(info, parsed)| (info.clone(), parsed.clone()));

        if let Some((info, parsed)) = trigger {
            self.trigger_handoff(&info, &parsed);
        }
    }

    fn trigger_handoff(&mut self, info: &SessionInfo, parsed: &ParsedSession) {
        // Auto-commit before handoff if enabled
        if self.config.auto_commit && self.config.commit_before_handoff {
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
        let next_prompt_path = dirs::home_dir()
            .unwrap()
            .join(".relay")
            .join("next_prompt");
        if let Err(e) = std::fs::write(&next_prompt_path, &prompt) {
            self.status_msg = Some((format!("write next_prompt: {e}"), Instant::now()));
            return;
        }

        // Find and kill Claude
        let killed = if let Some(pid) = find_claude_pid(&info.jsonl_path) {
            kill_process(pid);
            true
        } else {
            false
        };

        // Notify
        if self.config.notify {
            #[cfg(target_os = "macos")]
            {
                let body = if killed {
                    format!("{reason} — restarting")
                } else {
                    format!("{reason} — handoff saved (could not find claude pid)")
                };
                let _ = notify_rust::Notification::new()
                    .summary("relay — auto-handoff")
                    .body(&body)
                    .sound_name("Ping")
                    .show();
            }
        }

        self.triggered_sessions.insert(info.session_id.clone());
        self.handoffs = load_handoffs();

        let status = if killed {
            format!("auto-handoff: {} — {id}", reason)
        } else {
            format!("handoff saved: {} — pid not found", reason)
        };
        self.status_msg = Some((status, Instant::now()));
    }

    // ── Git auto-commit ────────────────────────────────────────────────────

    fn check_auto_commit(&mut self) {
        if !self.config.auto_commit {
            return;
        }

        let events = hooks::read_stop_events();
        for event in events {
            // Find the matching session
            let session_data = self.sessions.iter().find(|(info, _)| {
                info.session_id == event.session_id
            });

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
            git::generate_commit_message(parsed, status, reason, &self.config.commit_prefix);

        match git::auto_commit(cwd, &message) {
            Ok(hash) => {
                self.status_msg =
                    Some((format!("git: committed {hash}"), Instant::now()));
            }
            Err(e) => {
                self.status_msg = Some((format!("git: {e}"), Instant::now()));
            }
        }
    }
}

// ── Auto-handoff helpers ────────────────────────────────────────────────────

fn context_pct(parsed: &ParsedSession) -> u8 {
    let window = context_window(&parsed.model, parsed.current_context_tokens);
    if window > 0 {
        (parsed.current_context_tokens as f64 / window as f64 * 100.0) as u8
    } else {
        0
    }
}

/// Find the Claude Code process that owns a session JSONL file.
fn find_claude_pid(jsonl_path: &std::path::Path) -> Option<u32> {
    // Try cwd-based matching via lsof
    let claude_pids = session::discover_claude_pids();
    if let Some(pid) = session::find_session_pid(jsonl_path, &claude_pids) {
        return Some(pid);
    }

    // Fallback: pgrep for claude, excluding our own PID
    let my_pid = std::process::id();
    let output = Command::new("pgrep")
        .args(["-f", "claude"])
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
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "md"))
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

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if app.last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh();
            app.check_auto_handoff();
            app.check_auto_commit();
        }

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
        KeyCode::Tab | KeyCode::BackTab => app.next_tab(),
        KeyCode::Char('s') => app.save_selected(),
        KeyCode::Char('r') => {
            app.refresh();
            app.status_msg = Some(("✓ Refreshed".to_string(), Instant::now()));
        }
        KeyCode::Char('a') => {
            app.config.auto_handoff = !app.config.auto_handoff;
            let state = if app.config.auto_handoff { "ON" } else { "OFF" };
            app.status_msg = Some((format!("auto-handoff {state}"), Instant::now()));
        }
        KeyCode::Char('g') => {
            app.config.auto_commit = !app.config.auto_commit;
            let state = if app.config.auto_commit { "ON" } else { "OFF" };
            app.status_msg = Some((format!("git auto-commit {state}"), Instant::now()));
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),   // content
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    render_header(f, app, chunks[0]);

    match app.tab {
        Tab::Sessions => render_sessions(f, app, chunks[1]),
        Tab::Handoffs => render_handoffs(f, app, chunks[1]),
    }

    render_footer(f, app, chunks[2]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let active = app.sessions.iter().filter(|(info, p)| {
        app.session_state(info, p).is_active()
    }).count();
    let idle = app.sessions.len() - active;
    let agg_cost: f64 = app.sessions.iter().map(|(info, p)| {
        app.statuses.get(&info.session_id)
            .map(|s| s.cost_usd)
            .unwrap_or_else(|| estimate_cost(p))
    }).sum();

    let idle_label = if app.hide_idle {
        format!("{idle} hidden")
    } else {
        format!("{idle} idle")
    };

    let auto_label = if app.config.auto_handoff {
        format!("│ auto {}% ", app.config.threshold)
    } else {
        String::new()
    };

    let git_label = if app.config.auto_commit {
        "│ git ✓ ".to_string()
    } else {
        String::new()
    };

    let now = chrono::Local::now().format("%H:%M").to_string();

    let title = format!(
        " ◉ relay  {} active · {} │ ~${:.2} total {auto_label}{git_label}│ {} ",
        active, idle_label, agg_cost, now,
    );

    let titles = vec!["Sessions", "Handoffs"];
    let selected = match app.tab {
        Tab::Sessions => 0,
        Tab::Handoffs => 1,
    };

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .title(Span::styled(
                    title,
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                )),
        )
        .select(selected)
        .style(Style::default().fg(DIM))
        .highlight_style(
            Style::default()
                .fg(WHITE)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
        )
        .divider("│");

    f.render_widget(tabs, area);
}

fn render_sessions(f: &mut Frame, app: &mut App, area: Rect) {
    if app.show_detail {
        let selected_data = app.selected_session_data();
        if let Some((info, parsed)) = selected_data {
            render_dashboard(f, app, &info, &parsed, area);
            return;
        }
    }
    render_sessions_table(f, app, area);
}

// ── Dashboard ───────────────────────────────────────────────────────────────

fn render_dashboard(
    f: &mut Frame,
    app: &mut App,
    info: &SessionInfo,
    parsed: &ParsedSession,
    area: Rect,
) {
    // Fallback to simple table on small terminals
    if area.height < 22 {
        render_sessions_table(f, app, area);
        return;
    }

    let vis_count = app.visible_count() as u16;
    let table_h = (vis_count + 3).clamp(4, 8);

    let has_messages =
        !parsed.user_messages.is_empty() || !parsed.assistant_messages.is_empty();
    let msg_h: u16 = if has_messages && area.height >= 30 {
        5
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),       // top panels
            Constraint::Length(4),       // tokens
            Constraint::Length(table_h), // sessions table
            Constraint::Min(5),          // files + activity
            Constraint::Length(msg_h),   // last messages
        ])
        .split(area);

    // Top panels: 3 columns
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(30),
            Constraint::Percentage(40),
        ])
        .split(chunks[0]);

    let status = app.statuses.get(&info.session_id);
    render_context_panel(f, parsed, status, panels[0]);
    render_usage_panel(f, parsed, status, panels[1]);
    render_session_info_panel(f, info, parsed, status, panels[2]);
    render_tokens_bar(f, parsed, chunks[1]);
    render_sessions_table(f, app, chunks[2]);
    render_files_activity(f, parsed, chunks[3]);

    if msg_h > 0 {
        render_last_messages(f, parsed, chunks[4]);
    }
}

// ── Top panels ──────────────────────────────────────────────────────────────

fn render_context_panel(f: &mut Frame, parsed: &ParsedSession, status: Option<&SessionStatus>, area: Rect) {
    // Prefer live data from statusLine hook
    let (pct, window) = if let Some(s) = status {
        let p = s.context_used_pct.unwrap_or(0.0) as u8;
        let w = s.context_window_size.unwrap_or(200_000);
        (p, w)
    } else {
        let w = context_window(&parsed.model, parsed.current_context_tokens);
        let p = if w > 0 {
            (parsed.current_context_tokens as f64 / w as f64 * 100.0) as u8
        } else {
            0
        };
        (p, w)
    };

    let accent = if pct >= 90 {
        RED
    } else if pct >= 70 {
        YELLOW
    } else {
        CYAN
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" Context ", Style::default().fg(accent)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 || inner.width < 10 {
        return;
    }

    let mut lines = Vec::new();

    // Mini sparkline (text)
    let spark_w = (inner.width as usize).saturating_sub(2);
    let spark = mini_sparkline(&parsed.context_history, spark_w);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(spark, Style::default().fg(accent)),
    ]));

    // Context gauge bar
    let bar_w = (inner.width as usize).saturating_sub(7).min(25);
    let filled = (pct as usize * bar_w / 100).min(bar_w);
    let empty = bar_w.saturating_sub(filled);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("█".repeat(filled), Style::default().fg(accent)),
        Span::styled("░".repeat(empty), Style::default().fg(DIMMER)),
        Span::styled(
            format!(" {}%", pct),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
    ]));

    // Token counts
    let current_ctx = status.map(|s| s.current_input + s.cache_read + s.cache_create)
        .filter(|&t| t > 0)
        .unwrap_or(parsed.current_context_tokens);
    lines.push(Line::from(Span::styled(
        format!(
            " {} / {}",
            format_tokens(current_ctx),
            format_tokens(window)
        ),
        Style::default().fg(DIM),
    )));

    // Compactions
    let comp_text = if parsed.compaction_count > 0 {
        format!(" {} compactions", parsed.compaction_count)
    } else {
        " 0 compactions".to_string()
    };
    let comp_color = if parsed.compaction_count > 0 {
        YELLOW
    } else {
        DIMMER
    };
    lines.push(Line::from(Span::styled(
        comp_text,
        Style::default().fg(comp_color),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn render_usage_panel(
    f: &mut Frame,
    parsed: &ParsedSession,
    status: Option<&SessionStatus>,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" Usage ", Style::default().fg(YELLOW)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();

    // Duration
    if let Some(s) = status {
        let dur = format_duration(s.duration_ms / 1000);
        let api_dur = format_duration(s.api_duration_ms / 1000);
        lines.push(Line::from(vec![
            Span::styled(" Time ", Style::default().fg(DIM)),
            Span::styled(dur, Style::default().fg(WHITE)),
            Span::styled(format!(" (api {})", api_dur), Style::default().fg(DIMMER)),
        ]));
    } else {
        let age = format_duration(parsed.age_secs);
        lines.push(Line::from(vec![
            Span::styled(" Time ", Style::default().fg(DIM)),
            Span::styled(age, Style::default().fg(WHITE)),
        ]));
    }

    // Quota bars
    if let Some(s) = status {
        if let Some(used) = s.five_hour_used_pct {
            let reset = s.five_hour_resets_at.map(format_reset).unwrap_or_default();
            let color = usage_color(used);
            let bar = usage_bar(used, inner.width.saturating_sub(18) as usize);
            lines.push(Line::from(vec![
                Span::styled(" 5h  ", Style::default().fg(DIM)),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(format!(" {:>3.0}%", used), Style::default().fg(color)),
                Span::styled(reset, Style::default().fg(DIMMER)),
            ]));
        }
        if let Some(used) = s.seven_day_used_pct {
            let reset = s.seven_day_resets_at.map(format_reset).unwrap_or_default();
            let color = usage_color(used);
            let bar = usage_bar(used, inner.width.saturating_sub(18) as usize);
            lines.push(Line::from(vec![
                Span::styled(" 7d  ", Style::default().fg(DIM)),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(format!(" {:>3.0}%", used), Style::default().fg(color)),
                Span::styled(reset, Style::default().fg(DIMMER)),
            ]));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn usage_color(used: f64) -> Color {
    if used < 50.0 {
        GREEN
    } else if used < 80.0 {
        YELLOW
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

fn render_session_info_panel(
    f: &mut Frame,
    info: &SessionInfo,
    parsed: &ParsedSession,
    status: Option<&SessionStatus>,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" Session ", Style::default().fg(GREEN)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Prefer display_name from hook ("Opus", "Sonnet") over raw model id
    let model_display = status
        .filter(|s| !s.model_name.is_empty())
        .map(|s| s.model_name.clone())
        .unwrap_or_else(|| parsed.model.replace("claude-", ""));
    let age = format_duration(parsed.age_secs);

    let mut lines = Vec::new();

    // Project + branch (hide branch if empty or bare "HEAD")
    let has_branch = !parsed.git_branch.is_empty() && parsed.git_branch != "HEAD";
    if has_branch {
        let max_branch = (inner.width as usize).saturating_sub(info.project_name.len() + 6);
        let branch: String = parsed.git_branch.chars().take(max_branch).collect();
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}", info.project_name),
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ⎇ ", Style::default().fg(MAGENTA)),
            Span::styled(branch, Style::default().fg(MAGENTA)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" {}", info.project_name),
            Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
        )));
    }

    // Model
    let max_model = (inner.width as usize).saturating_sub(2);
    let model_short: String = model_display.chars().take(max_model).collect();
    lines.push(Line::from(Span::styled(
        format!(" {}", model_short),
        Style::default().fg(CYAN),
    )));

    // Turns + age
    lines.push(Line::from(Span::styled(
        format!(" {} turns · {}", parsed.turn_count, age),
        Style::default().fg(WHITE),
    )));

    // Version
    let version = status
        .filter(|s| !s.version.is_empty())
        .map(|s| s.version.as_str())
        .unwrap_or(&parsed.version);
    if !version.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" v{}", version),
            Style::default().fg(DIMMER),
        )));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// ── Tokens bar ──────────────────────────────────────────────────────────────

fn render_tokens_bar(f: &mut Frame, parsed: &ParsedSession, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" Tokens ", Style::default().fg(MAGENTA)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 30 || inner.height < 2 {
        return;
    }

    let total = parsed.total_input_tokens
        + parsed.total_output_tokens
        + parsed.total_cache_read
        + parsed.total_cache_create;

    let half = inner.width as usize / 2;
    let bar_w = half.saturating_sub(20).max(3).min(12);

    let line1 = dual_bar(
        "Input",
        parsed.total_input_tokens,
        CYAN,
        "Output",
        parsed.total_output_tokens,
        MAGENTA,
        total,
        bar_w,
        half,
    );
    let line2 = dual_bar(
        "Cache↓",
        parsed.total_cache_read,
        GREEN,
        "Cache↑",
        parsed.total_cache_create,
        YELLOW,
        total,
        bar_w,
        half,
    );

    f.render_widget(Paragraph::new(vec![line1, line2]), inner);
}

// ── Sessions table ──────────────────────────────────────────────────────────

fn render_sessions_table(f: &mut Frame, app: &mut App, area: Rect) {
    let hide_idle = app.hide_idle;

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("PROJECT"),
        Cell::from("MODEL"),
        Cell::from("CONTEXT"),
        Cell::from("COST"),
        Cell::from("LINES"),
        Cell::from("AGE"),
    ])
    .style(Style::default().fg(DIM).add_modifier(Modifier::BOLD))
    .bottom_margin(0);

    let dead_sessions = &app.dead_sessions;
    let statuses = &app.statuses;

    let rows: Vec<Row> = app
        .sessions
        .iter()
        .filter(|(_, p)| !hide_idle || p.age_secs < 300)
        .map(|(info, parsed)| {
            let status = statuses.get(&info.session_id);

            let process_dead = dead_sessions.contains(&info.session_id);
            let state = session::classify_state(parsed.age_secs, process_dead);

            let (dot, dot_color) = match state {
                session::SessionState::Working => ("●", GREEN),
                session::SessionState::Waiting => ("◐", YELLOW),
                session::SessionState::Ended => ("✕", RED),
                session::SessionState::Idle => ("○", DIMMER),
            };

            let is_idle = state == session::SessionState::Idle;
            let text_color = if is_idle { DIMMER } else { WHITE };

            // Context % — prefer live data
            let pct = status
                .and_then(|s| s.context_used_pct)
                .map(|p| p as u8)
                .unwrap_or_else(|| {
                    let w = context_window(&parsed.model, parsed.current_context_tokens);
                    if w > 0 { (parsed.current_context_tokens as f64 / w as f64 * 100.0) as u8 } else { 0 }
                });

            let ctx_color = if pct >= 90 {
                RED
            } else if pct >= 70 {
                YELLOW
            } else {
                GREEN
            };

            let spark = mini_sparkline(&parsed.context_history, 8);
            let ctx_display = format!("{} {}%", spark, pct);

            // Model — prefer display_name
            let model_short = status
                .filter(|s| !s.model_name.is_empty())
                .map(|s| s.model_name.clone())
                .unwrap_or_else(|| {
                    parsed.model.replace("claude-", "")
                        .split('[').next().unwrap_or("").to_string()
                });

            // Cost — prefer live
            let cost = status.map(|s| s.cost_usd).unwrap_or_else(|| estimate_cost(parsed));
            let cost_color = if cost > 1.0 { YELLOW } else { DIM };

            // Lines changed
            let lines_display = status
                .filter(|s| s.lines_added > 0 || s.lines_removed > 0)
                .map(|s| format!("+{}-{}", s.lines_added, s.lines_removed))
                .unwrap_or_else(|| "–".to_string());

            let age = format_duration(parsed.age_secs);

            Row::new(vec![
                Cell::from(dot).style(Style::default().fg(dot_color)),
                Cell::from(info.project_name.clone()).style(Style::default().fg(text_color)),
                Cell::from(model_short)
                    .style(Style::default().fg(if is_idle { DIMMER } else { DIM })),
                Cell::from(ctx_display).style(Style::default().fg(ctx_color)),
                Cell::from(format!("${:.2}", cost)).style(Style::default().fg(cost_color)),
                Cell::from(lines_display).style(Style::default().fg(text_color)),
                Cell::from(age).style(Style::default().fg(DIM)),
            ])
        })
        .collect();

    let vis_count = rows.len();
    let idle_hint = if hide_idle { "  i: show idle" } else { "" };

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),  // dot
            Constraint::Min(12),    // PROJECT
            Constraint::Length(10), // MODEL
            Constraint::Length(14), // CONTEXT
            Constraint::Length(7),  // COST
            Constraint::Length(9),  // LINES
            Constraint::Length(5),  // AGE
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .title(Span::styled(
                format!(" {} sessions{} ", vis_count, idle_hint),
                Style::default().fg(WHITE),
            )),
    )
    .highlight_style(
        Style::default()
            .bg(BG_SELECT)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▸ ");

    f.render_stateful_widget(table, area, &mut app.table_state);
}

// ── Files + Activity ────────────────────────────────────────────────────────

fn render_files_activity(f: &mut Frame, parsed: &ParsedSession, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // ── Files panel ─────────────────────────────────────────────────────
    let files_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" Files ({}) ", parsed.files_touched.len()),
            Style::default().fg(WHITE),
        ));
    let files_inner = files_block.inner(cols[0]);
    f.render_widget(files_block, cols[0]);

    let max_files = files_inner.height as usize;
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
        let max_w = (files_inner.width as usize).saturating_sub(3);
        let display: String = display.chars().take(max_w).collect();
        file_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(display, Style::default().fg(WHITE)),
        ]));
    }
    if parsed.files_touched.len() > max_files {
        file_lines.push(Line::from(Span::styled(
            format!("  +{} more", parsed.files_touched.len() - max_files),
            Style::default().fg(DIMMER),
        )));
    }
    if file_lines.is_empty() {
        file_lines.push(Line::from(Span::styled(
            "  no files yet",
            Style::default().fg(DIMMER),
        )));
    }
    f.render_widget(Paragraph::new(file_lines), files_inner);

    // ── Activity panel ──────────────────────────────────────────────────
    let activity_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" Activity ({}) ", parsed.tool_uses.len()),
            Style::default().fg(WHITE),
        ));
    let activity_inner = activity_block.inner(cols[1]);
    f.render_widget(activity_block, cols[1]);

    let max_tools = activity_inner.height as usize;
    let mut tool_lines: Vec<Line> = Vec::new();
    for tool in parsed.tool_uses.iter().rev().take(max_tools) {
        let max_summary = (activity_inner.width as usize).saturating_sub(12);
        let summary: String = tool.input_summary.chars().take(max_summary).collect();
        let short = summary
            .rsplit('/')
            .next()
            .unwrap_or(&summary)
            .to_string();
        tool_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<6}", tool.name), Style::default().fg(CYAN)),
            Span::raw(" "),
            Span::styled(short, Style::default().fg(DIM)),
        ]));
    }
    if tool_lines.is_empty() {
        tool_lines.push(Line::from(Span::styled(
            "  no activity yet",
            Style::default().fg(DIMMER),
        )));
    }
    f.render_widget(Paragraph::new(tool_lines), activity_inner);
}

// ── Last messages ───────────────────────────────────────────────────────────

fn render_last_messages(f: &mut Frame, parsed: &ParsedSession, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Last prompt
    let prompt_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" Last prompt ", Style::default().fg(WHITE)));
    let prompt_inner = prompt_block.inner(cols[0]);
    f.render_widget(prompt_block, cols[0]);

    let prompt_text = parsed
        .user_messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let prompt_w = (prompt_inner.width as usize).saturating_sub(2);
    let prompt_lines: Vec<Line> = word_wrap(&prompt_text, prompt_w, prompt_inner.height as usize)
        .into_iter()
        .map(|l| {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(l, Style::default().fg(WHITE)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(prompt_lines), prompt_inner);

    // Last response
    let resp_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" Last response ", Style::default().fg(DIM)));
    let resp_inner = resp_block.inner(cols[1]);
    f.render_widget(resp_block, cols[1]);

    let resp_text = parsed
        .assistant_messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let resp_w = (resp_inner.width as usize).saturating_sub(2);
    let resp_lines: Vec<Line> = word_wrap(&resp_text, resp_w, resp_inner.height as usize)
        .into_iter()
        .map(|l| {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(l, Style::default().fg(DIM)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(resp_lines), resp_inner);
}

// ── Handoffs tab ────────────────────────────────────────────────────────────

fn render_handoffs(f: &mut Frame, app: &mut App, area: Rect) {
    if app.handoffs.is_empty() {
        let empty = Paragraph::new(
            "  No handoffs saved yet. Press 's' in Sessions tab to create one.",
        )
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
        .highlight_style(
            Style::default()
                .bg(BG_SELECT)
                .add_modifier(Modifier::BOLD),
        )
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
        .style(Style::default().fg(WHITE));

    f.render_widget(preview, chunks[1]);
}

// ── Footer ──────────────────────────────────────────────────────────────────

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let (text, color) = if let Some((ref msg, at)) = app.status_msg {
        if at.elapsed() < Duration::from_secs(4) {
            (format!(" {msg}"), GREEN)
        } else {
            (keybindings_text(), DIM)
        }
    } else {
        (keybindings_text(), DIM)
    };

    let footer = Paragraph::new(text).style(Style::default().fg(color));
    f.render_widget(footer, area);
}

fn keybindings_text() -> String {
    " ↑↓ select │ a auto-handoff │ g git-commit │ s save │ d detail │ i idle │ tab switch │ r refresh │ q quit"
        .to_string()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn word_wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let clean: String = text
        .replace("**", "")
        .replace('\n', " ")
        .replace("  ", " ");
    let mut lines = Vec::new();
    let mut cur = String::new();

    for word in clean.split_whitespace() {
        if lines.len() >= max_lines {
            break;
        }
        if cur.is_empty() {
            cur = word.to_string();
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(cur);
            cur = word.to_string();
        }
    }
    if !cur.is_empty() && lines.len() < max_lines {
        lines.push(cur);
    }
    lines
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

fn bar_spans(label: &str, value: u64, total: u64, bar_w: usize, color: Color) -> Vec<Span<'static>> {
    let filled = if total > 0 {
        ((value as f64 / total as f64) * bar_w as f64) as usize
    } else {
        0
    };
    let empty = bar_w.saturating_sub(filled);
    let pct = if total > 0 {
        (value as f64 / total as f64 * 100.0) as u8
    } else {
        0
    };

    vec![
        Span::styled(format!(" {:<7}", label), Style::default().fg(DIM)),
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(empty), Style::default().fg(DIMMER)),
        Span::styled(
            format!(" {}({}%)", format_tokens(value), pct),
            Style::default().fg(WHITE),
        ),
    ]
}

fn dual_bar(
    l1: &str,
    v1: u64,
    c1: Color,
    l2: &str,
    v2: u64,
    c2: Color,
    total: u64,
    bar_w: usize,
    half_w: usize,
) -> Line<'static> {
    let left = bar_spans(l1, v1, total, bar_w, c1);
    let left_len: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let pad = half_w.saturating_sub(left_len);

    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(bar_spans(l2, v2, total, bar_w, c2));
    Line::from(spans)
}

fn context_window(model: &str, observed: u64) -> u64 {
    let m = model.to_lowercase();
    if m.contains("[1m]") || m.contains("opus") || observed > 180_000 {
        1_000_000
    } else {
        200_000
    }
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

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
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