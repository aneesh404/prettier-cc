#!/usr/bin/env zsh
# Rewind Rail — Zsh shell integration
#
# Source this file in your .zshrc:
#   source ~/.config/rewind/rewind.zsh
#
# Emits structured events to the rewind daemon over a Unix socket
# on every command start (preexec) and command end (precmd).

# ── Configuration ────────────────────────────────────────────────────────────

REWIND_SOCKET="${REWIND_SOCKET:-$HOME/.rewind/ingest.sock}"
REWIND_ENABLED="${REWIND_ENABLED:-1}"

# ── Internal state ───────────────────────────────────────────────────────────

_rewind_cmd_start_ts=0
_rewind_initialized=0
_rewind_last_histcnt=0

# ── Helpers ──────────────────────────────────────────────────────────────────

_rewind_send() {
    # Send a JSON message to the daemon socket (fire-and-forget).
    # Uses /dev/udp or socat or perl — whatever is available.
    local msg="$1"

    if [[ ! -S "$REWIND_SOCKET" ]]; then
        return 0
    fi

    # Try socat first (most reliable), then perl, then python
    if command -v socat &>/dev/null; then
        echo "$msg" | socat - UNIX-CONNECT:"$REWIND_SOCKET" 2>/dev/null &!
    elif command -v perl &>/dev/null; then
        perl -e '
            use IO::Socket::UNIX;
            my $sock = IO::Socket::UNIX->new(
                Type => SOCK_STREAM,
                Peer => $ARGV[0],
            ) or exit 0;
            print $sock $ARGV[1] . "\n";
            close $sock;
        ' "$REWIND_SOCKET" "$msg" &!
    elif command -v python3 &>/dev/null; then
        python3 -c "
import socket, sys
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.send((sys.argv[2] + '\n').encode())
    s.close()
except: pass
" "$REWIND_SOCKET" "$msg" &!
    fi
}

_rewind_tty() {
    # Get the current TTY path.
    if [[ -n "$TTY" ]]; then
        echo "$TTY"
    else
        tty 2>/dev/null || echo "unknown"
    fi
}

_rewind_json_escape() {
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

_rewind_init() {
    [[ "$REWIND_ENABLED" != "1" ]] && return
    [[ "$_rewind_initialized" == "1" ]] && return
    _rewind_initialized=1

    local tty=$(_rewind_tty)
    local ts=$(date +%s)
    local shell="zsh"
    local pid=$$

    _rewind_send "{\"type\":\"session_start\",\"tty\":\"$tty\",\"pid\":$pid,\"shell\":\"$shell\",\"ts\":$ts}"
}

_rewind_preexec() {
    [[ "$REWIND_ENABLED" != "1" ]] && return

    local cmd=$(_rewind_json_escape "$1")
    local cwd=$(_rewind_json_escape "$PWD")
    local tty=$(_rewind_tty)
    local ts=$(date +%s)
    local pid=$$

    _rewind_cmd_start_ts=$ts
    _rewind_last_histcnt=$HISTCMD

    _rewind_send "{\"type\":\"cmd_start\",\"cmd\":\"$cmd\",\"cwd\":\"$cwd\",\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\"}"
}

_rewind_precmd() {
    local exit_code=$?

    [[ "$REWIND_ENABLED" != "1" ]] && return

    # Initialize on first precmd
    _rewind_init

    # Only send cmd_end if we had a cmd_start
    [[ "$_rewind_cmd_start_ts" == "0" ]] && return

    local tty=$(_rewind_tty)
    local ts=$(date +%s)
    local pid=$$

    # Estimate output lines: difference in cursor position is hard to get,
    # so we use a heuristic based on LINES and time elapsed.
    # A more accurate approach would use terminal queries, but this is
    # good enough for density visualization.
    local output_lines=0
    if (( ts - _rewind_cmd_start_ts > 0 )); then
        # Rough heuristic: assume ~10 lines/sec for interactive commands
        # This will be refined when we integrate with Ghostty's scrollback
        output_lines=$(( (ts - _rewind_cmd_start_ts) * 10 ))
        # Cap at something reasonable
        (( output_lines > 500 )) && output_lines=500
    fi

    _rewind_send "{\"type\":\"cmd_end\",\"exit_code\":$exit_code,\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\",\"output_lines\":$output_lines}"

    _rewind_cmd_start_ts=0
}

# ── Register hooks ───────────────────────────────────────────────────────────

# Zsh has built-in hook arrays
autoload -Uz add-zsh-hook
add-zsh-hook preexec _rewind_preexec
add-zsh-hook precmd _rewind_precmd

# Send session_end on shell exit
_rewind_exit() {
    [[ "$REWIND_ENABLED" != "1" ]] && return
    local tty=$(_rewind_tty)
    local ts=$(date +%s)
    local pid=$$
    _rewind_send "{\"type\":\"session_end\",\"tty\":\"$tty\",\"pid\":$pid,\"ts\":$ts}"
}
add-zsh-hook zshexit _rewind_exit
