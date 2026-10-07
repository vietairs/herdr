import importlib.util
import unittest
from pathlib import Path
from unittest import mock


ASSET = Path(__file__).parents[1] / "src/integration/assets/hermes/__init__.py"


def load_asset():
    spec = importlib.util.spec_from_file_location("herdr_hermes_integration", ASSET)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load Hermes integration asset")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FakeContext:
    def __init__(self):
        self.hooks = {}

    def register_hook(self, name, callback):
        self.hooks[name] = callback


class HermesIntegrationAssetTests(unittest.TestCase):
    def test_reports_only_root_session_identity(self):
        module = load_asset()
        calls = []
        module._send_session = lambda session_id, start_source: calls.append(
            (session_id, start_source)
        )
        context = FakeContext()
        module.register(context)

        self.assertEqual(
            set(context.hooks),
            {
                "on_session_start",
                "on_session_reset",
                "pre_llm_call",
                "post_api_request",
            },
        )

        context.hooks["on_session_start"](session_id="root-1", platform="tui")
        context.hooks["pre_llm_call"](session_id="root-1", platform="tui")
        context.hooks["pre_llm_call"](session_id="child", platform="subagent")
        context.hooks["on_session_reset"](session_id="root-2", platform="tui")
        context.hooks["pre_llm_call"](
            session_id="background", platform="tui"
        )

        self.assertEqual(calls, [("root-1", "startup"), ("root-2", "new")])

    def test_send_session_uses_cli_with_the_active_pane(self):
        module = load_asset()
        environment = {
            "HERDR_ENV": "1",
            "HERDR_PANE_ID": "w1:p2",
            "HERDR_BIN_PATH": "C:/bin/herdr.exe",
        }
        with mock.patch.dict(module.os.environ, environment, clear=True):
            with mock.patch.object(module.subprocess, "run") as run:
                module._send_session("session-1", "resume")

        command = run.call_args.args[0]
        self.assertEqual(
            command[:4],
            ["C:/bin/herdr.exe", "pane", "report-agent-session", "w1:p2"],
        )
        self.assertIn("session-1", command)
        self.assertIn("resume", command)
        self.assertFalse(run.call_args.kwargs["check"])
        if module.os.name == "nt":
            self.assertEqual(
                run.call_args.kwargs["creationflags"],
                module.subprocess.CREATE_NO_WINDOW,
            )

    def test_first_turn_recovers_resumed_session_identity(self):
        module = load_asset()
        calls = []
        module._send_session = lambda session_id, start_source: calls.append(
            (session_id, start_source)
        )
        context = FakeContext()
        module.register(context)

        context.hooks["pre_llm_call"](session_id="resumed", platform="cli")

        self.assertEqual(calls, [("resumed", "resume")])

    def test_post_api_request_reports_context_tokens(self):
        module = load_asset()
        environment = {
            "HERDR_ENV": "1",
            "HERDR_PANE_ID": "w1:p2",
            "HERDR_BIN_PATH": "C:/bin/herdr.exe",
        }
        # prompt_tokens already includes cache reads and writes, so it is the
        # whole context of the request and is reported as is.
        usage = {
            "input_tokens": 100,
            "cache_read_tokens": 4000,
            "cache_write_tokens": 900,
            "output_tokens": 50,
            "prompt_tokens": 5000,
            "total_tokens": 5050,
        }
        with mock.patch.dict(module.os.environ, environment, clear=True):
            with mock.patch.object(module.subprocess, "Popen") as popen:
                module._api_request_finished(
                    platform="tui", usage=usage, ended_at=1700000000.25
                )

        command = popen.call_args.args[0]
        self.assertEqual(
            command,
            [
                "C:/bin/herdr.exe",
                "pane",
                "report-context-usage",
                "w1:p2",
                "--source",
                "herdr:hermes",
                "--used",
                "5000",
                "--observed-at",
                "1700000000250",
            ],
        )
        self.assertNotIn("--window", command)
        self.assertEqual(popen.call_args.kwargs["stdin"], module.subprocess.DEVNULL)
        self.assertEqual(popen.call_args.kwargs["stdout"], module.subprocess.DEVNULL)
        self.assertEqual(popen.call_args.kwargs["stderr"], module.subprocess.DEVNULL)
        if module.os.name == "nt":
            self.assertEqual(
                popen.call_args.kwargs["creationflags"],
                module.subprocess.CREATE_NO_WINDOW,
            )

    def test_post_api_request_omits_observed_at_without_a_usable_end_time(self):
        module = load_asset()
        environment = {"HERDR_ENV": "1", "HERDR_PANE_ID": "w1:p2"}
        for ended_at in (None, "2026-10-07T00:00:00Z", 0, -1.0, True):
            with self.subTest(ended_at=ended_at):
                with mock.patch.dict(module.os.environ, environment, clear=True):
                    with mock.patch.object(module.subprocess, "Popen") as popen:
                        module._api_request_finished(
                            platform="cli",
                            usage={"prompt_tokens": 7},
                            ended_at=ended_at,
                        )
                command = popen.call_args.args[0]
                self.assertEqual(command[0], "herdr")
                self.assertEqual(command[-2:], ["--used", "7"])
                self.assertNotIn("--observed-at", command)

    def test_post_api_request_never_waits(self):
        module = load_asset()
        environment = {"HERDR_ENV": "1", "HERDR_PANE_ID": "w1:p2"}
        with mock.patch.dict(module.os.environ, environment, clear=True):
            with mock.patch.object(module.subprocess, "Popen") as popen:
                with mock.patch.object(module.subprocess, "run") as run:
                    module._api_request_finished(
                        platform="tui", usage={"prompt_tokens": 12}, ended_at=1.5
                    )

        popen.assert_called_once()
        run.assert_not_called()
        child = popen.return_value
        child.wait.assert_not_called()
        child.communicate.assert_not_called()
        child.poll.assert_not_called()

    def test_post_api_request_ignores_non_interactive_and_bad_usage(self):
        module = load_asset()
        environment = {"HERDR_ENV": "1", "HERDR_PANE_ID": "w1:p2"}
        cases = [
            {"platform": "subagent", "usage": {"prompt_tokens": 10}},
            {"platform": "", "usage": {"prompt_tokens": 10}},
            {"usage": {"prompt_tokens": 10}},
            {"platform": "tui", "usage": None},
            {"platform": "tui", "usage": "5000"},
            {"platform": "tui", "usage": [5000]},
            {"platform": "tui", "usage": {}},
            {"platform": "tui", "usage": {"input_tokens": 10}},
            {"platform": "tui", "usage": {"prompt_tokens": "10"}},
            {"platform": "tui", "usage": {"prompt_tokens": 10.5}},
            {"platform": "tui", "usage": {"prompt_tokens": True}},
            {"platform": "tui", "usage": {"prompt_tokens": 0}},
            {"platform": "tui", "usage": {"prompt_tokens": -3}},
        ]
        for case in cases:
            with self.subTest(case=case):
                with mock.patch.dict(module.os.environ, environment, clear=True):
                    with mock.patch.object(module.subprocess, "Popen") as popen:
                        module._api_request_finished(ended_at=1.5, **case)
                popen.assert_not_called()

    def test_post_api_request_is_silent_outside_a_herdr_pane(self):
        module = load_asset()
        for environment in ({}, {"HERDR_ENV": "1"}, {"HERDR_PANE_ID": "w1:p2"}):
            with self.subTest(environment=environment):
                with mock.patch.dict(module.os.environ, environment, clear=True):
                    with mock.patch.object(module.subprocess, "Popen") as popen:
                        module._api_request_finished(
                            platform="tui", usage={"prompt_tokens": 10}
                        )
                popen.assert_not_called()

    def test_post_api_request_never_raises(self):
        module = load_asset()
        environment = {"HERDR_ENV": "1", "HERDR_PANE_ID": "w1:p2"}
        with mock.patch.dict(module.os.environ, environment, clear=True):
            with mock.patch.object(
                module.subprocess, "Popen", side_effect=OSError("no herdr")
            ):
                module._api_request_finished(
                    platform="tui", usage={"prompt_tokens": 10}, ended_at=1.5
                )

    def test_register_adds_post_api_request_hook(self):
        module = load_asset()
        context = FakeContext()
        module.register(context)

        self.assertEqual(
            set(context.hooks),
            {
                "on_session_start",
                "on_session_reset",
                "pre_llm_call",
                "post_api_request",
            },
        )
        self.assertIs(context.hooks["post_api_request"], module._api_request_finished)

    def test_asset_carries_version_six(self):
        text = ASSET.read_text(encoding="utf-8")
        self.assertIn("# HERDR_INTEGRATION_VERSION=6\n", text)
        self.assertNotIn("# HERDR_INTEGRATION_VERSION=5\n", text)


if __name__ == "__main__":
    unittest.main()
