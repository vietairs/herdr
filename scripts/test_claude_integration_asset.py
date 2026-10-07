"""End-to-end tests for the Claude Code hook asset.

The shell hook is exercised against a fake herdr socket instead of mocked
internals, because the Rust CLI harness in `tests/cli` only compiles on Linux.
"""

import datetime
import json
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path


ASSET_DIR = Path(__file__).parents[1] / "src/integration/assets/claude"
ASSET = ASSET_DIR / "herdr-agent-state.sh"
POWERSHELL_ASSET = ASSET_DIR / "herdr-agent-state.ps1"
TAP_ASSET = ASSET_DIR / "herdr-statusline-tap.sh"
TAP_FILE_NAME = "herdr-statusline-tap.sh"
TAP_SCRIPT = '[ -r "$0" ] && exec sh "$0" "$1"; exec sh -c "$1"'

EPOCH = datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)
# Every test time is an offset from this instant.
BASE_MS = 1_790_000_000_000


def iso(ms):
    moment = EPOCH + datetime.timedelta(milliseconds=ms)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}Z"


def assistant(
    ms,
    input_tokens=None,
    creation=None,
    read=None,
    ephemeral_5m=None,
    ephemeral_1h=None,
    sidechain=False,
    model="claude-opus-4",
):
    usage = {}
    if input_tokens is not None:
        usage["input_tokens"] = input_tokens
    if creation is not None:
        usage["cache_creation_input_tokens"] = creation
    if read is not None:
        usage["cache_read_input_tokens"] = read
    if ephemeral_5m is not None or ephemeral_1h is not None:
        usage["cache_creation"] = {
            "ephemeral_5m_input_tokens": ephemeral_5m or 0,
            "ephemeral_1h_input_tokens": ephemeral_1h or 0,
        }
    return {
        "type": "assistant",
        "isSidechain": sidechain,
        "timestamp": iso(ms),
        "message": {"model": model, "usage": usage},
    }


def user(ms, sidechain=False):
    return {
        "type": "user",
        "isSidechain": sidechain,
        "timestamp": iso(ms),
        "message": {"content": [{"type": "tool_result"}]},
    }


COMPACT_BOUNDARY = {"type": "system", "subtype": "compact_boundary", "isSidechain": False}


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


class StuckHerdrSocket:
    """Accepts connections and never replies or closes them."""

    def __init__(self):
        self.directory = tempfile.mkdtemp(prefix="hcs")
        self.path = os.path.join(self.directory, "s")
        self._connections = []
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
        for connection in self._connections:
            connection.close()
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
            self._connections.append(connection)


def shell_single_quote(value):
    """Mirror of the Rust `shell_single_quote` rule."""
    return "'" + value.replace("'", "'\"'\"'") + "'"


def tap_command(tap_path, original):
    """The exact settings command a statusline tap install writes."""
    return " ".join(
        [
            "sh -c",
            shell_single_quote(TAP_SCRIPT),
            shell_single_quote(str(tap_path)),
            shell_single_quote(original),
        ]
    )


def herdr_env(socket_path, **extra):
    env = {
        "HERDR_ENV": "1",
        "HERDR_SOCKET_PATH": socket_path,
        "HERDR_PANE_ID": "p1",
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "TMPDIR": tempfile.gettempdir(),
    }
    env.update(extra)
    return env


def run_tap(original, stdin, env, tap_path=TAP_ASSET):
    """Runs the wrapped statusline command the way the agent does."""
    return subprocess.run(
        ["sh", "-c", tap_command(tap_path, original)],
        input=stdin,
        env=env,
        capture_output=True,
        text=True,
        timeout=10,
    )


def wait_for_requests(fake, count, timeout=5.0):
    """Waits until `count` requests arrived; the reporter runs detached."""
    deadline = time.monotonic() + timeout
    while len(fake.requests) < count and time.monotonic() < deadline:
        time.sleep(0.02)
    return list(fake.requests)


def settle_requests(fake, quiet=1.0):
    """Requests received after a quiet period long enough for a reporter run."""
    time.sleep(quiet)
    return list(fake.requests)


STATUSLINE_FIXTURE = json.dumps(
    {
        "session_id": "s",
        "context_window": {
            "context_window_size": 200000,
            "current_usage": {
                "input_tokens": 5,
                "output_tokens": 9,
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 83895,
            },
        },
        "prompt_cache": {"ttl": "1h", "expires_at": 1760003600},
    }
)
# Echoes its stdin after "OUT:" and exits 3.
ECHO_STDIN = (
    "python3 -c 'import sys; d=sys.stdin.read(); "
    "sys.stdout.write(\"OUT:\"+d); sys.exit(3)'"
)


