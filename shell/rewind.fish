# Rewind Rail — Fish shell integration
#
# Source this file in your config.fish:
#   source ~/.config/rewind/rewind.fish
#
# Emits structured events to the rewind daemon over a Unix socket
# on every command start and command end.

# ── Configuration ────────────────────────────────────────────────────────────

set -q REWIND_SOCKET; or set -gx REWIND_SOCKET "$HOME/.rewind/ingest.sock"
set -q REWIND_ENABLED; or set -gx REWIND_ENABLED 1

# ── Internal state ───────────────────────────────────────────────────────────

set -g _rewind_cmd_start_ts 0
set -g _rewind_initialized 0

# ── Helpers ──────────────────────────────────────────────────────────────────

function _rewind_send
    set -l msg $argv[1]

    if not test -S "$REWIND_SOCKET"
        return 0
    end

    if command -q socat
        echo "$msg" | socat - UNIX-CONNECT:"$REWIND_SOCKET" 2>/dev/null &
        disown 2>/dev/null
    else if command -q python3
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
    end
end

function _rewind_tty
    tty 2>/dev/null; or echo "unknown"
end

function _rewind_json_escape
    string replace -a '\\' '\\\\' -- $argv[1] | \
    string replace -a '"' '\\"' | \
    string replace -a \n '\\n' | \
    string replace -a \r '\\r' | \
    string replace -a \t '\\t'
end

# ── Hooks ────────────────────────────────────────────────────────────────────

function _rewind_init
    test "$REWIND_ENABLED" = "1"; or return
    test "$_rewind_initialized" = "1"; and return
    set -g _rewind_initialized 1

    set -l tty (_rewind_tty)
    set -l ts (date +%s)
    set -l pid %self

    _rewind_send "{\"type\":\"session_start\",\"tty\":\"$tty\",\"pid\":$pid,\"shell\":\"fish\",\"ts\":$ts}"
end

function _rewind_preexec --on-event fish_preexec
    test "$REWIND_ENABLED" = "1"; or return

    set -l cmd (_rewind_json_escape "$argv[1]")
    set -l cwd (_rewind_json_escape "$PWD")
    set -l tty (_rewind_tty)
    set -l ts (date +%s)
    set -l pid %self

    set -g _rewind_cmd_start_ts $ts

    _rewind_send "{\"type\":\"cmd_start\",\"cmd\":\"$cmd\",\"cwd\":\"$cwd\",\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\"}"
end

function _rewind_postexec --on-event fish_postexec
    set -l exit_code $status

    test "$REWIND_ENABLED" = "1"; or return

    # Initialize on first run
    _rewind_init

    # Only send if we had a start
    test "$_rewind_cmd_start_ts" = "0"; and return

    set -l tty (_rewind_tty)
    set -l ts (date +%s)
    set -l pid %self

    set -l output_lines 0
    if test (math "$ts - $_rewind_cmd_start_ts") -gt 0
        set output_lines (math "min(($ts - $_rewind_cmd_start_ts) * 10, 500)")
    end

    _rewind_send "{\"type\":\"cmd_end\",\"exit_code\":$exit_code,\"ts\":$ts,\"pid\":$pid,\"tty\":\"$tty\",\"output_lines\":$output_lines}"

    set -g _rewind_cmd_start_ts 0
end

function _rewind_exit --on-event fish_exit
    test "$REWIND_ENABLED" = "1"; or return

    set -l tty (_rewind_tty)
    set -l ts (date +%s)
    set -l pid %self

    _rewind_send "{\"type\":\"session_end\",\"tty\":\"$tty\",\"pid\":$pid,\"ts\":$ts}"
end
