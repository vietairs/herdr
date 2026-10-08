#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# runs the statusline command it wraps, unchanged, and reports context usage
# and prompt-cache expiry to herdr in the background.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=12
#
# usage: sh herdr-statusline-tap.sh ORIGINAL_COMMAND
# no `set -e`: every failure path must still run the original command.

statusline_report_py=$(cat <<'PY'
import datetime
import json
import os
import random
import socket
import stat
import sys
import tempfile
import time

source = "herdr:claude"
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
TTL_SECS = {"5m": 300, "1h": 3600}
# Only the end of a transcript is read: the newest entries hold the anchor.
TRANSCRIPT_TAIL_BYTES = 262144
EPOCH = datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)
SAFE_SESSION_CHARS = frozenset(
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-"
)


def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def token_count(value):
    return value if is_int(value) and value > 0 else 0


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


def is_cache_bearing(entry):
    """True for a real main-thread assistant entry that used the prompt cache."""
    if entry.get("type") != "assistant" or entry.get("isSidechain") is True:
        return False
    message = entry.get("message")
    if not isinstance(message, dict) or message.get("model") == "<synthetic>":
        return False
    usage = message.get("usage")
    if not isinstance(usage, dict):
        return False
    for key in ("cache_creation_input_tokens", "cache_read_input_tokens"):
        if is_int(usage.get(key)) and usage[key] > 0:
            return True
    return False


def transcript_anchor_ms(path):
    """End of Claude's last cached response, floored to a whole second, else None.

    The countdown counts from the end of the last response, matching statusline
    tools such as tokenline; flooring makes both show the same second.
    """
    try:
        with open(path, "rb") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            start = max(0, size - TRANSCRIPT_TAIL_BYTES)
            handle.seek(start)
            data = handle.read()
        lines = data.decode("utf-8", errors="replace").split("\n")
        if start > 0:
            lines = lines[1:]
        for line in reversed(lines):
            try:
                entry = json.loads(line)
            except Exception:
                continue
            if not isinstance(entry, dict) or not is_cache_bearing(entry):
                continue
            timestamp_ms = parse_timestamp_ms(entry.get("timestamp"))
            if timestamp_ms is not None:
                return timestamp_ms // 1000 * 1000
    except Exception:
        pass
    return None


def observed_change_ms(status, used_tokens, now_ms):
    """Second (ms) at which this session's token usage was first seen to change, else None.

    tokenline counts the cache countdown from when its statusline first sees a
    new response's usage when that is later than the transcript line, so herdr
    keeps the same per-session memory: "<used_tokens> <change_ms>" in a private
    per-user directory. The first run only records the usage; it is not a
    change. Any error turns the feature off for the run.
    """
    try:
        session_id = status.get("session_id")
        if (
            not isinstance(session_id, str)
            or not 0 < len(session_id) <= 128
            or session_id.strip(".") == ""
            or not all(char in SAFE_SESSION_CHARS for char in session_id)
        ):
            return None
        uid = os.getuid()
        directory = os.path.join(tempfile.gettempdir(), f"herdr-statusline-{uid}")
        try:
            os.mkdir(directory, 0o700)
        except FileExistsError:
            pass
        info = os.lstat(directory)
        if (
            not stat.S_ISDIR(info.st_mode)
            or info.st_uid != uid
            or info.st_mode & 0o022
        ):
            return None
        path = os.path.join(directory, session_id)
        stored_tokens = None
        stored_change_ms = 0
        try:
            with open(path, "r", encoding="ascii") as handle:
                fields = handle.read().split()
            if len(fields) == 2:
                stored_tokens, stored_change_ms = int(fields[0]), int(fields[1])
        except Exception:
            stored_tokens = None
            stored_change_ms = 0
        if stored_tokens == used_tokens:
            return stored_change_ms if stored_change_ms > 0 else None
        change_ms = 0 if stored_tokens is None else now_ms // 1000 * 1000
        temporary = f"{path}.{os.getpid()}.tmp"
        try:
            with open(temporary, "w", encoding="ascii") as handle:
                handle.write(f"{used_tokens} {change_ms}")
            os.replace(temporary, path)
        except Exception:
            try:
                os.remove(temporary)
            except Exception:
                pass
            return None
        return change_ms if change_ms > 0 else None
    except Exception:
        return None


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
    change_ms = None
    context = status.get("context_window")
    if isinstance(context, dict) and "current_usage" in context:
        usage = context["current_usage"]
        if usage is None:
            # Before the first reply and right after a compaction.
            send("pane.report_context_usage", {"clear": True})
            # Counted as zero usage, as tokenline does, so the first reply of a
            # new session is a change rather than a first sighting.
            change_ms = observed_change_ms(status, 0, int(time.time() * 1000))
        elif isinstance(usage, dict):
            used_tokens = (
                token_count(usage.get("input_tokens"))
                + token_count(usage.get("cache_creation_input_tokens"))
                + token_count(usage.get("cache_read_input_tokens"))
            )
            now_ms = int(time.time() * 1000)
            change_ms = observed_change_ms(status, used_tokens, now_ms)
            params = {"used_tokens": used_tokens, "observed_at_ms": now_ms}
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
            candidates = []
            transcript_path = status.get("transcript_path")
            if isinstance(transcript_path, str) and transcript_path:
                transcript_ms = transcript_anchor_ms(transcript_path)
                if transcript_ms is not None:
                    candidates.append(transcript_ms)
            if change_ms is not None:
                candidates.append(change_ms)
            anchor_ms = max(candidates) if candidates else None
            if anchor_ms is None:
                anchor_ms = (expires_at - ttl_secs) * 1000
            send(
                "pane.report_prompt_cache",
                {"last_request_at_ms": anchor_ms, "ttl_secs": ttl_secs},
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
