use std::process::{Command, Stdio};

use crate::parser::ParsedSession;
use crate::statusline::SessionStatus;

/// Check if a directory is inside a git work tree.
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

    format!(
        "{prefix}: auto-commit ({reason}) [{} turns, {pct}%]",
        parsed.turn_count,
    )
}

fn context_window(model: &str, observed: u64) -> u64 {
    let m = model.to_lowercase();
    if m.contains("[1m]") || m.contains("opus") || observed > 180_000 {
        1_000_000
    } else {
        200_000
    }
}
