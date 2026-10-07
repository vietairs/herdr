#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=codex
# HERDR_INTEGRATION_VERSION=9

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-codex-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|usage) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import datetime
import json
import os
import random
import socket
import time

EPOCH = datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)
# The rollout grows for the whole session; only its tail holds the newest event.
ROLLOUT_TAIL_BYTES = 262144

source = "herdr:codex"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

hook_event_name = str(hook_input.get("hook_event_name") or "")
# Session reports keep tolerating a payload without an event name; usage
# reports only run from the Stop hook.
allowed_events = {"session": {"SessionStart", ""}, "usage": {"Stop"}}
if hook_event_name not in allowed_events.get(action, set()):
    raise SystemExit(0)


def parse_timestamp_ms(value):
    """Milliseconds since the epoch for an ISO-8601 string, else None."""
    if not isinstance(value, str):
        return None
    try:
        text = value[:-1] + "+00:00" if value.endswith("Z") else value
        moment = datetime.datetime.fromisoformat(text)
        if moment.tzinfo is None:
            moment = moment.replace(tzinfo=datetime.timezone.utc)
        return (moment - EPOCH) // datetime.timedelta(milliseconds=1)
    except Exception:
        return None


def is_count(value):
    return isinstance(value, int) and not isinstance(value, bool)


def read_rollout_tail_lines(path):
    """Lines of the rollout tail, newest first; the cut first line is dropped."""
    try:
        with open(path, "rb") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            start = max(0, size - ROLLOUT_TAIL_BYTES)
            handle.seek(start)
            data = handle.read()
    except Exception:
        return []
    lines = data.decode("utf-8", errors="replace").split("\n")
    if start > 0:
        lines = lines[1:]
    lines.reverse()
    return lines


def latest_context_usage(path):
    """(used tokens, window tokens or None, observed ms) of the newest reading."""
    for line in read_rollout_tail_lines(path):
        try:
            entry = json.loads(line)
        except Exception:
            continue
        if not isinstance(entry, dict) or entry.get("type") != "event_msg":
            continue
        payload = entry.get("payload")
        if not isinstance(payload, dict) or payload.get("type") != "token_count":
            continue
        info = payload.get("info")
        if not isinstance(info, dict):
            continue
        last_usage = info.get("last_token_usage")
        used = last_usage.get("input_tokens") if isinstance(last_usage, dict) else None
        if not is_count(used) or used < 0:
            continue
        observed_ms = parse_timestamp_ms(entry.get("timestamp"))
        if observed_ms is None:
            continue
        window = info.get("model_context_window")
        return used, window if is_count(window) and window > 0 else None, observed_ms
    return None


request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
transcript_path = hook_input.get("transcript_path")
if not isinstance(transcript_path, str) or not transcript_path.strip():
    raise SystemExit(0)
inherited_session_id = os.environ.get("CODEX_THREAD_ID")
if inherited_session_id and inherited_session_id != agent_session_id:
    raise SystemExit(0)
if action == "usage":
    reading = latest_context_usage(transcript_path)
    if reading is None:
        raise SystemExit(0)
    used_tokens, window_tokens, observed_at_ms = reading
    usage_params = {
        "pane_id": pane_id,
        "source": source,
        "used_tokens": used_tokens,
        "observed_at_ms": observed_at_ms,
    }
    if window_tokens is not None:
        usage_params["window_tokens"] = window_tokens
    request = {
        "id": request_id,
        "method": "pane.report_context_usage",
        "params": usage_params,
    }
elif agent_session_id:
    session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
    if not isinstance(session_start_source, str) or not session_start_source:
        session_start_source = None
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "codex",
        "seq": report_seq,
        "agent_session_id": agent_session_id,
    }
    if session_start_source:
        params["session_start_source"] = session_start_source
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": params,
    }
else:
    raise SystemExit(0)

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
PY
