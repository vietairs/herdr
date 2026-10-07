"""Hermes plugin installed by Herdr to report resumable session identity and context tokens."""

# HERDR_INTEGRATION_ID=hermes
# HERDR_INTEGRATION_VERSION=6

from __future__ import annotations

import os
import subprocess
import time

_SOURCE = "herdr:hermes"
_AGENT = "hermes"
_INTERACTIVE_PLATFORMS = {"cli", "tui", "desktop", "acp"}


def _pane_id() -> str | None:
    if os.environ.get("HERDR_ENV") != "1":
        return None
    return os.environ.get("HERDR_PANE_ID", "").strip() or None


def _send_session(session_id: str, start_source: str) -> None:
    pane_id = _pane_id()
    if pane_id is None:
        return
    command = [
        os.environ.get("HERDR_BIN_PATH") or "herdr",
        "pane",
        "report-agent-session",
        pane_id,
        "--source",
        _SOURCE,
        "--agent",
        _AGENT,
        "--seq",
        str(time.time_ns()),
        "--agent-session-id",
        session_id,
        "--session-start-source",
        start_source,
    ]
    try:
        kwargs = {"timeout": 1, "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}
        if os.name == "nt":
            kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW
        subprocess.run(command, check=False, **kwargs)
    except Exception:
        pass


def _report_session(start_source: str, **kwargs) -> None:
    if kwargs.get("platform") not in _INTERACTIVE_PLATFORMS:
        return
    session_id = kwargs.get("session_id")
    if not isinstance(session_id, str) or not session_id:
        return
    _send_session(session_id, start_source)


def _session_started(**kwargs) -> None:
    _report_session("startup", **kwargs)


def _session_reset(**kwargs) -> None:
    _report_session("new", **kwargs)


def _session_observed(**kwargs) -> None:
    if kwargs.get("platform") == "cli":
        _report_session("resume", **kwargs)


def _usage_tokens(usage) -> int | None:
    """Context tokens of one request, or None when the usage dict has no reading.

    Pinned against NousResearch/hermes-agent at commit
    82732650a18f303c2ac3e305f3c12a122ef1649a:
    https://github.com/NousResearch/hermes-agent/blob/82732650a18f303c2ac3e305f3c12a122ef1649a/agent/api_request_hooks.py
    (`_usage_summary_for_api_request_hook`) builds the `usage` dict of
    `post_api_request` from `CanonicalUsage` (agent/usage_pricing.py) and adds
    `prompt_tokens = input_tokens + cache_read_tokens + cache_write_tokens`.
    `prompt_tokens` therefore already counts cached input, so it is the whole
    context of the request and cache buckets must not be added again. `usage`
    is None when the provider returned no accounting data.
    """
    if not isinstance(usage, dict):
        return None
    tokens = usage.get("prompt_tokens")
    if isinstance(tokens, bool) or not isinstance(tokens, int) or tokens <= 0:
        return None
    return tokens


def _send_context_usage(pane_id: str, tokens: int, ended_at) -> None:
    command = [
        os.environ.get("HERDR_BIN_PATH") or "herdr",
        "pane",
        "report-context-usage",
        pane_id,
        "--source",
        _SOURCE,
        "--used",
        str(tokens),
    ]
    # `ended_at` is epoch seconds in the hook payload; anything else is dropped
    # so the server stamps the report itself.
    if (
        isinstance(ended_at, (int, float))
        and not isinstance(ended_at, bool)
        and ended_at > 0
    ):
        command += ["--observed-at", str(int(ended_at * 1000))]
    kwargs = {
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL,
        "stderr": subprocess.DEVNULL,
    }
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW
    # Fire and forget: the hook runs inside the agent loop and must not wait.
    subprocess.Popen(command, **kwargs)


def _api_request_finished(**kwargs) -> None:
    try:
        if kwargs.get("platform") not in _INTERACTIVE_PLATFORMS:
            return
        pane_id = _pane_id()
        if pane_id is None:
            return
        tokens = _usage_tokens(kwargs.get("usage"))
        if tokens is None:
            return
        _send_context_usage(pane_id, tokens, kwargs.get("ended_at"))
    except Exception:
        pass


def register(ctx):
    ctx.register_hook("on_session_start", _session_started)
    ctx.register_hook("on_session_reset", _session_reset)
    ctx.register_hook("pre_llm_call", _session_observed)
    ctx.register_hook("post_api_request", _api_request_finished)
