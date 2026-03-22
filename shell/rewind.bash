#!/usr/bin/env bash
# Rewind Rail — Bash shell integration
#
# Source this file in your .bashrc:
#   source ~/.config/rewind/rewind.bash
#
# Emits structured events to the rewind daemon over a Unix socket
# on every command start (DEBUG trap) and command end (PROMPT_COMMAND).

# ── Configuration ────────────────────────────────────────────────────────────

REWIND_SOCKET="${REWIND_SOCKET:-$HOME/.rewind/ingest.sock}"
REWIND_ENABLED="${REWIND_ENABLED:-1}"

# ── Internal state ───────────────────────────────────────────────────────────

_rewind_cmd_start_ts=0
_rewind_initialized=0
_rewind_in_preexec=0
_rewind_last_cmd=""

# ── Helpers ──────────────────────────────────────────────────────────────────

_rewind_send() {
    local msg="$1"

    if [[ ! -S "$REWIND_SOCKET" ]]; then
        return 0
    fi

    if command -v socat &>/dev/null; then
        echo "$msg" | socat - UNIX-CONNECT:"$REWIND_SOCKET" 2>/dev/null &
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
        ' "$REWIND_SOCKET" "$msg" &
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
" "$REWIND_SOCKET" "$msg" &
        disown 2>/dev/null
    fi
}

_rewind_tty() {
    tty 2>/dev/null || echo "unknown"
}

_rewind_json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    # Bash doesn't have $'\n' replacement as easily, use printf
    s=$(printf '%s' "$s" | tr '\n' ' ' | tr '\r' ' ' | tr '\t' ' ')
    echo "$s"
}

# ── Hooks ────────────────────────────────────────────────────────────────────

_rewind_init() {
    [[ "$REWIND_ENABLED" != "1" ]] && return
    [[ "$_rewind_initialized" == "1" ]] && return
    _rewind_initialized=1

    local tty
    tty=$(_rewind_tty)
    local ts
    ts=$(date +%s)
    local pid=$$

    _rewind_send "{\"type\":\"session_start\",\"tty\":\"$tty\",\"pid\":$pid,\"shell\":\"bash\",\"ts\":$ts}"
}

_rewind_debug_trap() {
    [[ "$REWIND_ENABLED" != "1" ]] && return

    # Avoid re-entrancy and skip PROMPT_COMMAND itself
    [[ "$_rewind_in_preexec" == "1" ]] && return
    [[ "$BASH_COMMAND" == "$PROMPT_COMMAND" ]] && return
    [[ "$BASH_COMMAND" == _rewind_* ]] && return

    _rewind_in_preexec=1

    local cmd
    cmd=$(_rewind_json_escape "$BASH_COMMAND")
    local cwd
    cwd=$(_rewind_json_escape "$PWD")
    local tty
    tty=$(_rewind_tty)
    local ts
    ts=$(date +%s)
    local pid=$$

    _rewind_cmd_start_ts=$ts
    _rewind_last_cmd="$BASH_COMMAND"

    _rewind_send "{\"type\":\"cmd_start\",\"cmd\":\"$cmd\",\"cwd\":\"$cwd\",\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\"}"
}

_rewind_prompt_command() {
    local exit_code=$?

    [[ "$REWIND_ENABLED" != "1" ]] && return

    # Initialize on first prompt
    _rewind_init

    # Reset preexec guard
    _rewind_in_preexec=0

    # Only send cmd_end if we had a cmd_start
    [[ "$_rewind_cmd_start_ts" == "0" ]] && return

    local tty
    tty=$(_rewind_tty)
    local ts
    ts=$(date +%s)
    local pid=$$

    local output_lines=0
    if (( ts - _rewind_cmd_start_ts > 0 )); then
        output_lines=$(( (ts - _rewind_cmd_start_ts) * 10 ))
        (( output_lines > 500 )) && output_lines=500
    fi

    _rewind_send "{\"type\":\"cmd_end\",\"exit_code\":$exit_code,\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\",\"output_lines\":$output_lines}"

    _rewind_cmd_start_ts=0
}

# ── Register hooks ───────────────────────────────────────────────────────────

# Use DEBUG trap for preexec equivalent
trap '_rewind_debug_trap' DEBUG

# Prepend to PROMPT_COMMAND (preserve existing)
if [[ -z "$PROMPT_COMMAND" ]]; then
    PROMPT_COMMAND="_rewind_prompt_command"
else
    PROMPT_COMMAND="_rewind_prompt_command;$PROMPT_COMMAND"
fi

# Send session_end on exit
trap '_rewind_send "{\"type\":\"session_end\",\"tty\":\"$(_rewind_tty)\",\"pid\":$$,\"ts\":$(date +%s)}"' EXIT
