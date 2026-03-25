#!/usr/bin/env bash
set -euo pipefail

# Continuum — installer
# Usage: curl -fsSL https://continuum.dev/install.sh | sh

BOLD='\033[1m'
DIM='\033[2m'
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
RESET='\033[0m'

info()  { echo -e "${GREEN}●${RESET} $*"; }
warn()  { echo -e "${YELLOW}●${RESET} $*"; }
error() { echo -e "${RED}●${RESET} $*"; }
step()  { echo -e "${BOLD}→${RESET} $*"; }

CONTINUUM_DIR="$HOME/.continuum"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/continuum"
SHELL_DIR="$CONFIG_DIR"

echo ""
echo -e "${BOLD}Continuum${RESET} — terminal history navigation"
echo ""

# ── 1. Create directories ───────────────────────────────────────────────────

step "Creating directories..."
mkdir -p "$CONTINUUM_DIR/sessions"
mkdir -p "$CONFIG_DIR"
info "Data dir: $CONTINUUM_DIR"
info "Config dir: $CONFIG_DIR"

# ── 2. Build the daemon ─────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ -f "$SCRIPT_DIR/Cargo.toml" ]]; then
    step "Building continuumd from source..."
    if command -v cargo &>/dev/null; then
        cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml" 2>&1 | tail -3
        DAEMON_BIN="$SCRIPT_DIR/target/release/continuumd"
        if [[ -f "$DAEMON_BIN" ]]; then
            # Install to ~/.local/bin or /usr/local/bin
            INSTALL_DIR="$HOME/.local/bin"
            mkdir -p "$INSTALL_DIR"
            cp "$DAEMON_BIN" "$INSTALL_DIR/continuumd"
            chmod +x "$INSTALL_DIR/continuumd"
            info "Installed continuumd to $INSTALL_DIR/continuumd"
        else
            error "Build failed — binary not found"
            exit 1
        fi
    else
        error "cargo not found — install Rust first: https://rustup.rs"
        exit 1
    fi
else
    error "Cargo.toml not found — run install.sh from the project directory"
    exit 1
fi

# ── 3. Install shell hooks ──────────────────────────────────────────────────

step "Installing shell hooks..."
for shell_file in "$SCRIPT_DIR/shell"/continuum.*; do
    if [[ -f "$shell_file" ]]; then
        cp "$shell_file" "$SHELL_DIR/"
        info "Installed $(basename "$shell_file")"
    fi
done

# ── 4. Detect shell and add source line ─────────────────────────────────────

step "Configuring shell integration..."

CURRENT_SHELL="$(basename "${SHELL:-/bin/bash}")"
SOURCE_LINE=""
RC_FILE=""

case "$CURRENT_SHELL" in
    zsh)
        SOURCE_LINE="source \"$SHELL_DIR/continuum.zsh\""
        RC_FILE="$HOME/.zshrc"
        ;;
    bash)
        SOURCE_LINE="source \"$SHELL_DIR/continuum.bash\""
        RC_FILE="$HOME/.bashrc"
        ;;
    fish)
        SOURCE_LINE="source \"$SHELL_DIR/continuum.fish\""
        RC_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
        ;;
    *)
        warn "Unsupported shell: $CURRENT_SHELL"
        warn "Manually source the appropriate shell/continuum.* file"
        ;;
esac

if [[ -n "$RC_FILE" && -n "$SOURCE_LINE" ]]; then
    if [[ -f "$RC_FILE" ]] && grep -qF "continuum" "$RC_FILE" 2>/dev/null; then
        info "Shell hook already in $RC_FILE"
    else
        echo "" >> "$RC_FILE"
        echo "# Continuum — terminal history navigation" >> "$RC_FILE"
        echo "$SOURCE_LINE" >> "$RC_FILE"
        info "Added to $RC_FILE"
    fi
fi

# ── 5. Install default config ───────────────────────────────────────────────

if [[ ! -f "$CONFIG_DIR/config.toml" ]]; then
    cp "$SCRIPT_DIR/config/config.toml" "$CONFIG_DIR/config.toml"
    info "Default config at $CONFIG_DIR/config.toml"
fi

# ── 6. Set up launchd (macOS) or systemd (Linux) ────────────────────────────

step "Setting up auto-start..."

if [[ "$(uname)" == "Darwin" ]]; then
    PLIST_DIR="$HOME/Library/LaunchAgents"
    PLIST_FILE="$PLIST_DIR/dev.continuum.continuumd.plist"
    mkdir -p "$PLIST_DIR"

    cat > "$PLIST_FILE" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.continuum.continuumd</string>
    <key>ProgramArguments</key>
    <array>
        <string>$INSTALL_DIR/continuumd</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$CONTINUUM_DIR/continuumd.log</string>
    <key>StandardErrorPath</key>
    <string>$CONTINUUM_DIR/continuumd.err</string>
</dict>
</plist>
PLIST

    # Load the agent
    launchctl unload "$PLIST_FILE" 2>/dev/null || true
    launchctl load "$PLIST_FILE"
    info "launchd agent installed and started"

elif command -v systemctl &>/dev/null; then
    UNIT_DIR="$HOME/.config/systemd/user"
    UNIT_FILE="$UNIT_DIR/continuumd.service"
    mkdir -p "$UNIT_DIR"

    cat > "$UNIT_FILE" <<UNIT
[Unit]
Description=Continuum daemon
After=default.target

[Service]
ExecStart=$INSTALL_DIR/continuumd
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
UNIT

    systemctl --user daemon-reload
    systemctl --user enable --now continuumd.service
    info "systemd user service installed and started"
else
    warn "No service manager detected — start continuumd manually"
fi

# ── Done ─────────────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}${GREEN}✓ Continuum installed!${RESET}"
echo ""
echo "  Restart your shell or run:"
echo -e "    ${DIM}source $SHELL_DIR/continuum.$CURRENT_SHELL${RESET}"
echo ""
echo "  The daemon is running. Open Ghostty and run some commands."
echo "  The sidebar UI will be available in a future update."
echo ""