def run_hook(action, payload, transcript_lines=None, transcript_text=None, extra_env=None):
    """Runs the hook and returns the requests the fake socket received."""
    with tempfile.TemporaryDirectory(prefix="hct") as work:
        transcript_path = os.path.join(work, "transcript.jsonl")
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
            return list(fake.requests)


def stop_payload(**extra):
    payload = {"hook_event_name": "Stop"}
    payload.update(extra)
    return payload


def by_method(requests, method):
    return [request for request in requests if request["method"] == method]


class ClaudeIntegrationAssetTests(unittest.TestCase):
    def test_stop_reports_latest_cache_request_with_one_hour_ttl(self):
        t0, t1 = BASE_MS, BASE_MS + 60_000
        lines = [
            user(t0),
            assistant(t0 + 4_000, input_tokens=3, creation=500, read=100, ephemeral_5m=500),
            user(t1),
            assistant(
                t1 + 5_000,
                input_tokens=4,
                creation=200,
                read=800,
                ephemeral_1h=10,
            ),
        ]

        requests = run_hook("cache", stop_payload(), lines)

        self.assertEqual(
            [request["method"] for request in requests],
            ["pane.report_prompt_cache", "pane.report_context_usage"],
        )
        cache = requests[0]["params"]
        self.assertEqual(cache["pane_id"], "p1")
        self.assertEqual(cache["source"], "herdr:claude")
        self.assertEqual(cache["last_request_at_ms"], t1)
        self.assertEqual(cache["ttl_secs"], 3600)
        context = requests[1]["params"]
        self.assertEqual(context["pane_id"], "p1")
        self.assertEqual(context["source"], "herdr:claude")
        self.assertEqual(context["used_tokens"], 4 + 200 + 800)
        self.assertEqual(context["observed_at_ms"], t1 + 5_000)

    def test_context_usage_sums_input_and_cache_tokens_without_window(self):
        entry_ms = BASE_MS + 9_000
        lines = [
            user(BASE_MS),
            assistant(entry_ms, input_tokens=5, creation=100, read=83_895),
        ]

        requests = run_hook("cache", stop_payload(), lines)

        context = by_method(requests, "pane.report_context_usage")
        self.assertEqual(len(context), 1)
        self.assertEqual(context[0]["params"]["used_tokens"], 84_000)
        self.assertEqual(context[0]["params"]["observed_at_ms"], entry_ms)
        self.assertNotIn("window_tokens", context[0]["params"])

    def test_context_usage_reported_for_uncached_entry(self):
        lines = [user(BASE_MS), assistant(BASE_MS + 2_000, input_tokens=1200)]

        requests = run_hook("cache", stop_payload(), lines)

        self.assertEqual(by_method(requests, "pane.report_prompt_cache"), [])
        context = by_method(requests, "pane.report_context_usage")
        self.assertEqual(len(context), 1)
        self.assertEqual(context[0]["params"]["used_tokens"], 1200)
        self.assertNotIn("window_tokens", context[0]["params"])

    def test_compact_boundary_suppresses_stale_context(self):
        lines = [
            user(BASE_MS),
            assistant(BASE_MS + 1_000, input_tokens=2, creation=50, read=900),
            COMPACT_BOUNDARY,
        ]

        requests = run_hook("cache", stop_payload(), lines)

        self.assertEqual(len(by_method(requests, "pane.report_prompt_cache")), 1)
        self.assertEqual(by_method(requests, "pane.report_context_usage"), [])

    def test_prompt_cache_time_is_the_preceding_user_entry(self):
        t0 = BASE_MS
        lines = [
            user(t0),
            user(t0 + 2_000, sidechain=True),
            assistant(t0 + 18_000, input_tokens=1, creation=10, read=20, ephemeral_5m=10),
            assistant(t0 + 52_000, input_tokens=1, creation=10, read=20, ephemeral_5m=10),
        ]

        requests = run_hook("cache", stop_payload(), lines)

        cache = by_method(requests, "pane.report_prompt_cache")
        self.assertEqual(len(cache), 1)
        self.assertEqual(cache[0]["params"]["last_request_at_ms"], t0)

    def test_prompt_cache_time_falls_back_to_the_entry_without_a_user_line(self):
        entry_ms = BASE_MS + 7_000
        lines = [assistant(entry_ms, input_tokens=1, creation=10, read=20, ephemeral_5m=10)]

        requests = run_hook("cache", stop_payload(), lines)

        cache = by_method(requests, "pane.report_prompt_cache")
        self.assertEqual(len(cache), 1)
        self.assertEqual(cache[0]["params"]["last_request_at_ms"], entry_ms)

    def test_post_tool_use_without_ephemeral_split_omits_ttl(self):
        lines = [user(BASE_MS), assistant(BASE_MS + 3_000, input_tokens=1, read=900)]

        requests = run_hook(
            "cache", {"hook_event_name": "PostToolUse"}, lines
        )

        cache = by_method(requests, "pane.report_prompt_cache")
        self.assertEqual(len(cache), 1)
        self.assertNotIn("ttl_secs", cache[0]["params"])
        self.assertEqual(cache[0]["params"]["last_request_at_ms"], BASE_MS)

    def test_five_minute_ttl(self):
        lines = [
            user(BASE_MS),
            assistant(BASE_MS + 3_000, input_tokens=1, creation=300, ephemeral_5m=300),
        ]

        requests = run_hook("cache", stop_payload(), lines)

        cache = by_method(requests, "pane.report_prompt_cache")
        self.assertEqual(cache[0]["params"]["ttl_secs"], 300)

    def test_skips_sidechain_synthetic_and_uncached_entries(self):
        good_user = BASE_MS
        lines = [
            user(good_user),
            assistant(good_user + 1_000, input_tokens=2, creation=40, read=60, ephemeral_1h=40),
            assistant(good_user + 2_000, input_tokens=9, creation=9, read=9, sidechain=True),
            assistant(
                good_user + 3_000,
                input_tokens=8,
                creation=8,
                read=8,
                model="<synthetic>",
            ),
            assistant(good_user + 4_000, input_tokens=7, creation=0, read=0),
        ]

        requests = run_hook("cache", stop_payload(), lines)

        cache = by_method(requests, "pane.report_prompt_cache")
        self.assertEqual(len(cache), 1)
        self.assertEqual(cache[0]["params"]["last_request_at_ms"], good_user)
        self.assertEqual(cache[0]["params"]["ttl_secs"], 3600)
        context = by_method(requests, "pane.report_context_usage")
        self.assertEqual(len(context), 1)
        self.assertEqual(context[0]["params"]["used_tokens"], 7)
        self.assertEqual(context[0]["params"]["observed_at_ms"], good_user + 4_000)

    def test_reads_only_the_tail_of_large_transcripts(self):
        filler = json.dumps({"type": "attachment", "padding": "x" * 1000})
        filler_block = "\n".join([filler] * 300) + "\n"
        self.assertGreaterEqual(len(filler_block), 300_000)
        entry = assistant(BASE_MS + 5_000, input_tokens=1, creation=10, read=20, ephemeral_5m=10)
        old_entry = assistant(BASE_MS, input_tokens=1, creation=10, read=20, ephemeral_5m=10)

        reported = run_hook(
            "cache",
            stop_payload(),
            transcript_text=filler_block + json.dumps(entry) + "\n",
        )
        head_only = run_hook(
            "cache",
            stop_payload(),
            transcript_text=json.dumps(old_entry) + "\n" + filler_block,
        )

        self.assertEqual(len(by_method(reported, "pane.report_prompt_cache")), 1)
        self.assertEqual(head_only, [])

    def test_ignores_subagents_wrong_events_and_missing_transcript(self):
        lines = [user(BASE_MS), assistant(BASE_MS + 1_000, input_tokens=1, creation=10, read=20)]

        self.assertEqual(
            run_hook("cache", stop_payload(agent_id="agent-1"), lines), []
        )
        self.assertEqual(
            run_hook("cache", {"hook_event_name": "SessionStart"}, lines), []
        )
        self.assertEqual(
            run_hook("session", stop_payload(session_id="s1"), lines), []
        )
        self.assertEqual(
            run_hook("cache", stop_payload(cursor_version="2026.08.11"), lines), []
        )
        self.assertEqual(
            run_hook("cache", stop_payload(transcript_path="<missing>")), []
        )

    def test_session_action_still_reports_the_session(self):
        requests = run_hook(
            "session",
            {"hook_event_name": "SessionStart", "session_id": "claude-session"},
        )

        self.assertEqual(len(requests), 1)
        self.assertEqual(requests[0]["method"], "pane.report_agent_session")
        self.assertEqual(requests[0]["params"]["agent_session_id"], "claude-session")

    def test_assets_carry_version_eleven(self):
        for asset in (ASSET, POWERSHELL_ASSET):
            self.assertIn("# HERDR_INTEGRATION_VERSION=11", asset.read_text("utf-8"))


