"""End-to-end tests for the Codex hook asset's `usage` action.

The shell hook is exercised against a fake herdr socket instead of mocked
internals, because the Rust CLI harness in `tests/cli` only compiles on Linux.
"""

import datetime
import json
import os
import socket
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path


ASSET_DIR = Path(__file__).parents[1] / "src/integration/assets/codex"
ASSET = ASSET_DIR / "herdr-agent-state.sh"
POWERSHELL_ASSET = ASSET_DIR / "herdr-agent-state.ps1"

EPOCH = datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)
# Every test time is an offset from this instant.
BASE_MS = 1_790_000_000_000


def iso(ms):
    moment = EPOCH + datetime.timedelta(milliseconds=ms)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}Z"


def token_count(ms, input_tokens=84_000, window=258_400, info="usage"):
    """A rollout `token_count` event line."""
    if info == "usage":
        payload_info = {
            "last_token_usage": {
                "input_tokens": input_tokens,
                "cached_input_tokens": input_tokens // 2,
                "output_tokens": 10,
            },
            "total_token_usage": {"input_tokens": input_tokens * 3},
        }
        if window is not None:
            payload_info["model_context_window"] = window
    else:
        payload_info = info
    return {
        "timestamp": iso(ms),
        "type": "event_msg",
        "payload": {"type": "token_count", "info": payload_info},
    }


def other_event(ms):
    return {
        "timestamp": iso(ms),
        "type": "event_msg",
        "payload": {"type": "agent_message", "message": "hello"},
    }


class FakeHerdrSocket:
    """Accepts one request per connection and records each JSON line."""

    def __init__(self):
        self.directory = tempfile.mkdtemp(prefix="hcs")
        self.path = os.path.join(self.directory, "s")
        self.requests = []
        self._stop = threading.Event()
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(self.path)
        self._listener.listen(8)
        self._listener.settimeout(0.1)
        self._thread = threading.Thread(target=self._serve, daemon=True)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *_):
        self._stop.set()
        self._thread.join(timeout=2)
        self._listener.close()
        try:
            os.unlink(self.path)
            os.rmdir(self.directory)
        except OSError:
            pass

    def _serve(self):
        while not self._stop.is_set():
            try:
                connection, _ = self._listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with connection:
                connection.settimeout(1.0)
                data = b""
                try:
                    while not data.endswith(b"\n"):
                        chunk = connection.recv(65536)
                        if not chunk:
                            break
                        data += chunk
                except OSError:
                    continue
                if not data.strip():
                    continue
                request = json.loads(data.decode())
                self.requests.append(request)
                reply = {"id": request.get("id"), "result": {"type": "ok"}}
                try:
                    connection.sendall((json.dumps(reply) + "\n").encode())
                except OSError:
                    pass


