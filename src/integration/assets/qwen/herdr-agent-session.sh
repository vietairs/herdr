#!/bin/sh
# managed by herdr; reinstalling the integration replaces this file.
# HERDR_INTEGRATION_ID=qwen
# HERDR_INTEGRATION_VERSION=2

case "${1:-}" in session|usage) ;; *) exit 0 ;; esac
[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
if [ -n "${HERDR_BIN_PATH:-}" ]; then
    [ -x "$HERDR_BIN_PATH" ] || exit 0
else
    command -v herdr >/dev/null 2>&1 || exit 0
fi
command -v python3 >/dev/null 2>&1 || exit 0

python3 -c '
import json
import os
import subprocess
import sys
import time


def non_negative_int(value):
    # bool is an int subclass but never a token count.
    if isinstance(value, int) and not isinstance(value, bool) and value >= 0:
        return value
    return None


def session_args(payload, command):
    session_id = payload.get("session_id")
    source = payload.get("source")
    if not isinstance(session_id, str) or not session_id:
        return None
    args = [
        command, "pane", "report-agent-session", os.environ["HERDR_PANE_ID"],
        "--source", "herdr:qwen", "--agent", "qwen",
        "--agent-session-id", session_id, "--seq", str(time.time_ns()),
    ]
    if source in ("startup", "resume", "clear", "compact", "branch"):
        args.extend(["--session-start-source", source])
    return args


def usage_args(payload, command):
    if payload.get("hook_event_name") != "Stop":
        return None
    used = non_negative_int(payload.get("input_tokens"))
    if used is None:
        return None
    args = [
        command, "pane", "report-context-usage", os.environ["HERDR_PANE_ID"],
        "--source", "herdr:qwen", "--used", str(used),
    ]
    window = non_negative_int(payload.get("context_limit"))
    if window:
        args.extend(["--window", str(window)])
    return args


try:
    payload = json.load(sys.stdin)
    command = os.environ.get("HERDR_BIN_PATH") or "herdr"
    build = usage_args if sys.argv[1] == "usage" else session_args
    args = build(payload, command)
    if args is None:
        raise ValueError
    subprocess.run(
        args,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=1,
        check=False,
    )
except Exception:
    pass
' "$1" 2>/dev/null || true