class ClaudeStatuslineTapTests(unittest.TestCase):
    def assert_passthrough(self, result, stdin=STATUSLINE_FIXTURE):
        self.assertEqual(result.stdout, "OUT:" + stdin, result.stderr)
        self.assertEqual(result.returncode, 3, result.stderr)

    def test_statusline_passes_stdin_stdout_and_exit_code_through(self):
        with FakeHerdrSocket() as fake:
            result = run_tap(ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path))
        self.assert_passthrough(result)

    def test_statusline_runs_the_original_with_sh_semantics(self):
        original = "echo 'a\\tb'"
        direct = subprocess.run(
            ["sh", "-c", original], capture_output=True, text=True, timeout=10
        )
        with FakeHerdrSocket() as fake:
            result = run_tap(original, STATUSLINE_FIXTURE, herdr_env(fake.path))
        self.assertEqual(result.stdout, direct.stdout)
        self.assertEqual(result.returncode, direct.returncode)

    def test_statusline_reports_exact_window_context_and_cache_expiry(self):
        with FakeHerdrSocket() as fake:
            before_ms = int(time.time() * 1000)
            result = run_tap(ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path))
            requests = wait_for_requests(fake, 2)
        self.assert_passthrough(result)

        context = by_method(requests, "pane.report_context_usage")
        self.assertEqual(len(context), 1, requests)
        params = context[0]["params"]
        self.assertEqual(params["pane_id"], "p1")
        self.assertEqual(params["source"], "herdr:claude")
        self.assertEqual(params["used_tokens"], 84_000)
        self.assertEqual(params["window_tokens"], 200_000)
        self.assertLess(abs(params["observed_at_ms"] - before_ms), 10_000)
        self.assertNotIn("clear", params)

        cache = by_method(requests, "pane.report_prompt_cache")
        self.assertEqual(len(cache), 1, requests)
        self.assertEqual(cache[0]["params"]["pane_id"], "p1")
        self.assertEqual(cache[0]["params"]["source"], "herdr:claude")
        self.assertEqual(cache[0]["params"]["last_request_at_ms"], 1_760_000_000_000)
        self.assertEqual(cache[0]["params"]["ttl_secs"], 3600)

    def test_statusline_with_tap_file_deleted_still_runs_the_original(self):
        with tempfile.TemporaryDirectory(prefix="hct") as work:
            missing = Path(work) / TAP_FILE_NAME
            with FakeHerdrSocket() as fake:
                result = run_tap(
                    ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path), tap_path=missing
                )
                requests = settle_requests(fake)
        self.assert_passthrough(result)
        self.assertEqual(requests, [])

    def test_statusline_ignores_an_old_hook_asset_beside_it(self):
        with tempfile.TemporaryDirectory(prefix="hct") as work:
            tap = Path(work) / TAP_FILE_NAME
            shutil.copyfile(TAP_ASSET, tap)
            (Path(work) / "herdr-agent-state.sh").write_text("exit 0\n", "utf-8")
            with FakeHerdrSocket() as fake:
                result = run_tap(
                    ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path), tap_path=tap
                )
                requests = wait_for_requests(fake, 2)
        self.assert_passthrough(result)
        self.assertEqual(
            sorted(request["method"] for request in requests),
            ["pane.report_context_usage", "pane.report_prompt_cache"],
        )

    def test_statusline_outside_herdr_runs_original_and_reports_nothing(self):
        with FakeHerdrSocket() as fake:
            env = {
                name: value
                for name, value in herdr_env(fake.path).items()
                if name != "HERDR_ENV"
            }
            result = run_tap(ECHO_STDIN, STATUSLINE_FIXTURE, env)
            requests = settle_requests(fake)
        self.assert_passthrough(result)
        self.assertEqual(requests, [])

    def test_statusline_clears_context_when_current_usage_is_null(self):
        payload = json.dumps(
            {"context_window": {"context_window_size": 200000, "current_usage": None}}
        )
        with FakeHerdrSocket() as fake:
            result = run_tap(ECHO_STDIN, payload, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assert_passthrough(result, payload)
        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["method"], "pane.report_context_usage")
        self.assertEqual(
            requests[0]["params"],
            {"pane_id": "p1", "source": "herdr:claude", "clear": True},
        )

    def test_statusline_ignores_missing_or_mistyped_fields(self):
        for payload in [json.dumps({"context_window": {"current_usage": "x"}}), "{}"]:
            with self.subTest(payload=payload):
                with FakeHerdrSocket() as fake:
                    result = run_tap(ECHO_STDIN, payload, herdr_env(fake.path))
                    requests = settle_requests(fake)
                self.assert_passthrough(result, payload)
                self.assertEqual(requests, [])

    def test_statusline_never_waits_for_a_stuck_socket(self):
        with StuckHerdrSocket() as stuck:
            started = time.monotonic()
            result = run_tap(ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(stuck.path))
            elapsed = time.monotonic() - started
        self.assert_passthrough(result)
        self.assertLess(elapsed, 1.0)

    def test_statusline_survives_unwritable_tmpdir(self):
        with tempfile.TemporaryDirectory(prefix="hct") as work:
            missing = os.path.join(work, "does-not-exist")
            with FakeHerdrSocket() as fake:
                result = run_tap(
                    ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path, TMPDIR=missing)
                )
        self.assert_passthrough(result)

    def test_tap_asset_carries_version_eleven(self):
        self.assertIn("# HERDR_INTEGRATION_VERSION=11", TAP_ASSET.read_text("utf-8"))


if __name__ == "__main__":
    unittest.main()
