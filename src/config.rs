use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Context % to trigger handoff (default: 75)
    pub threshold: u8,
    /// Max turns before handoff, 0 = disabled (default: 0)
    pub max_turns: u32,
    /// Poll interval in seconds (default: 10)
    pub interval: u64,
    /// Seconds to wait before restarting claude (default: 5)
    pub cooldown: u64,
    /// macOS notification on handoff (default: true)
    pub notify: bool,
    /// Auto-handoff from TUI: monitor, save, kill, restart (default: false)
    pub auto_handoff: bool,
    /// Git auto-commit on Stop hook (default: false)
    pub auto_commit: bool,
    /// Auto-commit before handoff (default: true)
    pub commit_before_handoff: bool,
    /// Prefix for auto-commit messages (default: "relay")
    pub commit_prefix: String,
    /// Play terminal bell on handoff / auto-commit (default: true)
    pub sound: bool,
    /// Discord webhook URL for handoff notifications (default: none)
    pub discord_webhook: Option<String>,
    /// Slack webhook URL for handoff notifications (default: none)
    pub slack_webhook: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: 75,
            max_turns: 0,
            interval: 10,
            cooldown: 5,
            notify: true,
            auto_handoff: false,
            auto_commit: false,
            commit_before_handoff: true,
            commit_prefix: "relay".to_string(),
            sound: true,
            discord_webhook: None,
            slack_webhook: None,
        }
    }
}

const DEFAULT_CONFIG: &str = "\
# relay — config
#
# All settings are optional. Defaults shown below.

# Auto-handoff: monitor sessions from the TUI, save handoff + kill + restart
# Requires sourcing the shell wrapper: source ~/.relay/claude-wrapper.sh
auto_handoff = false

# Context % used to trigger handoff
threshold = 75

# Max conversation turns before handoff (0 = disabled)
max_turns = 0

# Poll interval in seconds
interval = 10

# Pause before restart in seconds
cooldown = 5

# macOS notification on handoff
notify = true

# Git auto-commit: commit changes when Claude finishes a turn (Stop hook)
auto_commit = false

# Auto-commit before handoff (preserves work across sessions)
commit_before_handoff = true

# Prefix for auto-commit messages
commit_prefix = \"relay\"

# Play terminal bell sound on handoff / auto-commit
sound = true

# Discord webhook URL for handoff notifications (optional)
# Create one at: Discord > Server Settings > Integrations > Webhooks
# discord_webhook = \"https://discord.com/api/webhooks/...\"

# Slack webhook URL for handoff notifications (optional)
# Create one at: https://api.slack.com/messaging/webhooks
# slack_webhook = \"https://hooks.slack.com/services/...\"
";

const SHELL_WRAPPER: &str = r#"# relay — Claude Code wrapper for auto-handoff
# Source this file in your .zshrc:
#   source ~/.relay/claude-wrapper.sh

claude() {
  command claude "$@"
  # Reset terminal in case claude was killed mid-session
  stty sane 2>/dev/null
  while [ -f "$HOME/.relay/next_prompt" ]; do
    local prompt
    prompt="$(cat "$HOME/.relay/next_prompt")"
    rm -f "$HOME/.relay/next_prompt"
    [ -z "$prompt" ] && break
    printf '\n  \033[36mrelay:\033[0m handoff detected, restarting in 5s...\n\n'
    sleep 5
    command claude "$prompt"
    stty sane 2>/dev/null
  done
}
"#;

const FISH_WRAPPER: &str = r#"# relay — Claude Code wrapper for auto-handoff (fish shell)
# Source this file in your config.fish:
#   source ~/.relay/claude-wrapper.fish

function claude
    command claude $argv
    stty sane 2>/dev/null
    while test -f "$HOME/.relay/next_prompt"
        set -l prompt (cat "$HOME/.relay/next_prompt")
        rm -f "$HOME/.relay/next_prompt"
        test -z "$prompt"; and break
        printf '\n  \033[36mrelay:\033[0m handoff detected, restarting in 5s...\n\n'
        sleep 5
        command claude "$prompt"
        stty sane 2>/dev/null
    end
