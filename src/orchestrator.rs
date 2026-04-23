use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ── Plan definition ────────────────────────────────────────────────────────

const MAX_OUTPUT_LINES: usize = 5000;
const VALID_ON_COMPLETE: &[&str] = &["manual", "merge", "pr"];

#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    pub plan: PlanMeta,
    #[serde(rename = "tasks")]
    pub tasks: Vec<TaskDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlanMeta {
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_on_complete")]
    pub on_complete: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Skip all permission checks in spawned claude sessions (default: false).
    /// WARNING: This allows arbitrary code execution from the plan's prompts.
    #[serde(default)]
    pub skip_permissions: bool,
}

fn default_on_complete() -> String {
    "manual".to_string()
}

fn default_branch() -> String {
    "orchestrate".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskDef {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub allowed_tools: Option<String>,
}

// ── Runtime state ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TaskStatus {
    Pending,
    Blocked,
    Running,
    Done,
    Failed,
}

impl TaskStatus {
    pub fn symbol(self) -> &'static str {
        match self {
            TaskStatus::Pending => "◌",
            TaskStatus::Blocked => "⊘",
            TaskStatus::Running => "●",
            TaskStatus::Done => "✓",
            TaskStatus::Failed => "✗",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskState {
    pub def: TaskDef,
    pub status: TaskStatus,
    pub output_lines: Vec<String>,
    pub exit_code: Option<i32>,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot {
    pub name: String,
    pub status: String,
    pub depends_on: Vec<String>,
    pub output_tail: Vec<String>,
    pub exit_code: Option<i32>,
    pub elapsed_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrchestrationSnapshot {
    pub plan_name: String,
    pub branch: String,
    pub state: String,
    pub tasks: Vec<TaskSnapshot>,
    pub elapsed_secs: u64,
    pub counts: OrchestrationCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrchestrationCounts {
    pub pending: usize,
    pub blocked: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
}

pub struct Orchestrator {
    pub plan: Plan,
    pub project_root: PathBuf,
    pub tasks: Vec<TaskState>,
    pub branch_name: String,
    worktree_path: Option<PathBuf>,
    processes: HashMap<usize, Child>,
    /// Shared buffers populated by reader threads (one per running task).
    stdout_buffers: HashMap<usize, Arc<Mutex<Vec<String>>>>,
    pub started_at: Instant,
}

// ── Plan parsing & validation ──────────────────────────────────────────────

fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn load_plan(path: &Path) -> Result<Plan> {
    let content =
        std::fs::read_to_string(path).context(format!("Cannot read plan: {}", path.display()))?;
    let plan: Plan =
        toml::from_str(&content).context(format!("Invalid plan TOML: {}", path.display()))?;

    validate_plan(&plan)?;
    Ok(plan)
}

pub fn validate_plan(plan: &Plan) -> Result<()> {
    if plan.tasks.is_empty() {
        bail!("Plan has no tasks");
    }

    // Validate plan-level fields
    if !is_safe_identifier(&plan.plan.name) {
        bail!(
            "Plan name '{}' contains invalid characters (use alphanumeric, -, _)",
            plan.plan.name
        );
    }
    if !is_safe_identifier(&plan.plan.branch) {
        bail!(
            "Branch '{}' contains invalid characters (use alphanumeric, -, _)",
            plan.plan.branch
        );
    }
    if !VALID_ON_COMPLETE.contains(&plan.plan.on_complete.as_str()) {
        bail!(
            "Invalid on_complete value '{}' (valid: {})",
            plan.plan.on_complete,
            VALID_ON_COMPLETE.join(", ")
        );
    }

    // Check task names are unique and valid
    let mut seen = HashSet::new();
    for task in &plan.tasks {
        if !is_safe_identifier(&task.name) {
            bail!(
                "Task name '{}' contains invalid characters (use alphanumeric, -, _)",
                task.name
            );
        }
        if !seen.insert(&task.name) {
            bail!("Duplicate task name: '{}'", task.name);
        }
    }

    // Check dependencies exist
    let names: HashSet<&str> = plan.tasks.iter().map(|t| t.name.as_str()).collect();
    for task in &plan.tasks {
        for dep in &task.depends_on {
            if !names.contains(dep.as_str()) {
                bail!(
                    "Task '{}' depends on '{}', which does not exist",
                    task.name,
                    dep
                );
            }
        }
    }

    // Detect cycles via topological sort
    detect_cycles(plan)?;

    Ok(())
}

fn detect_cycles(plan: &Plan) -> Result<()> {
    let name_to_idx: HashMap<&str, usize> = plan
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.name.as_str(), i))
        .collect();

    let n = plan.tasks.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_degree: Vec<usize> = vec![0; n];

    for (i, task) in plan.tasks.iter().enumerate() {
        for dep in &task.depends_on {
            if let Some(&j) = name_to_idx.get(dep.as_str()) {
                adj[j].push(i);
                in_degree[i] += 1;
            }
        }
    }

    // Kahn's algorithm
    let mut queue: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut visited = 0;

    while let Some(node) = queue.pop() {
        visited += 1;
        for &next in &adj[node] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                queue.push(next);
            }
        }
    }

    if visited != n {
        bail!("Cycle detected in task dependencies");
    }

    Ok(())
}

// ── Orchestrator ───────────────────────────────────────────────────────────

impl Orchestrator {
    pub fn new(plan: Plan, project_root: PathBuf) -> Self {
        let branch_name = plan.plan.branch.clone();
        let tasks: Vec<TaskState> = plan
            .tasks
            .iter()
            .map(|def| {
                let initial_status = if def.depends_on.is_empty() {
                    TaskStatus::Pending
                } else {
                    TaskStatus::Blocked
                };
                TaskState {
                    def: def.clone(),
                    status: initial_status,
                    output_lines: Vec::new(),
                    exit_code: None,
                    started_at: None,
                    finished_at: None,
                }
            })
            .collect();

        Self {
            plan,
            project_root,
            tasks,
            branch_name,
            worktree_path: None,
            processes: HashMap::new(),
            stdout_buffers: HashMap::new(),
            started_at: Instant::now(),
        }
    }

