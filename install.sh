#!/usr/bin/env bash
set -euo pipefail

# relay — install script
# Usage: curl -fsSL https://raw.githubusercontent.com/LuluHow/relay/main/install.sh | bash

REPO="https://github.com/LuluHow/relay.git"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
RELAY_DIR="$HOME/.relay"

# ── Helpers ─────────────────────────────────────────────────────────────────

info()  { printf '\033[36m▸\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m!\033[0m %s\n' "$*"; }
die()   { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# ── Detect platform ────────────────────────────────────────────────────────

detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Darwin)  PLATFORM="macOS" ;;
        Linux)
            if grep -qi microsoft /proc/version 2>/dev/null; then
                PLATFORM="WSL"
            else
                PLATFORM="Linux"
            fi
            ;;
        MINGW*|MSYS*|CYGWIN*)
            die "Native Windows is not supported. Use WSL instead."
            ;;
        *)
            die "Unsupported OS: $OS"
            ;;
    esac

    info "Platform: $PLATFORM ($ARCH)"
}

# ── Check / install dependencies ───────────────────────────────────────────

check_deps() {
    # Git
    if ! command -v git &>/dev/null; then
        die "git is required. Install it first."
    fi

    # Rust
    if ! command -v cargo &>/dev/null; then
        info "Rust not found. Installing via rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck source=/dev/null
        source "$HOME/.cargo/env"
        ok "Rust installed"
    else
        ok "Rust found: $(rustc --version)"
    fi

    # Python3 (needed for hooks)
    if ! command -v python3 &>/dev/null; then
        warn "python3 not found. Hooks (auto-commit, statusline) won't work without it."
    fi
}

# ── Build ──────────────────────────────────────────────────────────────────

build_relay() {
    TMPDIR_RELAY="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR_RELAY"' EXIT
    local tmpdir="$TMPDIR_RELAY"

    info "Cloning relay..."
    git clone --depth 1 "$REPO" "$tmpdir/relay" 2>/dev/null

    info "Building (release)..."
    cd "$tmpdir/relay"

    # On Linux without D-Bus, skip notifications
    if [ "$PLATFORM" = "Linux" ] || [ "$PLATFORM" = "WSL" ]; then
        if ! pkg-config --exists dbus-1 2>/dev/null; then
            info "D-Bus not found, building without notifications..."
            cargo build --release --no-default-features 2>&1 | tail -1
        else
            cargo build --release 2>&1 | tail -1
        fi
    else
        cargo build --release 2>&1 | tail -1
    fi

    ok "Build complete"

    # Install binary
    mkdir -p "$INSTALL_DIR"
    cp target/release/relay "$INSTALL_DIR/relay"
    chmod +x "$INSTALL_DIR/relay"
    ok "Installed to $INSTALL_DIR/relay"
}

# ── Shell wrappers ────────────────────────────────────────────────────────

write_shell_wrappers() {
    mkdir -p "$RELAY_DIR"
    chmod 700 "$RELAY_DIR"

    # Bash/Zsh wrapper
    cat > "$RELAY_DIR/claude-wrapper.sh" << 'WRAPPER'
# relay — Claude Code wrapper for auto-handoff
# Sourced automatically by relay installer

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
WRAPPER

    # Fish wrapper
    cat > "$RELAY_DIR/claude-wrapper.fish" << 'WRAPPER'
# relay — Claude Code wrapper for auto-handoff (fish shell)
# Sourced automatically by relay installer

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
WRAPPER

    ok "Shell wrappers written to $RELAY_DIR/"
}

# ── Configure ──────────────────────────────────────────────────────────────

configure() {
    # Write shell wrappers directly (before relay init, so they're available immediately)
    write_shell_wrappers

    # Run relay init (config, hooks, etc.)
    "$INSTALL_DIR/relay" init

    # Detect shell and rc file
    local shell_name source_line
    shell_name="$(basename "${SHELL:-bash}")"

    case "$shell_name" in
        zsh)
            RC_FILE="$HOME/.zshrc"
            source_line='source ~/.relay/claude-wrapper.sh'
            ;;
        bash)
            RC_FILE="$HOME/.bashrc"
            source_line='source ~/.relay/claude-wrapper.sh'
            ;;
        fish)
            RC_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
            source_line='source ~/.relay/claude-wrapper.fish'
            ;;
        *)
            RC_FILE=""
            source_line=""
            ;;
    esac

    info "Detected shell: $shell_name"

    # Add source line to rc file
    if [ -n "$RC_FILE" ]; then
        # Ensure rc file parent directory exists (for fish)
        mkdir -p "$(dirname "$RC_FILE")"

        if [ -f "$RC_FILE" ] && grep -qF "$source_line" "$RC_FILE" 2>/dev/null; then
            ok "Shell wrapper already sourced in $RC_FILE"
        else
            printf '\n# relay — Claude Code handoff wrapper\n%s\n' "$source_line" >> "$RC_FILE"
            ok "Added shell wrapper to $RC_FILE"
        fi
    else
        warn "Unknown shell ($shell_name). Add one of these to your shell rc file:"
        warn "  bash/zsh: source ~/.relay/claude-wrapper.sh"
        warn "  fish:     source ~/.relay/claude-wrapper.fish"
    fi

    # Ensure ~/.local/bin is in PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        warn "$INSTALL_DIR is not in your PATH. Add it:"
        warn "  export PATH=\"$INSTALL_DIR:\$PATH\""
        if [ -n "$RC_FILE" ] && [ "$shell_name" != "fish" ]; then
            if ! grep -qF "$INSTALL_DIR" "$RC_FILE" 2>/dev/null; then
                printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$RC_FILE"
                ok "Added $INSTALL_DIR to PATH in $RC_FILE"
            fi
        elif [ "$shell_name" = "fish" ]; then
            if ! grep -qF "$INSTALL_DIR" "$RC_FILE" 2>/dev/null; then
                printf 'fish_add_path %s\n' "$INSTALL_DIR" >> "$RC_FILE"
                ok "Added $INSTALL_DIR to PATH in $RC_FILE"
            fi
        fi
    fi
}

# ── Main ───────────────────────────────────────────────────────────────────

main() {
    printf '\n  \033[1mrelay\033[0m — Claude Code session monitor & handoff tool\n\n'

    detect_platform
    check_deps
    build_relay
    configure

    printf '\n  \033[32m✓ Installation complete!\033[0m\n\n'
    printf '  Next steps:\n'
    if [ -n "${RC_FILE:-}" ]; then
        printf '    1. Restart your shell (or run: source %s)\n' "$RC_FILE"
    else
        printf '    1. Restart your shell\n'
    fi
    printf '    2. Run \033[1mrelay\033[0m to launch the TUI\n'
    printf '    3. Edit ~/.relay/config.toml to enable auto_handoff\n\n'
}

main "$@"
