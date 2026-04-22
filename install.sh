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

# ── Configure ──────────────────────────────────────────────────────────────

configure() {
    # Run relay init
    "$INSTALL_DIR/relay" init

    # Detect shell and rc file
    local shell_name rc_file
    shell_name="$(basename "${SHELL:-bash}")"

    case "$shell_name" in
        zsh)  rc_file="$HOME/.zshrc" ;;
        bash) rc_file="$HOME/.bashrc" ;;
        *)    rc_file="" ;;
    esac

    # Add source line if not already present
    if [ -n "$rc_file" ]; then
        local source_line='source ~/.relay/claude-wrapper.sh'
        if [ -f "$rc_file" ] && grep -qF "$source_line" "$rc_file" 2>/dev/null; then
            ok "Shell wrapper already in $rc_file"
        else
            printf '\n# relay — Claude Code handoff wrapper\n%s\n' "$source_line" >> "$rc_file"
            ok "Added shell wrapper to $rc_file"
        fi
    else
        warn "Unknown shell ($shell_name). Add this to your shell rc file manually:"
        warn "  source ~/.relay/claude-wrapper.sh"
    fi

    # Ensure ~/.local/bin is in PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        warn "$INSTALL_DIR is not in your PATH. Add it:"
        warn "  export PATH=\"$INSTALL_DIR:\$PATH\""
        if [ -n "$rc_file" ] && ! grep -qF "$INSTALL_DIR" "$rc_file" 2>/dev/null; then
            printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$rc_file"
            ok "Added $INSTALL_DIR to PATH in $rc_file"
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
    printf '    1. Restart your shell (or run: source %s)\n' "${rc_file:-~/.bashrc}"
    printf '    2. Run \033[1mrelay\033[0m to launch the TUI\n'
    printf '    3. Edit ~/.relay/config.toml to enable auto_handoff\n\n'
}

main "$@"
