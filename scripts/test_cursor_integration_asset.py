"""End-to-end tests for the Cursor agent CLI statusline tap asset.

The tap is run through `sh -c` against a fake herdr socket, because the Rust
CLI harness in `tests/cli` only compiles on Linux. The Cursor stdin keys are
documented only by a changelog entry and a forum post, so these tests pin the
exact keys the reporter accepts and that anything else sends nothing.
"""

import json
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from scripts.test_claude_integration_asset import (
    ECHO_STDIN,
    TAP_FILE_NAME,
    FakeHerdrSocket,
    by_method,
    herdr_env,
    run_tap,
    settle_requests,
    wait_for_requests,
)

ASSET_DIR = Path(__file__).parents[1] / "src/integration/assets/cursor"
TAP_ASSET = ASSET_DIR / "herdr-statusline-tap.sh"
HOOK_ASSETS = (ASSET_DIR / "herdr-agent-state.sh", ASSET_DIR / "herdr-agent-state.ps1")
SOURCE = "herdr:cursor"

TOTAL_INPUT_FIXTURE = json.dumps(
    {
        "context_window": {"total_input_tokens": 84000, "context_window_size": 200000},
        "autorun": False,
    }
)


def tap(original, stdin, env, tap_path=TAP_ASSET):
    return run_tap(original, stdin, env, tap_path=tap_path)


@unittest.skipIf(os.name == "nt", "runs the POSIX shell asset against a Unix-socket fake server")
class CursorStatuslineTapTests(unittest.TestCase):
    def assert_passthrough(self, result, stdin=TOTAL_INPUT_FIXTURE):
        self.assertEqual(result.stdout, "OUT:" + stdin, result.stderr)
        self.assertEqual(result.returncode, 3, result.stderr)

    def reported(self, payload, quiet=0.5):
        """Runs the tap once and returns the requests the fake socket saw."""
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, payload, herdr_env(fake.path))
            requests = settle_requests(fake, quiet=quiet)
        self.assert_passthrough(result, payload)
        return requests

    def test_passthrough_of_stdin_stdout_and_exit_code(self):
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, TOTAL_INPUT_FIXTURE, herdr_env(fake.path))
        self.assert_passthrough(result)

    def test_original_runs_with_sh_semantics(self):
        original = "echo 'a\\tb'"
        direct = subprocess.run(
            ["sh", "-c", original], capture_output=True, text=True, timeout=10
        )
        with FakeHerdrSocket() as fake:
            result = tap(original, TOTAL_INPUT_FIXTURE, herdr_env(fake.path))
        self.assertEqual(result.stdout, direct.stdout)
        self.assertEqual(result.returncode, direct.returncode)

    def test_total_input_tokens_reports_exact_window(self):
        with FakeHerdrSocket() as fake:
            before_ms = int(time.time() * 1000)
            result = tap(ECHO_STDIN, TOTAL_INPUT_FIXTURE, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assert_passthrough(result)

        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["method"], "pane.report_context_usage")
        params = requests[0]["params"]
        self.assertEqual(params["pane_id"], "p1")
        self.assertEqual(params["source"], SOURCE)
        self.assertEqual(params["used_tokens"], 84_000)
        self.assertEqual(params["window_tokens"], 200_000)
        self.assertLess(abs(params["observed_at_ms"] - before_ms), 10_000)
        self.assertNotIn("clear", params)

    def test_claude_shaped_current_usage_wins(self):
        payload = json.dumps(
            {
                "context_window": {
                    "context_window_size": 200000,
                    "total_input_tokens": 5,
                    "current_usage": {
                        "input_tokens": 5,
                        "cache_creation_input_tokens": 100,
                        "cache_read_input_tokens": 83895,
                    },
                }
            }
        )
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, payload, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assert_passthrough(result, payload)
        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["params"]["used_tokens"], 84_000)
        self.assertEqual(requests[0]["params"]["window_tokens"], 200_000)

    def test_implausible_or_missing_fields_send_nothing(self):
        for payload in [
            json.dumps(
                {
                    "context_window": {
                        "total_input_tokens": 900000,
                        "context_window_size": 200000,
                    }
                }
            ),
            "{}",
            json.dumps({"context_window": []}),
            json.dumps({"context_window": {"total_input_tokens": "84000"}}),
            json.dumps({"context_window": {"total_input_tokens": False}}),
            json.dumps({"context_window": {"total_input_tokens": -1}}),
            json.dumps({"context_window": {"current_usage": None}}),
            json.dumps({"context_window": {"current_context_tokens": 5000}}),
            json.dumps([1, 2]),
            "not json",
        ]:
            with self.subTest(payload=payload):
                self.assertEqual(self.reported(payload), [])

    def test_window_unknown_reports_tokens_only(self):
        payload = json.dumps({"context_window": {"total_input_tokens": 1200}})
        with FakeHerdrSocket() as fake:
            result = tap(ECHO_STDIN, payload, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assert_passthrough(result, payload)
        self.assertEqual(len(requests), 1, requests)
        self.assertEqual(requests[0]["params"]["used_tokens"], 1200)
        self.assertNotIn("window_tokens", requests[0]["params"])

    def test_mistyped_window_is_omitted_not_guessed(self):
        payload = json.dumps(
            {
                "context_window": {
                    "context_window_size": "200000",
                    "total_input_tokens": 7,
                }
            }
        )
        with FakeHerdrSocket() as fake:
            tap(ECHO_STDIN, payload, herdr_env(fake.path))
            wait_for_requests(fake, 1)
            requests = settle_requests(fake, quiet=0.3)
        self.assertEqual(len(requests), 1, requests)
        self.assertNotIn("window_tokens", requests[0]["params"])

    def test_never_reports_prompt_cache(self):
        payload = json.dumps(
            {
                "context_window": {
                    "total_input_tokens": 7,
                    "context_window_size": 200000,
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
        self.assertNotIn("clear", requests[0]["params"])

    def test_tap_file_deleted_still_runs_the_original(self):
        with tempfile.TemporaryDirectory(prefix="hct") as work:
            missing = Path(work) / TAP_FILE_NAME
            with FakeHerdrSocket() as fake:
                result = tap(
                    ECHO_STDIN,
                    TOTAL_INPUT_FIXTURE,
                    herdr_env(fake.path),
                    tap_path=missing,
                )
                requests = settle_requests(fake)
        self.assert_passthrough(result)
        self.assertEqual(requests, [])

    def test_outside_herdr_reports_nothing(self):
        with FakeHerdrSocket() as fake:
            env = {
                name: value
                for name, value in herdr_env(fake.path).items()
                if name != "HERDR_ENV"
            }
            result = tap(ECHO_STDIN, TOTAL_INPUT_FIXTURE, env)
            requests = settle_requests(fake)
        self.assert_passthrough(result)
        self.assertEqual(requests, [])

    def test_assets_carry_version_two(self):
        for asset in (TAP_ASSET, *HOOK_ASSETS):
            with self.subTest(asset=asset.name):
                self.assertIn("HERDR_INTEGRATION_VERSION=2", asset.read_text("utf-8"))
        self.assertIn("HERDR_INTEGRATION_ID=cursor", TAP_ASSET.read_text("utf-8"))


if __name__ == "__main__":
    unittest.main()
