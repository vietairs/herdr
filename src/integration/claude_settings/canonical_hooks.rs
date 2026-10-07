//! The hook entries the Claude integration owns in `settings.json`.
//!
//! One table describes every canonical entry so install, removal and the
//! byte-preserving compact writers all agree on its exact shape.

use std::io;
use std::path::Path;

use jsonc_parser::cst::CstInputValue;
use serde_json::{Map, Value};

use super::super::command::hook_command;

// Claude's documented SessionStart sources. Grok imports Claude hooks but uses
// `new`/`load`; filter before it starts an unnecessary hook process.
pub(super) const SESSION_START_MATCHER: &str = "^(startup|resume|clear|compact|fork)$";

/// Seconds Claude waits for the hook command before abandoning it.
const HOOK_TIMEOUT_SECS: u64 = 10;

pub(super) struct CanonicalHook {
    pub(super) event: &'static str,
    pub(super) action: &'static str,
    pub(super) matcher: Option<&'static str>,
    /// Async hooks run beside Claude instead of blocking it, so a transcript
    /// read after every tool call never delays the agent.
    pub(super) run_async: bool,
}

pub(super) const CANONICAL_HOOKS: &[CanonicalHook] = &[
    CanonicalHook {
        event: "SessionStart",
        action: "session",
        matcher: Some(SESSION_START_MATCHER),
        run_async: false,
    },
    CanonicalHook {
        event: "PostToolUse",
        action: "cache",
        matcher: None,
        run_async: true,
    },
    CanonicalHook {
        event: "Stop",
        action: "cache",
        matcher: None,
        run_async: true,
    },
];

pub(super) fn canonical_for_event(event: &str) -> Option<&'static CanonicalHook> {
    CANONICAL_HOOKS.iter().find(|hook| hook.event == event)
}

/// `{"matcher"?, "hooks": [{"type":"command","command":..,"timeout":10,"async":true?}]}`
pub(super) fn canonical_hook_value(hook: &CanonicalHook, hook_path: &Path) -> Value {
    let mut command = Map::new();
    command.insert("type".to_string(), Value::String("command".to_string()));
    command.insert(
        "command".to_string(),
        Value::String(hook_command(hook_path, Some(hook.action))),
    );
    command.insert("timeout".to_string(), Value::from(HOOK_TIMEOUT_SECS));
    if hook.run_async {
        command.insert("async".to_string(), Value::Bool(true));
    }

    let mut entry = Map::new();
    if let Some(matcher) = hook.matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    entry.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(command)]),
    );
    Value::Object(entry)
}

pub(super) fn canonical_hook_input(hook: &CanonicalHook, hook_path: &Path) -> CstInputValue {
    let mut command = vec![
        (
            "type".to_string(),
            CstInputValue::String("command".to_string()),
        ),
        (
            "command".to_string(),
            CstInputValue::String(hook_command(hook_path, Some(hook.action))),
        ),
        (
            "timeout".to_string(),
            CstInputValue::Number(HOOK_TIMEOUT_SECS.to_string()),
        ),
    ];
    if hook.run_async {
        command.push(("async".to_string(), CstInputValue::Bool(true)));
    }

    let mut entry = Vec::new();
    if let Some(matcher) = hook.matcher {
        entry.push((
            "matcher".to_string(),
            CstInputValue::String(matcher.to_string()),
        ));
    }
    entry.push((
        "hooks".to_string(),
        CstInputValue::Array(vec![CstInputValue::Object(command)]),
    ));
    CstInputValue::Object(entry)
}

/// Compact text in the key order matcher, hooks; inner order type, command,
/// timeout, async.
pub(super) fn canonical_hook_json(hook: &CanonicalHook, hook_path: &Path) -> io::Result<String> {
    let command = serde_json::to_string(&hook_command(hook_path, Some(hook.action)))?;
    let matcher = match hook.matcher {
        Some(matcher) => format!("\"matcher\":{},", serde_json::to_string(matcher)?),
        None => String::new(),
    };
    let async_flag = if hook.run_async {
        ",\"async\":true"
    } else {
        ""
    };
    Ok(format!(
        "{{{matcher}\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":{HOOK_TIMEOUT_SECS}{async_flag}}}]}}"
    ))
}