    /// Create the shared worktree and branch. Must be called before tick().
    pub fn setup(&mut self) -> Result<()> {
        let worktree_dir = self
            .project_root
            .join(".worktrees")
            .join(&self.plan.plan.name);
        let branch = &self.branch_name;

        std::fs::create_dir_all(worktree_dir.parent().unwrap_or(&self.project_root))?;

        // Delete stale worktree if exists
        if worktree_dir.exists() {
            let _ = Command::new("git")
                .args([
                    "-C",
                    &self.project_root.to_string_lossy(),
                    "worktree",
                    "remove",
                    "--force",
                    "--",
                    &worktree_dir.to_string_lossy(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        // Delete stale branch if exists
        let _ = Command::new("git")
            .args([
                "-C",
                &self.project_root.to_string_lossy(),
                "branch",
                "-D",
                "--",
                branch,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let output = Command::new("git")
            .args([
                "-C",
                &self.project_root.to_string_lossy(),
                "worktree",
                "add",
                &worktree_dir.to_string_lossy(),
                "-b",
                branch,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("Failed to run git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add failed: {}", stderr.trim());
        }

        self.worktree_path = Some(worktree_dir);
        Ok(())
    }

    /// Drive the scheduler forward: start ready tasks, poll running ones.
    /// Returns true if all tasks are terminal (done/failed).
    pub fn tick(&mut self) -> bool {
        self.poll_running();
        self.propagate_failures();
        self.unblock_ready();
        self.start_pending();
        self.drain_stdout();

        self.tasks
            .iter()
            .all(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Failed))
    }

    /// Summary counts: (pending, blocked, running, done, failed)
    pub fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let mut p = 0;
        let mut b = 0;
        let mut r = 0;
        let mut d = 0;
        let mut f = 0;
        for t in &self.tasks {
            match t.status {
                TaskStatus::Pending => p += 1,
                TaskStatus::Blocked => b += 1,
                TaskStatus::Running => r += 1,
                TaskStatus::Done => d += 1,
                TaskStatus::Failed => f += 1,
            }
        }
        (p, b, r, d, f)
    }

    /// Elapsed time since orchestration started.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// Build a serializable snapshot of the current orchestration state.
    pub fn snapshot(&self) -> OrchestrationSnapshot {
        let (pending, blocked, running, done, failed) = self.counts();

        let all_terminal = self
            .tasks
            .iter()
            .all(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Failed));

        let state = if !all_terminal {
            "running"
        } else if failed > 0 {
            "failed"
        } else {
            "completed"
        }
        .to_string();

        let tasks = self
            .tasks
            .iter()
            .map(|t| {
                let tail_start = t.output_lines.len().saturating_sub(50);
                TaskSnapshot {
                    name: t.def.name.clone(),
                    status: t.status.label().to_string(),
                    depends_on: t.def.depends_on.clone(),
                    output_tail: t.output_lines[tail_start..].to_vec(),
                    exit_code: t.exit_code,
                    elapsed_secs: t.started_at.map(|s| {
                        t.finished_at
                            .unwrap_or_else(Instant::now)
                            .duration_since(s)
                            .as_secs()
                    }),
                }
            })
            .collect();

        OrchestrationSnapshot {
            plan_name: self.plan.plan.name.clone(),
            branch: self.branch_name.clone(),
            state,
            tasks,
            elapsed_secs: self.elapsed().as_secs(),
            counts: OrchestrationCounts {
                pending,
                blocked,
                running,
                done,
                failed,
            },
        }
    }

    /// Kill all running tasks and clean up.
    pub fn abort(&mut self) {
        let running_indices: Vec<usize> = self.processes.keys().copied().collect();

        for idx in running_indices {
            if let Some(mut child) = self.processes.remove(&idx) {
                let _ = child.kill();
                let _ = child.wait();
            }
            self.stdout_buffers.remove(&idx);
            self.tasks[idx].status = TaskStatus::Failed;
            self.tasks[idx].finished_at = Some(Instant::now());
            self.tasks[idx]
                .output_lines
                .push("[aborted by relay]".to_string());
        }
    }

    /// Clean up the shared worktree.
    pub fn cleanup_worktree(&self) {
        if let Some(wt) = &self.worktree_path {
            if wt.exists() {
                let _ = Command::new("git")
                    .args([
                        "-C",
                        &self.project_root.to_string_lossy(),
                        "worktree",
                        "remove",
                        "--force",
                        &wt.to_string_lossy(),
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    /// Generate a combined summary of all task results.
    pub fn generate_summary(&self) -> String {
        let mut md = String::new();
        let elapsed = self.elapsed().as_secs();
        let elapsed_fmt = crate::util::format_duration(elapsed);

        md.push_str(&format!(
            "# Orchestration Summary — {}\n\n",
            self.plan.plan.name
        ));
        md.push_str(&format!("**Branch:** `{}`\n", self.branch_name));
        md.push_str(&format!("**Elapsed:** {elapsed_fmt}\n"));
        md.push_str(&format!("**Tasks:** {}\n\n", self.tasks.len()));

        for task in &self.tasks {
            let icon = task.status.symbol();
            let label = task.status.label();
            md.push_str(&format!("## {icon} {} — {label}\n\n", task.def.name));

            // Last 20 output lines as context
            let skip = task.output_lines.len().saturating_sub(20);
            let tail: Vec<&str> = task.output_lines[skip..]
                .iter()
                .map(|s| s.as_str())
                .collect();

            if !tail.is_empty() {
                md.push_str("\n<details><summary>Output (last 20 lines)</summary>\n\n```\n");
                for line in tail {
                    md.push_str(line);
                    md.push('\n');
                }
                md.push_str("```\n</details>\n");
            }
            md.push('\n');
        }

        md
    }

    // ── Internal ───────────────────────────────────────────────────────────

    /// Propagate failures: if a dependency failed, mark blocked dependents as failed too.
    fn propagate_failures(&mut self) {
        let failed_names: HashSet<String> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .map(|t| t.def.name.clone())
            .collect();

        if failed_names.is_empty() {
            return;
        }

        for task in &mut self.tasks {
            if task.status == TaskStatus::Blocked {
                let has_failed_dep = task.def.depends_on.iter().any(|d| failed_names.contains(d));
                if has_failed_dep {
                    task.status = TaskStatus::Failed;
                    task.finished_at = Some(Instant::now());
                    let failed_dep = task
                        .def
                        .depends_on
                        .iter()
                        .find(|d| failed_names.contains(*d))
                        .cloned()
                        .unwrap_or_default();
                    task.output_lines
                        .push(format!("[relay] dependency '{}' failed", failed_dep));
                }
            }
        }
    }

    /// Unblock tasks whose dependencies are all done.
    fn unblock_ready(&mut self) {
        let done_names: HashSet<String> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Done)
            .map(|t| t.def.name.clone())
            .collect();

        for task in &mut self.tasks {
            if task.status == TaskStatus::Blocked {
                let all_deps_done = task
                    .def
                    .depends_on
                    .iter()
                    .all(|dep| done_names.contains(dep));
                if all_deps_done {
                    task.status = TaskStatus::Pending;
                }
            }
        }
    }

    /// Start the next pending task (sequential: one at a time in the shared worktree).
    fn start_pending(&mut self) {
        let running = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        // Sequential: only one task at a time in the shared worktree
        if running > 0 {
            return;
        }

        let next = self
            .tasks
            .iter()
            .enumerate()
            .find(|(_, t)| t.status == TaskStatus::Pending)
            .map(|(i, _)| i);

        if let Some(idx) = next {
            match self.start_task(idx) {
                Ok(()) => {}
                Err(e) => {
                    self.tasks[idx].status = TaskStatus::Failed;
                    self.tasks[idx].finished_at = Some(Instant::now());
                    self.tasks[idx]
                        .output_lines
                        .push(format!("[relay error] {e}"));
                }
            }
        }
    }

    fn start_task(&mut self, idx: usize) -> Result<()> {
        let worktree_dir = self
            .worktree_path
            .as_ref()
            .context("Worktree not set up — call setup() first")?
            .clone();

        let task = &self.tasks[idx];

        // Build claude command
        let model = task
            .def
            .model
            .as_deref()
            .or(self.plan.plan.model.as_deref());

        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(&task.def.prompt)
            .arg("--output-format")
            .arg("text")
            .current_dir(&worktree_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if self.plan.plan.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }
        if let Some(m) = model {
            cmd.arg("--model").arg(m);
        }
        if let Some(tools) = &task.def.allowed_tools {
            cmd.arg("--allowedTools").arg(tools);
        }

        let mut child = cmd.spawn().context(format!(
            "Failed to spawn claude for task '{}'",
            task.def.name
        ))?;

        // Spawn reader threads for non-blocking stdout/stderr capture
        let buffer = Arc::new(Mutex::new(Vec::<String>::new()));

        if let Some(stdout) = child.stdout.take() {
            let buf = Arc::clone(&buffer);
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(mut v) = buf.lock() {
                        v.push(line);
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let buf = Arc::clone(&buffer);
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(mut v) = buf.lock() {
                        v.push(format!("[stderr] {line}"));
                    }
                }
            });
        }

        self.stdout_buffers.insert(idx, buffer);
        self.tasks[idx].status = TaskStatus::Running;
        self.tasks[idx].started_at = Some(Instant::now());
        self.processes.insert(idx, child);

        Ok(())
    }

    /// Poll running processes for completion.
    fn poll_running(&mut self) {
        // Collect completed process indices first to avoid borrow issues
        let mut completed: Vec<(usize, i32)> = Vec::new();
        let mut errored: Vec<(usize, String)> = Vec::new();

        for (&idx, child) in &mut self.processes {
            match child.try_wait() {
                Ok(Some(exit_status)) => {
                    let code = exit_status.code().unwrap_or(-1);
                    completed.push((idx, code));
                }
                Ok(None) => {} // still running
                Err(e) => {
                    errored.push((idx, format!("poll failed: {e}")));
                }
            }
        }

        // Process completed tasks
        for (idx, code) in completed {
            if let Some(mut child) = self.processes.remove(&idx) {
                let _ = child.wait(); // reap zombie
            }
            self.tasks[idx].exit_code = Some(code);
            self.tasks[idx].finished_at = Some(Instant::now());
            self.tasks[idx].status = if code == 0 {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };

            // Auto-commit successful task work in the worktree
            if code == 0 {
                if let Some(wt) = &self.worktree_path {
                    let wt_str = wt.to_string_lossy();
                    if crate::git::has_uncommitted_changes(&wt_str) {
                        let msg =
                            format!("relay(orchestrate): task '{}'", self.tasks[idx].def.name);
                        match crate::git::auto_commit(&wt_str, &msg) {
                            Ok(hash) => {
                                self.tasks[idx]
                                    .output_lines
                                    .push(format!("[relay] committed {hash}"));
                            }
                            Err(e) => {
                                self.tasks[idx]
                                    .output_lines
                                    .push(format!("[relay] commit failed: {e}"));
                            }
                        }
                    }
                }
            }

            // Reader threads will finish on their own when the pipe closes.
            // Final drain happens in drain_stdout.
        }

        // Process errored tasks
        for (idx, msg) in errored {
            if let Some(mut child) = self.processes.remove(&idx) {
                let _ = child.kill();
                let _ = child.wait();
            }
            self.tasks[idx].status = TaskStatus::Failed;
            self.tasks[idx].finished_at = Some(Instant::now());
            self.tasks[idx]
                .output_lines
                .push(format!("[relay error] {msg}"));
            self.stdout_buffers.remove(&idx);
        }
    }

    /// Drain output from reader thread buffers into task output_lines.
    fn drain_stdout(&mut self) {
        let indices: Vec<usize> = self.stdout_buffers.keys().copied().collect();

        for idx in indices {
            let buf = match self.stdout_buffers.get(&idx) {
                Some(b) => Arc::clone(b),
                None => continue,
            };

            if let Ok(mut locked) = buf.lock() {
                if !locked.is_empty() {
                    let task = &mut self.tasks[idx];
                    task.output_lines.append(&mut *locked);
                    // Cap output lines to prevent unbounded growth
                    if task.output_lines.len() > MAX_OUTPUT_LINES {
                        let excess = task.output_lines.len() - MAX_OUTPUT_LINES;
                        task.output_lines.drain(..excess);
                    }
                }
            }

            // Clean up buffer for finished tasks
            if matches!(
                self.tasks[idx].status,
                TaskStatus::Done | TaskStatus::Failed
            ) {
                self.stdout_buffers.remove(&idx);
            }
        }
    }

    /// Merge the plan branch back into the base branch.
    pub fn merge_branch(&self) -> Result<String, String> {
        let any_done = self.tasks.iter().any(|t| t.status == TaskStatus::Done);
        if !any_done {
            return Err("no completed tasks".to_string());
        }

        let output = Command::new("git")
            .args([
                "-C",
                &self.project_root.to_string_lossy(),
                "merge",
                "--no-edit",
                "-m",
                &format!("relay(orchestrate): merge plan '{}'", self.plan.plan.name),
                "--",
                &self.branch_name,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match output {
            Ok(o) if o.status.success() => Ok("merged".to_string()),
            Ok(o) => {
                // Abort failed merge
                let _ = Command::new("git")
                    .args([
                        "-C",
                        &self.project_root.to_string_lossy(),
                        "merge",
                        "--abort",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Create a pull request for the plan branch (requires gh CLI).
    pub fn create_pull_request(&self) -> Result<String, String> {
        let any_done = self.tasks.iter().any(|t| t.status == TaskStatus::Done);
        if !any_done {
            return Err("no completed tasks".to_string());
        }

        // Push the branch first
        let push = Command::new("git")
            .args([
                "-C",
                &self.project_root.to_string_lossy(),
                "push",
                "-u",
                "origin",
                "--",
                &self.branch_name,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        if let Ok(o) = &push {
            if !o.status.success() {
                return Err(format!(
                    "push failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
            }
        }

        let title = format!("relay({}): {}", self.plan.plan.name, self.plan.plan.name);
        let task_list: String = self
            .tasks
            .iter()
            .map(|t| format!("- {} {}", t.status.symbol(), t.def.name))
            .collect::<Vec<_>>()
            .join("\n");
        let body = format!(
            "## Orchestration: {}\n\n### Tasks\n\n{}\n",
            self.plan.plan.name, task_list
        );

        let output = Command::new("gh")
            .args([
                "pr",
                "create",
                "--title",
                &title,
                "--body",
                &body,
                "--head",
                &self.branch_name,
            ])
            .current_dir(&self.project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match output {
            Ok(o) if o.status.success() => {
                Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
            }
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ── Interactive plan creation ─────────────────────────────────────────────

fn prompt_line(label: &str, default: &str) -> String {
    if default.is_empty() {
        print!("  {label}: ");
    } else {
        print!("  {label} [{default}]: ");
    }
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    let trimmed = input.trim().to_string();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed
    }
}

fn prompt_yes_no(label: &str, default: bool) -> bool {
    let hint = if default { "Y/n" } else { "y/N" };
    print!("  {label} [{hint}]: ");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        default
    } else {
        trimmed.starts_with('y')
    }
}

fn prompt_multiline(label: &str) -> String {
    println!("  {label} (end with empty line):");
    let mut lines = Vec::new();
    loop {
        print!("  > ");
        io::stdout().flush().unwrap();
        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        let trimmed = input.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed.is_empty() && !lines.is_empty() {
            break;
        }
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }
    lines.join("\n")
}

pub fn create_plan_interactive(output_path: &Path) -> Result<()> {
    use colored::Colorize;

    println!("{}", "relay — create orchestration plan".green().bold());
    println!();

    // Plan metadata
    println!("{}", "[plan]".dimmed());
    let name = prompt_line("Plan name", "");
    if name.is_empty() {
        bail!("Plan name is required");
    }
    if !is_safe_identifier(&name) {
        bail!("Plan name must be alphanumeric, -, or _");
    }

    let branch = prompt_line("Branch name", &name);
    if !is_safe_identifier(&branch) {
        bail!("Branch name must be alphanumeric, -, or _");
    }

    let model = prompt_line("Model (optional, e.g. claude-sonnet-4-6)", "");

    let on_complete_input = prompt_line("On complete (manual/merge/pr)", "manual");
    let on_complete = match on_complete_input.as_str() {
        "manual" | "merge" | "pr" => on_complete_input,
        _ => {
            println!("  Invalid value, defaulting to 'manual'");
            "manual".to_string()
        }
    };

    let skip_permissions = prompt_yes_no("Skip permission checks?", false);

    // Tasks
    println!();
    println!("{}", "[[tasks]]".dimmed());

    let mut tasks: Vec<TaskDef> = Vec::new();
    let mut task_names: HashSet<String> = HashSet::new();

    loop {
        println!("{}", format!("  — Task #{} —", tasks.len() + 1).dimmed());

        let task_name = prompt_line("Task name", "");
        if task_name.is_empty() {
            if tasks.is_empty() {
                bail!("At least one task is required");
            }
            break;
        }
        if !is_safe_identifier(&task_name) {
            println!("  Name must be alphanumeric, -, or _. Skipping.");
            continue;
        }
        if task_names.contains(&task_name) {
            println!("  Duplicate name. Skipping.");
            continue;
        }

        let prompt = prompt_multiline("Prompt");
        if prompt.is_empty() {
            println!("  Prompt is required. Skipping task.");
            continue;
        }

        let task_model = prompt_line("Model (optional, press Enter to inherit plan model)", "");

        let deps_input = prompt_line("Depends on (comma-separated task names, or empty)", "");
        let depends_on: Vec<String> = if deps_input.is_empty() {
            vec![]
        } else {
            deps_input
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };

        // Validate deps exist
        let mut deps_valid = true;
        for dep in &depends_on {
            if !task_names.contains(dep) {
                println!("  Warning: dependency '{dep}' not found in previous tasks.");
                deps_valid = false;
            }
        }
        if !deps_valid && !prompt_yes_no("Keep this task anyway?", true) {
            continue;
        }

        let allowed_tools = prompt_line("Allowed tools (optional, e.g. Read,Grep,Edit)", "");

        task_names.insert(task_name.clone());
        tasks.push(TaskDef {
            name: task_name,
            prompt,
            model: if task_model.is_empty() {
                None
            } else {
                Some(task_model)
            },
            depends_on,
            allowed_tools: if allowed_tools.is_empty() {
                None
            } else {
                Some(allowed_tools)
            },
        });

        println!();
        if !prompt_yes_no("Add another task?", true) {
            break;
        }
        println!();
    }

    // Generate TOML
    let mut toml_out = String::new();
    toml_out.push_str("[plan]\n");
    toml_out.push_str(&format!("name = {:?}\n", name));
    if !model.is_empty() {
        toml_out.push_str(&format!("model = {:?}\n", model));
    }
    toml_out.push_str(&format!("on_complete = {:?}\n", on_complete));
    toml_out.push_str(&format!("branch = {:?}\n", branch));
    if skip_permissions {
        toml_out.push_str("skip_permissions = true\n");
    }

    for task in &tasks {
        toml_out.push('\n');
        toml_out.push_str("[[tasks]]\n");
        toml_out.push_str(&format!("name = {:?}\n", task.name));
        // Use multi-line string for prompts containing newlines
        if task.prompt.contains('\n') {
            toml_out.push_str(&format!("prompt = '''\n{}\n'''\n", task.prompt));
        } else {
            toml_out.push_str(&format!("prompt = {:?}\n", task.prompt));
        }
        if let Some(m) = &task.model {
            toml_out.push_str(&format!("model = {:?}\n", m));
        }
        if !task.depends_on.is_empty() {
            let deps: Vec<String> = task.depends_on.iter().map(|d| format!("{d:?}")).collect();
            toml_out.push_str(&format!("depends_on = [{}]\n", deps.join(", ")));
        }
        if let Some(tools) = &task.allowed_tools {
            toml_out.push_str(&format!("allowed_tools = {:?}\n", tools));
        }
    }

    // Write file
    std::fs::write(output_path, &toml_out)
        .context(format!("Cannot write plan to {}", output_path.display()))?;

    println!();
    println!(
        "{}",
        format!("Plan saved to {}", output_path.display())
            .green()
            .bold()
    );
    println!(
        "Run with: {}",
        format!("relay orchestrate {}", output_path.display()).cyan()
    );

    Ok(())
}

/// Ensure child processes are killed if the Orchestrator is dropped
/// (e.g. on panic or early return).
impl Drop for Orchestrator {
    fn drop(&mut self) {
        for (_, mut child) in self.processes.drain() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_plan() -> Plan {
        Plan {
            plan: PlanMeta {
                name: "test-plan".to_string(),
                model: None,
                on_complete: "manual".to_string(),
                branch: "orchestrate".to_string(),
                skip_permissions: false,
            },
            tasks: vec![
                TaskDef {
                    name: "task-a".to_string(),
                    prompt: "Do thing A".to_string(),
                    model: None,
                    depends_on: vec![],
                    allowed_tools: None,
                },
                TaskDef {
                    name: "task-b".to_string(),
                    prompt: "Do thing B".to_string(),
                    model: None,
                    depends_on: vec!["task-a".to_string()],
                    allowed_tools: None,
                },
            ],
        }
    }

    #[test]
    fn test_validate_plan_ok() {
        let plan = minimal_plan();
        assert!(validate_plan(&plan).is_ok());
    }

    #[test]
    fn test_validate_plan_empty_tasks() {
        let mut plan = minimal_plan();
        plan.tasks.clear();
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn test_validate_plan_duplicate_name() {
        let mut plan = minimal_plan();
        plan.tasks[1].name = "task-a".to_string();
        let err = validate_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("Duplicate"));
    }

    #[test]
    fn test_validate_plan_missing_dep() {
        let mut plan = minimal_plan();
        plan.tasks[1].depends_on = vec!["nonexistent".to_string()];
        let err = validate_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_validate_plan_cycle() {
        let plan = Plan {
            plan: PlanMeta {
                name: "cycle".to_string(),
                model: None,
                on_complete: "manual".to_string(),
                branch: "orchestrate".to_string(),
                skip_permissions: false,
            },
            tasks: vec![
                TaskDef {
                    name: "a".to_string(),
                    prompt: "A".to_string(),
                    model: None,
                    depends_on: vec!["b".to_string()],
                    allowed_tools: None,
                },
                TaskDef {
                    name: "b".to_string(),
                    prompt: "B".to_string(),
                    model: None,
                    depends_on: vec!["a".to_string()],
                    allowed_tools: None,
                },
            ],
        };
        let err = validate_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("Cycle"));
    }

    #[test]
    fn test_validate_plan_invalid_name() {
        let mut plan = minimal_plan();
        plan.tasks[0].name = "task with spaces".to_string();
        let err = validate_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn test_validate_plan_invalid_on_complete() {
        let mut plan = minimal_plan();
        plan.plan.on_complete = "marge".to_string();
        let err = validate_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("Invalid on_complete"));
    }

    #[test]
    fn test_validate_plan_invalid_branch() {
        let mut plan = minimal_plan();
        plan.plan.branch = "bad branch".to_string();
        let err = validate_plan(&plan).unwrap_err();
        assert!(err.to_string().contains("Branch"));
    }

    #[test]
    fn test_initial_task_states() {
        let plan = minimal_plan();
        let orch = Orchestrator::new(plan, PathBuf::from("/tmp/test"));
        assert_eq!(orch.tasks[0].status, TaskStatus::Pending);
        assert_eq!(orch.tasks[1].status, TaskStatus::Blocked);
    }

    #[test]
    fn test_counts() {
        let plan = minimal_plan();
        let orch = Orchestrator::new(plan, PathBuf::from("/tmp/test"));
        let (p, b, r, d, f) = orch.counts();
        assert_eq!((p, b, r, d, f), (1, 1, 0, 0, 0));
    }

    #[test]
    fn test_load_plan_from_toml() {
        let toml = r#"
[plan]
name = "test"

[[tasks]]
name = "alpha"
prompt = "Do alpha"

[[tasks]]
name = "beta"
prompt = "Do beta"
depends_on = ["alpha"]
model = "sonnet"
"#;
        let plan: Plan = toml::from_str(toml).unwrap();
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[1].depends_on, vec!["alpha"]);
        assert_eq!(plan.tasks[1].model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn test_task_status_symbols() {
        assert_eq!(TaskStatus::Pending.symbol(), "◌");
        assert_eq!(TaskStatus::Running.symbol(), "●");
        assert_eq!(TaskStatus::Done.symbol(), "✓");
        assert_eq!(TaskStatus::Failed.symbol(), "✗");
        assert_eq!(TaskStatus::Blocked.symbol(), "⊘");
    }

    #[test]
    fn test_generate_summary() {
        let plan = minimal_plan();
        let mut orch = Orchestrator::new(plan, PathBuf::from("/tmp/test"));
        orch.tasks[0].status = TaskStatus::Done;
        orch.tasks[0].output_lines = vec!["line 1".to_string(), "line 2".to_string()];
        orch.tasks[1].status = TaskStatus::Failed;

        let summary = orch.generate_summary();
        assert!(summary.contains("# Orchestration Summary"));
        assert!(summary.contains("test-plan"));
        assert!(summary.contains("✓ task-a"));
        assert!(summary.contains("✗ task-b"));
    }

    #[test]
    fn test_unblock_ready() {
        let plan = minimal_plan();
        let mut orch = Orchestrator::new(plan, PathBuf::from("/tmp/test"));
        assert_eq!(orch.tasks[1].status, TaskStatus::Blocked);

        // Simulate task-a completing
        orch.tasks[0].status = TaskStatus::Done;
        orch.unblock_ready();

        assert_eq!(orch.tasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn test_propagate_failures() {
        let plan = minimal_plan();
        let mut orch = Orchestrator::new(plan, PathBuf::from("/tmp/test"));
        assert_eq!(orch.tasks[1].status, TaskStatus::Blocked);

        // Simulate task-a failing
        orch.tasks[0].status = TaskStatus::Failed;
        orch.propagate_failures();

        assert_eq!(orch.tasks[1].status, TaskStatus::Failed);
        assert!(orch.tasks[1].output_lines[0].contains("dependency"));
    }

    #[test]
    fn test_skip_permissions_default_false() {
        let toml = r#"
[plan]
name = "test"

[[tasks]]
name = "a"
prompt = "A"
"#;
        let plan: Plan = toml::from_str(toml).unwrap();
        assert!(!plan.plan.skip_permissions);
    }

    #[test]
    fn test_branch_name_shared() {
        let plan = minimal_plan();
        let orch = Orchestrator::new(plan, PathBuf::from("/tmp/test"));
        assert_eq!(orch.branch_name, "orchestrate");
    }
}
