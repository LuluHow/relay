use anyhow::{Context, Result};
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

# Context % to trigger handoff
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
    let content = std::fs::read_to_string(&p)
        .context(format!("Cannot read {}", p.display()))?;
    let config: Config = toml::from_str(&content)
        .context(format!("Invalid config in {}", p.display()))?;
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

    // Always (re)create the shell wrapper
    let wrapper = p.parent().unwrap().join("claude-wrapper.sh");
    std::fs::write(&wrapper, SHELL_WRAPPER)?;

    Ok(p)
}
