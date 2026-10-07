#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# runs the statusline command it wraps, unchanged, and reports context usage
# and prompt-cache expiry to herdr in the background.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=11
#
# usage: sh herdr-statusline-tap.sh ORIGINAL_COMMAND
# no `set -e`: every failure path must still run the original command.

statusline_report_py=$(cat <<'PY'
import json
import os
import random
import socket
import sys
import time

source = "herdr:claude"
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
TTL_SECS = {"5m": 300, "1h": 3600}


def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def token_count(value):
    return value if is_int(value) and value > 0 else 0


def send(method, params):
    request = {
        "id": f"{source}:{int(time.time() * 1000)}:{random.randrange(1000000):06d}",
        "method": method,
        "params": dict({"pane_id": pane_id, "source": source}, **params),
    }
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(0.5)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        try:
            client.recv(4096)
        except Exception:
            pass
        client.close()
    except Exception:
        pass


def report(status):
    context = status.get("context_window")
    if isinstance(context, dict) and "current_usage" in context:
        usage = context["current_usage"]
        if usage is None:
            # Before the first reply and right after a compaction.
            send("pane.report_context_usage", {"clear": True})
        elif isinstance(usage, dict):
            params = {
                "used_tokens": token_count(usage.get("input_tokens"))
                + token_count(usage.get("cache_creation_input_tokens"))
                + token_count(usage.get("cache_read_input_tokens")),
                "observed_at_ms": int(time.time() * 1000),
            }
            window = context.get("context_window_size")
            if is_int(window) and window > 0:
                params["window_tokens"] = window
            send("pane.report_context_usage", params)
    cache = status.get("prompt_cache")
    if isinstance(cache, dict):
        ttl = cache.get("ttl")
        ttl_secs = TTL_SECS.get(ttl) if isinstance(ttl, str) else None
        expires_at = cache.get("expires_at")
        if ttl_secs is not None and is_int(expires_at) and expires_at > ttl_secs:
            send(
                "pane.report_prompt_cache",
                {
                    "last_request_at_ms": (expires_at - ttl_secs) * 1000,
                    "ttl_secs": ttl_secs,
                },
            )


try:
    if pane_id and socket_path:
        status = json.loads(sys.stdin.buffer.read())
        if isinstance(status, dict):
            report(status)
except Exception:
    pass
PY
)

# --- herdr statusline passthrough begin ---
statusline_original="${1:-}"
[ -n "$statusline_original" ] || exit 0

# Replaces this process with the original. An executable file runs as it is,
# the way an agent that runs the command without a shell ran it; anything else
# is shell text.
statusline_exec_original() {
  if [ -f "$statusline_original" ] && [ -x "$statusline_original" ]; then
    exec "$statusline_original"
  fi
  exec sh -c "$statusline_original"
}

# The input is buffered so the reporter and the original each read all of it.
statusline_input=$(mktemp "${TMPDIR:-/tmp}/herdr-statusline.XXXXXX" 2>/dev/null) ||
  statusline_exec_original
# A run cancelled before the buffer is unlinked still removes it.
trap 'rm -f "$statusline_input"; exit 129' HUP
trap 'rm -f "$statusline_input"; exit 130' INT
trap 'rm -f "$statusline_input"; exit 143' TERM
cat >"$statusline_input" 2>/dev/null
# `command` keeps a failed redirection from exiting the shell.
if ! { command exec 3<"$statusline_input" 4<"$statusline_input"; } 2>/dev/null; then
  rm -f "$statusline_input"
  trap - HUP INT TERM
  statusline_exec_original
fi
rm -f "$statusline_input"
trap - HUP INT TERM

if [ "${HERDR_ENV:-}" = "1" ] && [ -n "${HERDR_SOCKET_PATH:-}" ] &&
  [ -n "${HERDR_PANE_ID:-}" ] && command -v python3 >/dev/null 2>&1; then
  # Detached: the statusline never waits for herdr, and the reporter never
  # writes to the agent.
  ( python3 -c "$statusline_report_py" <&3 >/dev/null 2>&1 3<&- 4<&- & )
fi
exec 3<&-
# The original replaces this process: same input bytes, its own output and
# exit status, and cancelling the tap cancels the original.
exec <&4 4<&-
statusline_exec_original
# --- herdr statusline passthrough end ---
