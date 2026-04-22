use std::path::PathBuf;

use serde::Deserialize;

use crate::util;

// ── Paths ──────────────────────────────────────────────────────────────────

fn events_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".relay").join("events"))
}

fn hooks_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".relay").join("hooks"))
}

// ── Hook script ────────────────────────────────────────────────────────────

fn stop_hook_script() -> String {
    let events = events_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "$HOME/.relay/events".to_string());

    format!(
        r#"#!/bin/bash
# relay — Stop hook for auto-commit signaling
# Writes a signal file when Claude finishes a turn.

INPUT=""
while IFS= read -r -t 2 line || [ -n "$line" ]; do
    INPUT="${{INPUT}}${{line}}
"
done
[ -z "$INPUT" ] && exit 0

printf '%s' "$INPUT" | python3 -c "
import sys, json, os, time
data = json.load(sys.stdin)
sid = data.get('session_id', '')
if not sid:
    sys.exit(0)
out = {{
    'session_id': sid,
    'cwd': data.get('cwd', ''),
    'project_dir': data.get('workspace', {{}}).get('project_dir', ''),
    'timestamp': time.time()
}}
d = '{events}'
os.makedirs(d, exist_ok=True)
p = os.path.join(d, sid + '_stop.json')
tmp = p + '.tmp'
with open(tmp, 'w') as f:
    json.dump(out, f)
os.replace(tmp, p)
" 2>/dev/null
"#
    )
}

// ── StopEvent ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StopEvent {
    pub session_id: String,
    pub cwd: String,
    pub project_dir: String,
    pub timestamp: f64,
}

impl Default for StopEvent {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            cwd: String::new(),
            project_dir: String::new(),
            timestamp: 0.0,
        }
    }
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Install the Stop hook in Claude Code settings.json and write the hook script.
/// Preserves existing hooks. Returns true if a new hook was installed.
pub fn ensure_hooks() -> bool {
    let Some(hooks_path) = hooks_dir() else {
        return false;
    };
    let Some(settings_path) = util::claude_settings_path() else {
        return false;
    };

    let script_path = hooks_path.join("stop.sh");
    let our_command = format!("bash {}", script_path.display());

    // Read existing settings (with file lock)
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

    // Check if our hook is already installed (new format: matcher + hooks array)
    let already_installed = settings
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|arr| arr.as_array())
        .map(|arr| {
            arr.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c == our_command)
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    // Always (re)write the script
    let _ = std::fs::create_dir_all(&hooks_path);
    let _ = std::fs::write(&script_path, stop_hook_script());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700));
    }

    if already_installed {
        return false;
    }

    // Add our hook entry to the hooks.Stop array (matcher + hooks format)
    let hook_entry = serde_json::json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": our_command
            }
        ]
    });

    if let Some(obj) = settings.as_object_mut() {
        let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
        if let Some(hooks_obj) = hooks.as_object_mut() {
            let stop_arr = hooks_obj
                .entry("Stop")
                .or_insert_with(|| serde_json::json!([]));
            if let Some(arr) = stop_arr.as_array_mut() {
                arr.push(hook_entry);
            }
        }
    }

    if let Ok(json) = serde_json::to_string_pretty(&settings) {
        let _ = util::write_atomic(&settings_path, json.as_bytes());
    }

    // Ensure events dir exists
    if let Some(dir) = events_dir() {
        let _ = std::fs::create_dir_all(dir);
    }

    true
}

/// Read all pending Stop events from ~/.relay/events/
pub fn read_stop_events() -> Vec<StopEvent> {
    let Some(dir) = events_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if !name.ends_with("_stop.json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(event) = serde_json::from_str::<StopEvent>(&content) {
                if util::is_valid_session_id(&event.session_id) {
                    events.push(event);
                }
            }
        }
    }
    events
}

/// Remove the signal file for a session after processing.
pub fn clear_event(session_id: &str) {
    if !util::is_valid_session_id(session_id) {
        return;
    }
    if let Some(dir) = events_dir() {
        let path = dir.join(format!("{session_id}_stop.json"));
        let _ = std::fs::remove_file(path);
    }
}