end
"#;

pub fn path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("No home directory")?
        .join(".relay")
        .join("config.toml"))
}

pub fn load() -> Result<Config> {
    let p = path()?;
    if !p.exists() {
        return Ok(Config::default());
    }
    let content = std::fs::read_to_string(&p).context(format!("Cannot read {}", p.display()))?;
    let config: Config =
        toml::from_str(&content).context(format!("Invalid config in {}", p.display()))?;
    Ok(config)
}

/// Create default config file if it doesn't exist. Returns the path.
pub fn ensure_default() -> Result<PathBuf> {
    let p = path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !p.exists() {
        std::fs::write(&p, DEFAULT_CONFIG)?;
    }

    // Always (re)create the shell wrappers
    let wrapper = p.parent().unwrap().join("claude-wrapper.sh");
    std::fs::write(&wrapper, SHELL_WRAPPER)?;
    let fish_wrapper = p.parent().unwrap().join("claude-wrapper.fish");
    std::fs::write(&fish_wrapper, FISH_WRAPPER)?;

    Ok(p)
}

/// Remove all traces of relay from the system.
pub fn uninstall() -> Result<()> {
    let home = dirs::home_dir().context("No home directory")?;
    let relay_dir = home.join(".relay");

    // 1. Clean relay hooks from ~/.claude/settings.json
    let claude_settings = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
        .join("settings.json");

    if claude_settings.exists() {
        if let Ok(content) = std::fs::read_to_string(&claude_settings) {
            if let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&content) {
                let mut changed = false;

                // Remove relay entries from hooks.Stop
                if let Some(stop_arr) = settings
                    .get_mut("hooks")
                    .and_then(|h| h.get_mut("Stop"))
                    .and_then(|a| a.as_array_mut())
                {
                    let before = stop_arr.len();
                    stop_arr.retain(|entry| {
                        !entry
                            .get("hooks")
                            .and_then(|h| h.as_array())
                            .map(|hooks| {
                                hooks.iter().any(|hook| {
                                    hook.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(|c| c.contains(".relay/"))
                                        .unwrap_or(false)
                                })
                            })
                            .unwrap_or(false)
                    });
                    if stop_arr.len() != before {
                        changed = true;
                    }
                }

                // Remove statusLine if it points to relay
                if let Some(sl) = settings.get("statusLine") {
                    if sl
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains(".relay/"))
                        .unwrap_or(false)
                    {
                        settings.as_object_mut().unwrap().remove("statusLine");
                        changed = true;
                    }
                }

                if changed {
                    if let Ok(json) = serde_json::to_string_pretty(&settings) {
                        let _ = std::fs::write(&claude_settings, json);
                    }
                    println!("  {} cleaned Claude Code hooks", "✓".green());
                }
            }
        }
    }

    // 2. Remove source line from shell rc files
    let source_line = "source ~/.relay/claude-wrapper.sh";
    for rc in &[".zshrc", ".bashrc"] {
        let rc_path = home.join(rc);
        if rc_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&rc_path) {
                if content.contains(source_line) {
                    let cleaned: String = content
                        .lines()
                        .filter(|l| !l.contains(source_line) && !l.contains("# relay"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = std::fs::write(&rc_path, cleaned.trim_end().to_string() + "\n");
                    println!("  {} cleaned ~/{rc}", "✓".green());
                }
            }
        }
    }

    // 3. Remove ~/.relay/ directory
    if relay_dir.exists() {
        std::fs::remove_dir_all(&relay_dir)?;
        println!("  {} removed ~/.relay/", "✓".green());
    }

    // 4. Remove the binary itself
    if let Ok(exe) = std::env::current_exe() {
        println!("  {} removing {}", "✓".green(), exe.display());
        // On Unix, a running binary can delete itself
        let _ = std::fs::remove_file(&exe);
    }

    println!();
    println!("  relay has been uninstalled. Restart your shell to complete.");

    Ok(())
}
