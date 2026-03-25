#!/usr/bin/env zsh
# Continuum Rail — Zsh shell integration
#
# Source this file in your .zshrc:
#   source ~/.config/continuum/continuum.zsh
#
# Emits structured events to the continuum daemon over a Unix socket
# on every command start (preexec) and command end (precmd).

# ── Configuration ────────────────────────────────────────────────────────────

CONTINUUM_SOCKET="${CONTINUUM_SOCKET:-$HOME/.continuum/ingest.sock}"
CONTINUUM_ENABLED="${CONTINUUM_ENABLED:-1}"

# ── Internal state ───────────────────────────────────────────────────────────

_continuum_cmd_start_ts=0
_continuum_initialized=0
_continuum_last_histcnt=0

# ── Helpers ──────────────────────────────────────────────────────────────────

_continuum_send() {
    # Send a JSON message to the daemon socket (fire-and-forget).
    # Uses /dev/udp or socat or perl — whatever is available.
    local msg="$1"

    if [[ ! -S "$CONTINUUM_SOCKET" ]]; then
        return 0
    fi

    # Try socat first (most reliable), then perl, then python
    if command -v socat &>/dev/null; then
        echo "$msg" | socat - UNIX-CONNECT:"$CONTINUUM_SOCKET" 2>/dev/null &!
    elif command -v perl &>/dev/null; then
        perl -e '
            use IO::Socket::UNIX;
            my $sock = IO::Socket::UNIX->new(
                Type => SOCK_STREAM,
                Peer => $ARGV[0],
            ) or exit 0;
            print $sock $ARGV[1] . "\n";
            close $sock;
        ' "$CONTINUUM_SOCKET" "$msg" &!
    elif command -v python3 &>/dev/null; then
        python3 -c "
import socket, sys
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.send((sys.argv[2] + '\n').encode())
    s.close()
except: pass
" "$CONTINUUM_SOCKET" "$msg" &!
    fi
}

_continuum_tty() {
    # Get the current TTY path.
    if [[ -n "$TTY" ]]; then
        echo "$TTY"
    else
        tty 2>/dev/null || echo "unknown"
    fi
}

_continuum_json_escape() {
    # Minimal JSON string escaping.
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    echo "$s"
}

# ── Hooks ────────────────────────────────────────────────────────────────────

_continuum_init() {
    [[ "$CONTINUUM_ENABLED" != "1" ]] && return
    [[ "$_continuum_initialized" == "1" ]] && return
    _continuum_initialized=1

    local tty=$(_continuum_tty)
    local ts=$(date +%s)
    local shell="zsh"
    local pid=$$

    _continuum_send "{\"type\":\"session_start\",\"tty\":\"$tty\",\"pid\":$pid,\"shell\":\"$shell\",\"ts\":$ts}"
}

_continuum_preexec() {
    [[ "$CONTINUUM_ENABLED" != "1" ]] && return

    local cmd=$(_continuum_json_escape "$1")
    local cwd=$(_continuum_json_escape "$PWD")
    local tty=$(_continuum_tty)
    local ts=$(date +%s)
    local pid=$$

    _continuum_cmd_start_ts=$ts
    _continuum_last_histcnt=$HISTCMD

    _continuum_send "{\"type\":\"cmd_start\",\"cmd\":\"$cmd\",\"cwd\":\"$cwd\",\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\"}"
}

_continuum_precmd() {
    local exit_code=$?

    [[ "$CONTINUUM_ENABLED" != "1" ]] && return

    # Initialize on first precmd
    _continuum_init

    # Only send cmd_end if we had a cmd_start
    [[ "$_continuum_cmd_start_ts" == "0" ]] && return

    local tty=$(_continuum_tty)
    local ts=$(date +%s)
    local pid=$$

    # Estimate output lines: difference in cursor position is hard to get,
    # so we use a heuristic based on LINES and time elapsed.
    # A more accurate approach would use terminal queries, but this is
    # good enough for density visualization.
    local output_lines=0
    if (( ts - _continuum_cmd_start_ts > 0 )); then
        # Rough heuristic: assume ~10 lines/sec for interactive commands
        # This will be refined when we integrate with Ghostty's scrollback
        output_lines=$(( (ts - _continuum_cmd_start_ts) * 10 ))
        # Cap at something reasonable
        (( output_lines > 500 )) && output_lines=500
    fi

    _continuum_send "{\"type\":\"cmd_end\",\"exit_code\":$exit_code,\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\",\"output_lines\":$output_lines}"

    _continuum_cmd_start_ts=0
}

# ── Register hooks ───────────────────────────────────────────────────────────

# Zsh has built-in hook arrays
autoload -Uz add-zsh-hook
add-zsh-hook preexec _continuum_preexec
add-zsh-hook precmd _continuum_precmd

# Send session_end on shell exit
_continuum_exit() {
    [[ "$CONTINUUM_ENABLED" != "1" ]] && return
    local tty=$(_continuum_tty)
    local ts=$(date +%s)
    local pid=$$
    _continuum_send "{\"type\":\"session_end\",\"tty\":\"$tty\",\"pid\":$pid,\"ts\":$ts}"
}
add-zsh-hook zshexit _continuum_exit
