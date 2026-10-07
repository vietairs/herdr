"""End-to-end tests for the Antigravity CLI statusline tap asset.

The tap is run through `sh -c` exactly as the wrapped settings command runs
it, against a fake herdr socket, because the Rust CLI harness in `tests/cli`
only compiles on Linux.
"""

import json
import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from scripts.test_claude_integration_asset import (
    ECHO_STDIN,
    TAP_FILE_NAME,
    FakeHerdrSocket,
    StuckHerdrSocket,
    by_method,
    herdr_env,
    run_tap,
    settle_requests,
    wait_for_requests,
)

ASSET_DIR = Path(__file__).parents[1] / "src/integration/assets/antigravity_cli"
TAP_ASSET = ASSET_DIR / "herdr-statusline-tap.sh"
HOOK_ASSETS = (ASSET_DIR / "herdr-agent-state.sh", ASSET_DIR / "herdr-agent-state.ps1")
SOURCE = "herdr:antigravity_cli"

STATUSLINE_FIXTURE = json.dumps(
    {
        "session_id": "s",
        "context_window": {
            "context_window_size": 1000000,
            "current_usage": {
                "input_tokens": 5,
                "output_tokens": 9,
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 83895,
            },
        },
    }
)


def tap(original, stdin, env, tap_path=TAP_ASSET):
    return run_tap(original, stdin, env, tap_path=tap_path)


@unittest.skipIf(os.name == "nt", "runs the POSIX shell asset against a Unix-socket fake server")
class AntigravityStatuslineTapTests(unittest.TestCase):
    def assert_passthrough(self, result, stdin=STATUSLINE_FIXTURE):
        self.assertEqual(result.stdout, "OUT:" + stdin, result.stderr)
        self.assertEqual(result.returncode, 3, result.stderr)

    def test_statusline_passes_stdin_stdout_and_exit_code_through(self):
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path))
        self.assert_passthrough(result)

    def test_statusline_runs_the_original_with_sh_semantics(self):
        original = "echo 'a\\tb'"
        direct = subprocess.run(
            ["sh", "-c", original], capture_output=True, text=True, timeout=10
        )
        with FakeHerdrSocket() as fake:
            result = tap(original, STATUSLINE_FIXTURE, herdr_env(fake.path))
        self.assertEqual(result.stdout, direct.stdout)
        self.assertEqual(result.returncode, direct.returncode)

    def test_statusline_reports_exact_window_context(self):
        with FakeHerdrSocket() as fake:
            before_ms = int(time.time() * 1000)
            result = tap(ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assert_passthrough(result)

        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["method"], "pane.report_context_usage")
        params = requests[0]["params"]
        self.assertEqual(params["pane_id"], "p1")
        self.assertEqual(params["source"], SOURCE)
        self.assertEqual(params["used_tokens"], 84_000)
        self.assertEqual(params["window_tokens"], 1_000_000)
        self.assertLess(abs(params["observed_at_ms"] - before_ms), 10_000)
        self.assertNotIn("clear", params)

    def test_statusline_clears_context_when_current_usage_is_null(self):
        payload = json.dumps(
            {"context_window": {"context_window_size": 200000, "current_usage": None}}
        )
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, payload, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assert_passthrough(result, payload)
        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["method"], "pane.report_context_usage")
        self.assertEqual(
            requests[0]["params"],
            {"pane_id": "p1", "source": SOURCE, "clear": True},
        )

    def test_statusline_never_reports_prompt_cache(self):
        payload = json.dumps(
            {
                "context_window": {
                    "context_window_size": 200000,
                    "current_usage": {"input_tokens": 7},
                },
                "prompt_cache": {"ttl": "1h", "expires_at": 1760003600},
            }
        )
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, payload, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.5)
        self.assert_passthrough(result, payload)
        self.assertEqual(by_method(requests, "pane.report_prompt_cache"), [])
        self.assertEqual(len(by_method(requests, "pane.report_context_usage")), 1)

    def test_statusline_ignores_missing_or_mistyped_fields(self):
        for payload in [
            json.dumps({"context_window": {"current_usage": "x"}}),
            json.dumps({"context_window": "x"}),
            json.dumps([1, 2]),
            "not json",
            "{}",
        ]:
            with self.subTest(payload=payload):
                with FakeHerdrSocket() as fake:
                    result = tap(ECHO_STDIN, payload, herdr_env(fake.path))
                    requests = settle_requests(fake)
                self.assert_passthrough(result, payload)
                self.assertEqual(requests, [])

    def test_statusline_omits_a_mistyped_window(self):
        payload = json.dumps(
            {
                "context_window": {
                    "context_window_size": "200000",
                    "current_usage": {"input_tokens": 7},
                }
            }
        )
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, payload, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assert_passthrough(result, payload)
        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["params"]["used_tokens"], 7)
        self.assertNotIn("window_tokens", requests[0]["params"])

    def test_statusline_with_tap_file_deleted_still_runs_the_original(self):
        with tempfile.TemporaryDirectory(prefix="hct") as work:
            missing = Path(work) / TAP_FILE_NAME
            with FakeHerdrSocket() as fake:
                result = tap(
                    ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path), tap_path=missing
                )
                requests = settle_requests(fake)
        self.assert_passthrough(result)
        self.assertEqual(requests, [])

    def test_statusline_outside_herdr_runs_original_and_reports_nothing(self):
        with FakeHerdrSocket() as fake:
            env = {
                name: value
                for name, value in herdr_env(fake.path).items()
                if name != "HERDR_ENV"
            }
            result = tap(ECHO_STDIN, STATUSLINE_FIXTURE, env)
            requests = settle_requests(fake)
        self.assert_passthrough(result)
        self.assertEqual(requests, [])

    def test_statusline_never_waits_for_a_stuck_socket(self):
        with StuckHerdrSocket() as stuck:
            started = time.monotonic()
            result = tap(ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(stuck.path))
            elapsed = time.monotonic() - started
        self.assert_passthrough(result)
        self.assertLess(elapsed, 1.0)

    def test_tap_runs_from_a_copy_named_like_the_installed_file(self):
        with tempfile.TemporaryDirectory(prefix="hct") as work:
            installed = Path(work) / TAP_FILE_NAME
            shutil.copyfile(TAP_ASSET, installed)
            with FakeHerdrSocket() as fake:
                result = tap(
                    ECHO_STDIN, STATUSLINE_FIXTURE, herdr_env(fake.path), tap_path=installed
                )
                requests = wait_for_requests(fake, 1)
        self.assert_passthrough(result)
        self.assertEqual(len(requests), 1, requests)

    def test_assets_carry_version_four(self):
        for asset in (TAP_ASSET, *HOOK_ASSETS):
            with self.subTest(asset=asset.name):
                self.assertIn("HERDR_INTEGRATION_VERSION=4", asset.read_text("utf-8"))
        self.assertIn(
            "HERDR_INTEGRATION_ID=antigravity_cli", TAP_ASSET.read_text("utf-8")
        )


if __name__ == "__main__":
    unittest.main()
