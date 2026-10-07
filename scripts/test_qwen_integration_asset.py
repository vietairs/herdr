"""End-to-end tests for the Qwen Code hook asset's `usage` and `session` actions.

The shell hook runs against a fake `herdr` executable that records its argv, so
the exact CLI invocation is asserted without a running server.
"""

import json
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


ASSET_DIR = Path(__file__).parents[1] / "src/integration/assets/qwen"
ASSET = ASSET_DIR / "herdr-agent-session.sh"
POWERSHELL_ASSET = ASSET_DIR / "herdr-agent-session.ps1"

FAKE_HERDR = """#!/bin/sh
python3 - "$@" <<'PY' >> "$HERDR_FAKE_LOG"
import json
import sys

print(json.dumps(sys.argv[1:]))
PY
"""


def run_hook(payload, action="usage"):
    """Runs the hook; returns the argv lists the fake herdr recorded."""
    with tempfile.TemporaryDirectory(prefix="hqw") as work:
        fake = os.path.join(work, "herdr")
        log = os.path.join(work, "calls.jsonl")
        with open(fake, "w", encoding="utf-8") as handle:
            handle.write(FAKE_HERDR)
        os.chmod(fake, os.stat(fake).st_mode | stat.S_IXUSR)
        open(log, "w", encoding="utf-8").close()
        env = dict(os.environ)
        env.update(
            {
                "HERDR_ENV": "1",
                "HERDR_PANE_ID": "p1",
                "HERDR_SOCKET_PATH": "/unused",
                "HERDR_BIN_PATH": fake,
                "HERDR_FAKE_LOG": log,
            }
        )
        subprocess.run(
            ["sh", str(ASSET), action],
            input=json.dumps(payload).encode(),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=15,
            check=False,
        )
        with open(log, encoding="utf-8") as handle:
            return [json.loads(line) for line in handle if line.strip()]


def stop(**fields):
    return {"hook_event_name": "Stop", "session_id": "s1", **fields}


@unittest.skipIf(os.name == "nt", "runs the POSIX shell asset against a Unix-socket fake server")
class QwenUsageActionTests(unittest.TestCase):
    def test_stop_reports_used_and_window(self):
        calls = run_hook(stop(input_tokens=84000, context_limit=262144))
        self.assertEqual(
            calls,
            [
                [
                    "pane",
                    "report-context-usage",
                    "p1",
                    "--source",
                    "herdr:qwen",
                    "--used",
                    "84000",
                    "--window",
                    "262144",
                ]
            ],
        )

    def test_stop_without_limit_reports_tokens_only(self):
        expected = [
            [
                "pane",
                "report-context-usage",
                "p1",
                "--source",
                "herdr:qwen",
                "--used",
                "84000",
            ]
        ]
        self.assertEqual(run_hook(stop(input_tokens=84000)), expected)
        # A non-positive or non-integer limit is not a window.
        self.assertEqual(
            run_hook(stop(input_tokens=84000, context_limit=0)), expected
        )
        self.assertEqual(
            run_hook(stop(input_tokens=84000, context_limit="262144")), expected
        )
        self.assertEqual(
            run_hook(stop(input_tokens=84000, context_limit=True)), expected
        )

    def test_zero_tokens_are_reported(self):
        calls = run_hook(stop(input_tokens=0, context_limit=1000))
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][calls[0].index("--used") + 1], "0")

    def test_missing_tokens_and_other_events_are_silent(self):
        self.assertEqual(run_hook(stop(context_limit=262144)), [])
        self.assertEqual(run_hook(stop(input_tokens=-1)), [])
        self.assertEqual(run_hook(stop(input_tokens=1.5)), [])
        self.assertEqual(run_hook(stop(input_tokens="84000")), [])
        self.assertEqual(run_hook(stop(input_tokens=True)), [])
        self.assertEqual(
            run_hook(
                {
                    "hook_event_name": "SessionStart",
                    "session_id": "s1",
                    "input_tokens": 84000,
                }
            ),
            [],
        )
        self.assertEqual(run_hook({}), [])

    def test_unknown_action_is_silent(self):
        self.assertEqual(run_hook(stop(input_tokens=84000), action="bogus"), [])


@unittest.skipIf(os.name == "nt", "runs the POSIX shell asset against a Unix-socket fake server")
class QwenSessionActionTests(unittest.TestCase):
    def test_session_action_unchanged(self):
        calls = run_hook(
            {
                "hook_event_name": "SessionStart",
                "session_id": "abc",
                "source": "resume",
            },
            action="session",
        )
        self.assertEqual(len(calls), 1)
        argv = calls[0]
        self.assertEqual(argv[:3], ["pane", "report-agent-session", "p1"])
        self.assertIn("--agent-session-id", argv)
        self.assertEqual(argv[argv.index("--agent-session-id") + 1], "abc")
        self.assertEqual(argv[argv.index("--session-start-source") + 1], "resume")


class QwenAssetVersionTests(unittest.TestCase):
    def test_assets_carry_version_two(self):
        for asset in (ASSET, POWERSHELL_ASSET):
            text = asset.read_text(encoding="utf-8")
            self.assertIn("HERDR_INTEGRATION_ID=qwen", text, asset.name)
            self.assertIn("HERDR_INTEGRATION_VERSION=2", text, asset.name)


if __name__ == "__main__":
    unittest.main()
