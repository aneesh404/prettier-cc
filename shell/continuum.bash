#!/usr/bin/env bash
# Continuum Rail — Bash shell integration
#
# Source this file in your .bashrc:
#   source ~/.config/continuum/continuum.bash
#
# Emits structured events to the continuum daemon over a Unix socket
# on every command start (DEBUG trap) and command end (PROMPT_COMMAND).

# ── Configuration ────────────────────────────────────────────────────────────

CONTINUUM_SOCKET="${CONTINUUM_SOCKET:-$HOME/.continuum/ingest.sock}"
CONTINUUM_ENABLED="${CONTINUUM_ENABLED:-1}"

# ── Internal state ───────────────────────────────────────────────────────────

_continuum_cmd_start_ts=0
_continuum_initialized=0
_continuum_in_preexec=0
_continuum_last_cmd=""

# ── Helpers ──────────────────────────────────────────────────────────────────

_continuum_send() {
    local msg="$1"

    if [[ ! -S "$CONTINUUM_SOCKET" ]]; then
        return 0
    fi

    if command -v socat &>/dev/null; then
        echo "$msg" | socat - UNIX-CONNECT:"$CONTINUUM_SOCKET" 2>/dev/null &
        disown 2>/dev/null
    elif command -v perl &>/dev/null; then
        perl -e '
            use IO::Socket::UNIX;
            my $sock = IO::Socket::UNIX->new(
                Type => SOCK_STREAM,
                Peer => $ARGV[0],
            ) or exit 0;
            print $sock $ARGV[1] . "\n";
            close $sock;
        ' "$CONTINUUM_SOCKET" "$msg" &
        disown 2>/dev/null
    elif command -v python3 &>/dev/null; then
        python3 -c "
import socket, sys
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.send((sys.argv[2] + '\n').encode())
    s.close()
except: pass
" "$CONTINUUM_SOCKET" "$msg" &
        disown 2>/dev/null
    fi
}

_continuum_tty() {
    tty 2>/dev/null || echo "unknown"
}

_continuum_json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    # Bash doesn't have $'\n' replacement as easily, use printf
    s=$(printf '%s' "$s" | tr '\n' ' ' | tr '\r' ' ' | tr '\t' ' ')
    echo "$s"
}

# ── Hooks ────────────────────────────────────────────────────────────────────

_continuum_init() {
    [[ "$CONTINUUM_ENABLED" != "1" ]] && return
    [[ "$_continuum_initialized" == "1" ]] && return
    _continuum_initialized=1

    local tty
    tty=$(_continuum_tty)
    local ts
    ts=$(date +%s)
    local pid=$$

    _continuum_send "{\"type\":\"session_start\",\"tty\":\"$tty\",\"pid\":$pid,\"shell\":\"bash\",\"ts\":$ts}"
}

_continuum_debug_trap() {
    [[ "$CONTINUUM_ENABLED" != "1" ]] && return

    # Avoid re-entrancy and skip PROMPT_COMMAND itself
    [[ "$_continuum_in_preexec" == "1" ]] && return
    [[ "$BASH_COMMAND" == "$PROMPT_COMMAND" ]] && return
    [[ "$BASH_COMMAND" == _continuum_* ]] && return

    _continuum_in_preexec=1

    local cmd
    cmd=$(_continuum_json_escape "$BASH_COMMAND")
    local cwd
    cwd=$(_continuum_json_escape "$PWD")
    local tty
    tty=$(_continuum_tty)
    local ts
    ts=$(date +%s)
    local pid=$$

    _continuum_cmd_start_ts=$ts
    _continuum_last_cmd="$BASH_COMMAND"

    _continuum_send "{\"type\":\"cmd_start\",\"cmd\":\"$cmd\",\"cwd\":\"$cwd\",\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\"}"
}

_continuum_prompt_command() {
    local exit_code=$?

    [[ "$CONTINUUM_ENABLED" != "1" ]] && return

    # Initialize on first prompt
    _continuum_init

    # Reset preexec guard
    _continuum_in_preexec=0

    # Only send cmd_end if we had a cmd_start
    [[ "$_continuum_cmd_start_ts" == "0" ]] && return

    local tty
    tty=$(_continuum_tty)
    local ts
    ts=$(date +%s)
    local pid=$$

    local output_lines=0
    if (( ts - _continuum_cmd_start_ts > 0 )); then
        output_lines=$(( (ts - _continuum_cmd_start_ts) * 10 ))
        (( output_lines > 500 )) && output_lines=500
    fi

    _continuum_send "{\"type\":\"cmd_end\",\"exit_code\":$exit_code,\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\",\"output_lines\":$output_lines}"

    _continuum_cmd_start_ts=0
}

# ── Register hooks ───────────────────────────────────────────────────────────

# Use DEBUG trap for preexec equivalent
trap '_continuum_debug_trap' DEBUG

# Prepend to PROMPT_COMMAND (preserve existing)
if [[ -z "$PROMPT_COMMAND" ]]; then
    PROMPT_COMMAND="_continuum_prompt_command"
else
    PROMPT_COMMAND="_continuum_prompt_command;$PROMPT_COMMAND"
fi

# Send session_end on exit
trap '_continuum_send "{\"type\":\"session_end\",\"tty\":\"$(_continuum_tty)\",\"pid\":$$,\"ts\":$(date +%s)}"' EXIT
