use std::process::{Command, Stdio};

use crate::parser::ParsedSession;
use crate::statusline::SessionStatus;

/// Check if a directory is inside a git work tree (via rev-parse).
pub fn is_git_repo(cwd: &str) -> bool {
    Command::new("git")
        .args(["-C", cwd, "rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Return true if the working tree has uncommitted changes (staged or unstaged).
pub fn has_uncommitted_changes(cwd: &str) -> bool {
    let output = Command::new("git")
        .args(["-C", cwd, "status", "--porcelain"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(o) => !o.stdout.is_empty(),
        Err(_) => false,
    }
}

/// Stage all changes and commit. Returns the short commit hash on success.
pub fn auto_commit(cwd: &str, message: &str) -> Result<String, String> {
    // Stage all changes (respects .gitignore)
    let add = Command::new("git")
        .args(["-C", cwd, "add", "-A"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();
    match add {
        Ok(s) if s.success() => {}
        Ok(_) => return Err("git add failed".to_string()),
        Err(e) => return Err(format!("git add: {e}")),
    }

    // Commit
    let commit = Command::new("git")
        .args(["-C", cwd, "commit", "-m", message])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let output = match commit {
        Ok(o) => o,
        Err(e) => return Err(format!("git commit: {e}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git commit: {}", stderr.trim()));
    }

    // Extract short hash
    let hash = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--short", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "?".to_string());

    Ok(hash)
}

/// Build a commit message from session context.
pub fn generate_commit_message(
    cwd: &str,
    parsed: &ParsedSession,
    status: Option<&SessionStatus>,
    reason: &str,
    prefix: &str,
) -> String {
    let window = context_window(&parsed.model, parsed.current_context_tokens);
    let pct = if window > 0 {
        (parsed.current_context_tokens as f64 / window as f64 * 100.0) as u8
    } else {
        status
            .and_then(|s| s.context_used_pct)
            .map(|p| p as u8)
            .unwrap_or(0)
    };

    // Get changed files from git status
    let changed_files = changed_files(cwd);
    let diff_stat = diff_stat(cwd);

    // Title line: prefix(reason): summary of what changed
    let title = if changed_files.is_empty() {
        format!("{prefix}({reason}): auto-commit [{} turns, {pct}%]", parsed.turn_count)
    } else {
        let summary = summarize_changes(&changed_files);
        format!("{prefix}({reason}): {summary}")
    };

    // Body
    let mut body = String::new();

    if !diff_stat.is_empty() {
        body.push_str(&format!("\n\n{diff_stat}"));
    }

    if !changed_files.is_empty() {
        body.push_str("\n\nFiles:\n");
        for f in &changed_files {
            body.push_str(&format!("  {} {}\n", f.status_char(), f.path));
        }
    }

    body.push_str(&format!(
        "\n[{} turns, {pct}% context]",
        parsed.turn_count,
    ));

    format!("{title}{body}")
}

struct ChangedFile {
    status: char,
    path: String,
}

impl ChangedFile {
    fn status_char(&self) -> char {
        self.status
    }

    fn extension(&self) -> &str {
        self.path.rsplit('.').next().unwrap_or("")
    }
}

/// Get the list of changed files from `git status --porcelain`.
fn changed_files(cwd: &str) -> Vec<ChangedFile> {
    let output = Command::new("git")
        .args(["-C", cwd, "status", "--porcelain"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let status = line.chars().next().unwrap_or('?');
            // Normalize: first char is index status, second is worktree
            let s = if status == ' ' {
                line.chars().nth(1).unwrap_or('?')
            } else {
                status
            };
            let path = line[3..].trim().to_string();
            Some(ChangedFile { status: s, path })
        })
        .collect()
}

/// Get a one-line diff stat (e.g. "3 files changed, 42 insertions(+), 10 deletions(-)")
fn diff_stat(cwd: &str) -> String {
    // Stat for staged + unstaged combined
    let output = Command::new("git")
        .args(["-C", cwd, "diff", "--stat", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return String::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    // Last line of --stat is the summary
    text.lines()
        .last()
        .map(|l| l.trim().to_string())
        .filter(|l| l.contains("changed"))
        .unwrap_or_default()
}

/// Summarize changes into a short title from the file list.
fn summarize_changes(files: &[ChangedFile]) -> String {
    if files.len() == 1 {
        let f = &files[0];
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let verb = match f.status {
            'A' | '?' => "add",
            'D' => "remove",
            'R' => "rename",
            _ => "update",
        };
        return format!("{verb} {name}");
    }

    // Group by extension or directory
    let extensions: Vec<&str> = files.iter().map(|f| f.extension()).collect();
    let all_same_ext = extensions.windows(2).all(|w| w[0] == w[1]);

    if files.len() <= 3 {
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.path.rsplit('/').next().unwrap_or(&f.path))
            .collect();
        return format!("update {}", names.join(", "));
    }

    if all_same_ext && !extensions.is_empty() {
        return format!("update {} {} files", files.len(), extensions[0]);
    }

    format!("update {} files", files.len())
}

fn context_window(model: &str, observed: u64) -> u64 {
    let m = model.to_lowercase();
    if m.contains("[1m]") || m.contains("opus") || observed > 180_000 {
        1_000_000
    } else {
        200_000
    }
}
