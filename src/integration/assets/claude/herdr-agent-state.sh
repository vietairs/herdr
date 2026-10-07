#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=11

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|cache) ;;
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

source = "herdr:claude"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

# Only the end of a transcript is read: the newest entries are all a report needs.
TRANSCRIPT_TAIL_BYTES = 262144
EPOCH = datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)

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

if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)
hook_event_name = str(hook_input.get("hook_event_name") or "")
expected_events = {"session": {"SessionStart"}, "cache": {"Stop", "PostToolUse"}}
if hook_event_name not in expected_events.get(action, set()):
    raise SystemExit(0)
is_subagent = bool(hook_input.get("agent_id"))
if is_subagent:
    raise SystemExit(0)


def new_request_id():
    return f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"


def send(request):
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


def positive_int(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def token_count(value):
    return value if positive_int(value) else 0


def read_transcript_entries(path):
    """Parsed JSON objects of the transcript tail, oldest first."""
    try:
        with open(path, "rb") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            start = max(0, size - TRANSCRIPT_TAIL_BYTES)
            handle.seek(start)
            data = handle.read()
    except Exception:
        return []
    lines = data.decode("utf-8", errors="replace").split("\n")
    if start > 0:
        lines = lines[1:]
    entries = []
    for line in lines:
        try:
            entry = json.loads(line)
        except Exception:
            continue
        if isinstance(entry, dict):
            entries.append(entry)
    return entries


def main_chain_assistant(entry):
    """(usage, timestamp_ms) of a real main-thread assistant entry, else None."""
    if entry.get("type") != "assistant" or entry.get("isSidechain") is True:
        return None
    message = entry.get("message")
    if not isinstance(message, dict) or message.get("model") == "<synthetic>":
        return None
    usage = message.get("usage")
    if not isinstance(usage, dict):
        return None
    timestamp_ms = parse_timestamp_ms(entry.get("timestamp"))
    if timestamp_ms is None:
        return None
    return usage, timestamp_ms


def latest_cache_request(entries):
    """(request time ms, ttl secs or None) of the newest cache-bearing entry."""
    for index in range(len(entries) - 1, -1, -1):
        found = main_chain_assistant(entries[index])
        if found is None:
            continue
        usage, entry_ms = found
        if not (
            positive_int(usage.get("cache_creation_input_tokens"))
            or positive_int(usage.get("cache_read_input_tokens"))
        ):
            continue
        # The request left when the user prompt or tool result before this
        # response arrived; later lines of one response trail it by seconds.
        request_ms = entry_ms
        for earlier in range(index - 1, -1, -1):
            candidate = entries[earlier]
            if candidate.get("type") != "user" or candidate.get("isSidechain") is True:
                continue
            candidate_ms = parse_timestamp_ms(candidate.get("timestamp"))
            if candidate_ms is not None:
                request_ms = candidate_ms
                break
        ttl_secs = None
        split = usage.get("cache_creation")
        if isinstance(split, dict):
            if positive_int(split.get("ephemeral_1h_input_tokens")):
                ttl_secs = 3600
            elif positive_int(split.get("ephemeral_5m_input_tokens")):
                ttl_secs = 300
        return request_ms, ttl_secs
    return None


def latest_context_usage(entries):
    """(used tokens, observed ms) of the newest main-thread entry, else None."""
    for entry in reversed(entries):
        if entry.get("type") == "system" and entry.get("subtype") == "compact_boundary":
            # The conversation was just compacted; earlier usage is stale.
            return None
        found = main_chain_assistant(entry)
        if found is None:
            continue
        usage, observed_ms = found
        input_tokens = usage.get("input_tokens")
        if not isinstance(input_tokens, int) or isinstance(input_tokens, bool):
            continue
        used = (
            token_count(input_tokens)
            + token_count(usage.get("cache_creation_input_tokens"))
            + token_count(usage.get("cache_read_input_tokens"))
        )
        return used, observed_ms
    return None


if action == "session":
    request_id = new_request_id()
    report_seq = time.time_ns()
    session_id = hook_input.get("session_id")
    agent_session_id = session_id if isinstance(session_id, str) and session_id else None
    transcript_path = hook_input.get("transcript_path")
    agent_session_path = transcript_path if isinstance(transcript_path, str) and transcript_path else None
    session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
    if not isinstance(session_start_source, str) or not session_start_source:
        session_start_source = None
    if agent_session_id:
        params = {
            "pane_id": pane_id,
            "source": source,
            "agent": "claude",
            "seq": report_seq,
            "agent_session_id": agent_session_id,
        }
        if agent_session_path:
            params["agent_session_path"] = agent_session_path
        if session_start_source:
            params["session_start_source"] = session_start_source
        send(
            {
                "id": request_id,
                "method": "pane.report_agent_session",
                "params": params,
            }
        )
    else:
        raise SystemExit(0)
else:
    transcript_path = hook_input.get("transcript_path")
    if not isinstance(transcript_path, str) or not transcript_path:
        raise SystemExit(0)
    entries = read_transcript_entries(transcript_path)
    cache_request = latest_cache_request(entries)
    if cache_request is not None:
        last_request_at_ms, ttl_secs = cache_request
        params = {
            "pane_id": pane_id,
            "source": source,
            "last_request_at_ms": last_request_at_ms,
        }
        if ttl_secs is not None:
            params["ttl_secs"] = ttl_secs
        send(
            {
                "id": new_request_id(),
                "method": "pane.report_prompt_cache",
                "params": params,
            }
        )
    context_usage = latest_context_usage(entries)
    if context_usage is not None:
        used_tokens, observed_at_ms = context_usage
        # Hooks never see the model's window; only tokens are reported.
        send(
            {
                "id": new_request_id(),
                "method": "pane.report_context_usage",
                "params": {
                    "pane_id": pane_id,
                    "source": source,
                    "used_tokens": used_tokens,
                    "observed_at_ms": observed_at_ms,
                },
            }
        )
PY