def run_hook(
    payload,
    transcript_lines=None,
    transcript_text=None,
    extra_env=None,
    action="usage",
):
    """Runs the hook; returns (requests the fake socket received, stdout)."""
    with tempfile.TemporaryDirectory(prefix="hct") as work:
        transcript_path = os.path.join(work, "rollout.jsonl")
        if transcript_lines is not None:
            with open(transcript_path, "w", encoding="utf-8") as handle:
                for line in transcript_lines:
                    handle.write(json.dumps(line) + "\n")
        elif transcript_text is not None:
            with open(transcript_path, "w", encoding="utf-8") as handle:
                handle.write(transcript_text)
        payload = dict(payload)
        if transcript_lines is not None or transcript_text is not None:
            payload.setdefault("transcript_path", transcript_path)
        elif payload.get("transcript_path") == "<missing>":
            payload["transcript_path"] = os.path.join(work, "does-not-exist.jsonl")
        with FakeHerdrSocket() as fake:
            env = {
                "HERDR_ENV": "1",
                "HERDR_SOCKET_PATH": fake.path,
                "HERDR_PANE_ID": "p1",
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "TMPDIR": tempfile.gettempdir(),
            }
            env.update(extra_env or {})
            result = subprocess.run(
                ["sh", str(ASSET), action],
                input=json.dumps(payload),
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode != 0:
                raise AssertionError(
                    f"hook exited {result.returncode}: {result.stderr}"
                )
            return list(fake.requests), result.stdout


def stop_payload(**extra):
    payload = {"hook_event_name": "Stop", "session_id": "codex-session"}
    payload.update(extra)
    return payload


@unittest.skipIf(os.name == "nt", "runs the POSIX shell asset against a Unix-socket fake server")
class CodexUsageHookTests(unittest.TestCase):
    def test_stop_reports_last_token_count_with_window(self):
        older, newer = BASE_MS, BASE_MS + 30_000
        lines = [
            token_count(older, input_tokens=1_000, window=200_000),
            other_event(older + 1_000),
            token_count(newer, input_tokens=84_000, window=258_400),
            other_event(newer + 1_000),
        ]

        requests, _ = run_hook(stop_payload(), lines)

        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["method"], "pane.report_context_usage")
        params = requests[0]["params"]
        self.assertEqual(params["pane_id"], "p1")
        self.assertEqual(params["source"], "herdr:codex")
        self.assertEqual(params["used_tokens"], 84_000)
        self.assertEqual(params["window_tokens"], 258_400)
        self.assertEqual(params["observed_at_ms"], newer)

    def test_missing_window_is_omitted(self):
        for window in (None, 0, "258400", True):
            with self.subTest(window=window):
                lines = [token_count(BASE_MS, input_tokens=5_000, window=window)]
                requests, _ = run_hook(stop_payload(), lines)
                self.assertEqual(len(requests), 1, requests)
                self.assertEqual(requests[0]["params"]["used_tokens"], 5_000)
                self.assertNotIn("window_tokens", requests[0]["params"])

    def test_null_info_and_other_events_are_skipped(self):
        good_ms = BASE_MS
        lines = [
            token_count(good_ms, input_tokens=7_000),
            token_count(good_ms + 1_000, info=None),
            other_event(good_ms + 2_000),
            token_count(good_ms + 3_000, info={"last_token_usage": {"input_tokens": -1}}),
            token_count(good_ms + 4_000, info={"last_token_usage": {"input_tokens": "9"}}),
            token_count(good_ms + 5_000, info={"model_context_window": 1000}),
        ]

        requests, _ = run_hook(stop_payload(), lines)

        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["params"]["used_tokens"], 7_000)
        self.assertEqual(requests[0]["params"]["observed_at_ms"], good_ms)

        requests, _ = run_hook(
            stop_payload(), [other_event(good_ms), token_count(good_ms, info=None)]
        )
        self.assertEqual(requests, [])

    def test_unparseable_timestamp_entries_are_skipped(self):
        broken = token_count(BASE_MS + 9_000, input_tokens=9_999)
        broken["timestamp"] = "not a time"
        lines = [token_count(BASE_MS, input_tokens=3_000), broken]

        requests, _ = run_hook(stop_payload(), lines)

        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["params"]["used_tokens"], 3_000)

    def test_only_stop_events_report(self):
        lines = [token_count(BASE_MS)]

        for event in ("SessionStart", "UserPromptSubmit", "PreToolUse"):
            with self.subTest(event=event):
                requests, _ = run_hook(stop_payload(hook_event_name=event), lines)
                self.assertEqual(requests, [])
        requests, _ = run_hook({"session_id": "codex-session"}, lines)
        self.assertEqual(requests, [])

    def test_nested_thread_guard(self):
        lines = [token_count(BASE_MS)]

        requests, _ = run_hook(
            stop_payload(), lines, extra_env={"CODEX_THREAD_ID": "another-session"}
        )
        self.assertEqual(requests, [])

        requests, _ = run_hook(
            stop_payload(), lines, extra_env={"CODEX_THREAD_ID": "codex-session"}
        )
        self.assertEqual(len(requests), 1, requests)

    def test_missing_rollout_or_transcript_path_stays_silent(self):
        self.assertEqual(run_hook(stop_payload(transcript_path="<missing>"))[0], [])
        self.assertEqual(run_hook(stop_payload())[0], [])

    def test_outside_herdr_stays_silent(self):
        lines = [token_count(BASE_MS)]
        requests, _ = run_hook(stop_payload(), lines, extra_env={"HERDR_ENV": "0"})
        self.assertEqual(requests, [])

    def test_hook_writes_nothing_to_stdout(self):
        lines = [token_count(BASE_MS)]

        reported, stdout = run_hook(stop_payload(), lines)
        self.assertEqual(len(reported), 1)
        self.assertEqual(stdout, "")

        silent, stdout = run_hook(stop_payload(), [other_event(BASE_MS)])
        self.assertEqual(silent, [])
        self.assertEqual(stdout, "")

    def test_tail_only_for_large_rollouts(self):
        filler = json.dumps({"type": "response_item", "padding": "x" * 1000})
        filler_block = "\n".join([filler] * 400) + "\n"
        self.assertGreaterEqual(len(filler_block), 262_144 + 1)
        recent = token_count(BASE_MS + 5_000, input_tokens=2_222)
        old = token_count(BASE_MS, input_tokens=1_111)

        reported, _ = run_hook(
            stop_payload(), transcript_text=filler_block + json.dumps(recent) + "\n"
        )
        head_only, _ = run_hook(
            stop_payload(), transcript_text=json.dumps(old) + "\n" + filler_block
        )

        self.assertEqual(len(reported), 1)
        self.assertEqual(reported[0]["params"]["used_tokens"], 2_222)
        self.assertEqual(head_only, [])

    def test_session_action_still_reports_the_session(self):
        requests, _ = run_hook(
            {
                "hook_event_name": "SessionStart",
                "session_id": "codex-session",
                "transcript_path": "/tmp/codex-session.jsonl",
            },
            action="session",
        )

        self.assertEqual(len(requests), 1)
        self.assertEqual(requests[0]["method"], "pane.report_agent_session")
        self.assertEqual(requests[0]["params"]["agent_session_id"], "codex-session")

    def test_session_action_ignores_stop_events(self):
        requests, _ = run_hook(
            stop_payload(transcript_path="/tmp/codex-session.jsonl"), action="session"
        )
        self.assertEqual(requests, [])

    def test_assets_carry_version_nine(self):
        for asset in (ASSET, POWERSHELL_ASSET):
            self.assertIn("# HERDR_INTEGRATION_VERSION=9", asset.read_text("utf-8"))

    def test_powershell_asset_reports_through_the_cli(self):
        text = POWERSHELL_ASSET.read_text("utf-8")
        self.assertIn("report-context-usage", text)
        self.assertIn("herdr:codex", text)


if __name__ == "__main__":
    unittest.main()
