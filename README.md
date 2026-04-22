# relay

CLI daemon that monitors [Claude Code](https://docs.anthropic.com/en/docs/claude-code) sessions and generates structured handoffs to preserve context across session boundaries.

When Claude Code approaches its context limit, relay saves the session state as a markdown handoff, optionally kills the session, and restarts it with the handoff injected — so you never lose your thread.

## Features

- **Real-time TUI** — monitor all active Claude Code sessions (context %, tokens, model, cost)
- **Auto-handoff** — detects context saturation, saves state, kills & restarts with continuity
- **Structured handoffs** — goal, current focus, last assistant state, recent tools, files touched
- **Git auto-commit** — commits work before handoff so nothing is lost
- **Hook integration** — installs Claude Code hooks (Stop, statusLine) automatically
- **Shell wrapper** — transparent `claude` wrapper that handles restart loops
- **Discord / Slack notifications** — webhook alerts on every handoff

## Install

### One-liner

```bash
curl -fsSL https://raw.githubusercontent.com/LuluHow/relay/main/install.sh | bash
```

### From source

```bash
git clone https://github.com/LuluHow/relay.git
cd relay
cargo build --release
mkdir -p ~/.local/bin
cp target/release/relay ~/.local/bin/

# Ensure ~/.local/bin is in your PATH
export PATH="$HOME/.local/bin:$PATH"

relay init
```

Then add to your shell rc file (`~/.zshrc` or `~/.bashrc`):

```bash
export PATH="$HOME/.local/bin:$PATH"
source ~/.relay/claude-wrapper.sh
```

### From GitHub releases

Download the binary for your platform from [Releases](https://github.com/LuluHow/relay/releases), then:

```bash
chmod +x relay
mkdir -p ~/.local/bin
mv relay ~/.local/bin/
export PATH="$HOME/.local/bin:$PATH"
relay init
# Add export PATH and source lines as shown above
```

## Usage

```bash
relay              # Launch TUI (default)
relay status       # Show active sessions
relay save         # Save handoff for most recent session
relay list         # List saved handoffs
relay restore <id> # Print handoff to stdout
relay init         # Create config + shell wrapper
relay test-notify  # Test Discord/Slack webhook notifications
relay uninstall    # Remove all traces of relay
relay --version    # Show version
```

### TUI keybindings

| Key | Action |
|-----|--------|
| `j/k` or `↑/↓` | Navigate sessions |
| `Tab` | Switch between Sessions / Handoffs tabs |
| `s` | Save handoff for selected session |
| `d` | Toggle detail/dashboard view |
| `a` | Toggle auto-handoff |
| `g` | Toggle git auto-commit |
| `i` | Toggle idle sessions visibility |
| `r` | Refresh |
| `1/2` | Jump to Sessions/Handoffs tab |
| `q` | Quit |

## Configuration

Config lives at `~/.relay/config.toml`:

```toml
# Auto-handoff: monitor, save, kill, restart
auto_handoff = false

# Context % to trigger handoff
threshold = 20

# Max conversation turns before handoff (0 = disabled)
max_turns = 0

# Poll interval in seconds
interval = 10

# Pause before restart in seconds
cooldown = 5

# Desktop notification on handoff
notify = true

# Git auto-commit when Claude finishes a turn
auto_commit = false

# Auto-commit before handoff
commit_before_handoff = true

# Prefix for auto-commit messages
commit_prefix = "relay"

# Terminal bell on handoff / auto-commit
sound = true

# Discord webhook URL for handoff notifications (optional)
# discord_webhook = "https://discord.com/api/webhooks/..."

# Slack webhook URL for handoff notifications (optional)
# slack_webhook = "https://hooks.slack.com/services/..."
```

### Webhook notifications (Discord / Slack)

relay can send a message to Discord or Slack whenever a handoff is triggered.

**Discord setup:**
1. In your Discord server, go to **Server Settings > Integrations > Webhooks**
2. Click **New Webhook**, choose a channel, copy the URL
3. Add to config:
   ```toml
   discord_webhook = "https://discord.com/api/webhooks/..."
   ```

**Slack setup:**
1. Create an [Incoming Webhook](https://api.slack.com/messaging/webhooks) for your workspace
2. Copy the webhook URL
3. Add to config:
   ```toml
   slack_webhook = "https://hooks.slack.com/services/..."
   ```

**Test your setup:**
```bash
relay test-notify
```

## Platform support

| Platform | Status | Notes |
|----------|--------|-------|
| macOS (Apple Silicon) | Fully supported | Primary development platform |
| macOS (Intel) | Fully supported | |
| Linux (x86_64) | Supported | Uses `/proc` for process detection |
| WSL | Supported | Detected automatically |
| Windows (native) | Not supported | Use WSL |

### Linux notes

- **Notifications** require D-Bus. Build without them: `cargo build --release --no-default-features`
- **Python 3** is required for hooks (Stop, statusLine)
- Process detection uses `/proc` instead of `lsof`

## How it works

```
┌──────────────┐    hooks     ┌───────────────┐
│  Claude Code  │───────────▶│     relay      │
│   (session)   │◀───────────│   (daemon)     │
└──────────────┘  restart    └───────────────┘
                                    │
                              ┌─────┴─────┐
                              │  handoff   │
                              │   (.md)    │
                              └───────────┘
```

1. **relay** installs hooks in Claude Code settings (`Stop`, `statusLine`)
2. Hooks write session events to `~/.relay/events/` and `~/.relay/sessions/`
3. The TUI polls these files + the JSONL session logs
4. When context reaches the threshold, relay:
   - Generates a structured handoff markdown
   - Git-commits uncommitted work (if enabled)
   - Writes the handoff prompt to `~/.relay/next_prompt`
   - Kills the Claude process
5. The shell wrapper detects `next_prompt` and restarts Claude with the handoff

## File structure

```
~/.relay/
├── config.toml              # Configuration
├── claude-wrapper.sh        # Shell wrapper (sourced in .zshrc/.bashrc)
├── statusline-hook.sh       # statusLine hook script
├── hooks/
│   └── stop.sh              # Stop hook script
├── events/                  # Stop event signals
├── sessions/                # Live session status (from statusLine hook)
└── handoffs/                # Saved handoff markdowns
```

## Contributing

AI-assisted contributions are welcome, but **must be disclosed** — mention it in your PR description.

## Acknowledgements

- [abtop](https://github.com/graykode/abtop) — inspiration for terminal-based AI monitoring

## License

MIT
