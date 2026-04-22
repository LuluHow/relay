use std::path::{Path, PathBuf};

/// Infer context window size from model slug and observed token usage.
pub fn context_window(model: &str, observed_tokens: u64) -> u64 {
    let m = model.to_lowercase();
    if m.contains("[1m]") || m.contains("opus") || observed_tokens > 180_000 {
        1_000_000
    } else {
        200_000
    }
}

/// Format a duration in seconds into a human-readable short string.
pub fn format_duration(secs: u64) -> String {
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

/// Resolve the path to Claude Code's settings.json.
pub fn claude_settings_path() -> Option<PathBuf> {
    let dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"));
    Some(dir.join("settings.json"))
}

/// Safely truncate a session ID for display (avoids panic on short IDs).
pub fn short_session_id(session_id: &str) -> &str {
    &session_id[..8.min(session_id.len())]
}

/// Validate that a session ID contains only safe characters (alphanumeric, hyphens, underscores).
pub fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Write to a file atomically (write to tmp, then rename).
/// Prevents partial reads from concurrent processes.
pub fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Set restrictive file permissions (owner-only read/write).
#[cfg(unix)]
pub fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn set_private_permissions(_path: &Path) {}

/// Set restrictive directory permissions (owner-only rwx).
#[cfg(unix)]
pub fn set_private_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
pub fn set_private_dir_permissions(_path: &Path) {}
