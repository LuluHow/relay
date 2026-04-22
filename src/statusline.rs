use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

// ── Data written by the hook, one file per session ─────────────────────────

fn sessions_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".relay").join("sessions"))
}

fn claude_settings_path() -> Option<PathBuf> {
    let dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"));
    Some(dir.join("settings.json"))
}

fn hook_script_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".relay").join("statusline-hook.sh"))
}

// ── Hook script ────────────────────────────────────────────────────────────

/// Generate the hook script. If `chain_cmd` is Some, the script will also
/// pipe stdin to the original command and print its output (wrapping it).
fn hook_script(chain_cmd: Option<&str>) -> String {
    let sessions = sessions_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "$HOME/.relay/sessions".to_string());

    let chain_block = if let Some(cmd) = chain_cmd {
        format!(
            r#"
# Chain: forward to original statusLine command
printf '%s' "$INPUT" | {cmd}
"#
        )
    } else {
        String::new()
    };

    format!(
        r#"#!/bin/bash
# relay — statusLine hook for Claude Code
# Writes per-session JSON to {sessions}/<session_id>.json
# Installed automatically by relay.

INPUT=""
while IFS= read -r -t 5 line || [ -n "$line" ]; do
    INPUT="${{INPUT}}${{line}}
"
done
[ -z "$INPUT" ] && exit 0

# Extract session data for relay TUI
printf '%s' "$INPUT" | python3 -c "
import sys, json, os
data = json.load(sys.stdin)
sid = data.get('session_id', '')
if not sid:
    sys.exit(0)
out = {{}}
out['session_id'] = sid
m = data.get('model', {{}})
out['model_id'] = m.get('id', '')
out['model_name'] = m.get('display_name', '')
cw = data.get('context_window', {{}})
out['context_used_pct'] = cw.get('used_percentage')
out['context_remaining_pct'] = cw.get('remaining_percentage')
out['context_window_size'] = cw.get('context_window_size')
out['total_input_tokens'] = cw.get('total_input_tokens', 0)
out['total_output_tokens'] = cw.get('total_output_tokens', 0)
cu = cw.get('current_usage') or {{}}
out['current_input'] = cu.get('input_tokens', 0)
out['current_output'] = cu.get('output_tokens', 0)
out['cache_read'] = cu.get('cache_read_input_tokens', 0)
out['cache_create'] = cu.get('cache_creation_input_tokens', 0)
c = data.get('cost', {{}})
out['cost_usd'] = c.get('total_cost_usd', 0)
out['duration_ms'] = c.get('total_duration_ms', 0)
out['api_duration_ms'] = c.get('total_api_duration_ms', 0)
out['lines_added'] = c.get('total_lines_added', 0)
out['lines_removed'] = c.get('total_lines_removed', 0)
rl = data.get('rate_limits', {{}})
fh = rl.get('five_hour')
if fh:
    out['five_hour_used_pct'] = fh.get('used_percentage', 0)
    out['five_hour_resets_at'] = fh.get('resets_at', 0)
sd = rl.get('seven_day')
if sd:
    out['seven_day_used_pct'] = sd.get('used_percentage', 0)
    out['seven_day_resets_at'] = sd.get('resets_at', 0)
out['cwd'] = data.get('cwd', '')
ws = data.get('workspace', {{}})
out['project_dir'] = ws.get('project_dir', '')
out['version'] = data.get('version', '')
out['exceeds_200k'] = data.get('exceeds_200k_tokens', False)
d = '{sessions}'
os.makedirs(d, exist_ok=True)
p = os.path.join(d, sid + '.json')
tmp = p + '.tmp'
with open(tmp, 'w') as f:
    json.dump(out, f)
os.replace(tmp, p)
" 2>/dev/null
{chain_block}"#
    )
}

// ── Deserialized session status ────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionStatus {
    pub session_id: String,
    pub model_id: String,
    pub model_name: String,
    // Context
    pub context_used_pct: Option<f64>,
    pub context_remaining_pct: Option<f64>,
    pub context_window_size: Option<u64>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub current_input: u64,
    pub current_output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    // Cost & timing
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub api_duration_ms: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
    // Rate limits (quota)
    pub five_hour_used_pct: Option<f64>,
    pub five_hour_resets_at: Option<u64>,
    pub seven_day_used_pct: Option<f64>,
    pub seven_day_resets_at: Option<u64>,
    // Misc
    pub cwd: String,
    pub project_dir: String,
    pub version: String,
    pub exceeds_200k: bool,
}


// ── Public API ─────────────────────────────────────────────────────────────

/// Install the statusLine hook. If another hook is already configured (e.g. abtop),
/// wrap it so both run: relay extracts data, then the original command gets stdin forwarded.
pub fn ensure_hook() -> bool {
    let Some(script_path) = hook_script_path() else {
        return false;
    };
    let Some(settings_path) = claude_settings_path() else {
        return false;
    };

    // Read existing settings
    let mut settings: serde_json::Value = if settings_path.exists() {
        std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        if let Some(parent) = settings_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        serde_json::json!({})
    };

    // Check if our hook is already installed (points to our script)
    let our_path = script_path.display().to_string();
    if let Some(existing) = settings.get("statusLine") {
        if let Some(cmd) = existing.get("command").and_then(|c| c.as_str()) {
            if cmd == our_path {
                // Already installed — check if script needs updating (rewrite it)
                let chain_cmd = read_chain_cmd(&script_path);
                let script = hook_script(chain_cmd.as_deref());
                let _ = std::fs::write(&script_path, script);
                return false;
            }
        }
    }

    // Detect existing hook command to chain
    let chain_cmd = settings
        .get("statusLine")
        .and_then(|sl| sl.get("command"))
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string());

    // Write our hook script (wrapping the existing one if any)
    if let Some(parent) = script_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let script = hook_script(chain_cmd.as_deref());
    if std::fs::write(&script_path, script).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
    }

    // Point statusLine to our script
    if let Some(obj) = settings.as_object_mut() {
        obj.insert(
            "statusLine".to_string(),
            serde_json::json!({
                "type": "command",
                "command": our_path
            }),
        );
    }

    if let Ok(json) = serde_json::to_string_pretty(&settings) {
        let _ = std::fs::write(&settings_path, json);
    }

    true
}

/// Read the chained command from an existing relay hook script (if any).
fn read_chain_cmd(script_path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(script_path).ok()?;
    // Look for our chain marker: `printf '%s' "$INPUT" | <original_cmd>`
    // after the "# Chain:" comment
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("printf '%s' \"$INPUT\" | ") && !trimmed.contains("python3") {
            return Some(trimmed.strip_prefix("printf '%s' \"$INPUT\" | ")?.to_string());
        }
    }
    None
}

/// Read all session status files, keyed by session_id
pub fn read_all() -> HashMap<String, SessionStatus> {
    let Some(dir) = sessions_dir() else {
        return HashMap::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return HashMap::new();
    };

    let mut map = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map_or(true, |e| e != "json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(status) = serde_json::from_str::<SessionStatus>(&content) {
                if !status.session_id.is_empty() {
                    map.insert(status.session_id.clone(), status);
                }
            }
        }
    }
    map
}

/// Clean up stale session files (older than 24h)
pub fn cleanup_stale() {
    let Some(dir) = sessions_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);

    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = path.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}
