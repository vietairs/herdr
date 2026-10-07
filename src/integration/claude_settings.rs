use std::collections::HashSet;
use std::io;
use std::path::Path;

use jsonc_parser::ast::{Array as AstArray, Object as AstObject, Value as AstValue};
use jsonc_parser::common::Ranged;
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{parse_to_ast, CollectOptions, ParseOptions};
use serde_json::{Map, Value};

use super::command::hook_command;
use super::config_edit::{
    ensure_command_hook, ensure_hooks_object, hook_command_variants, hooks_object_if_present,
    is_matching_command_hook,
};
#[cfg(not(windows))]
use super::statusline_tap::{install_statusline_tap, uninstall_statusline_tap};

mod canonical_hooks;

use canonical_hooks::{
    canonical_for_event, canonical_hook_input, canonical_hook_json, canonical_hook_value,
    CanonicalHook, CANONICAL_HOOKS, SESSION_START_MATCHER,
};

struct HookRemoval {
    event: &'static str,
    actions: &'static [&'static str],
}

const HOOK_REMOVALS: &[HookRemoval] = &[
    HookRemoval {
        event: "PostToolUse",
        actions: &["working", "cache"],
    },
    HookRemoval {
        event: "PostToolUseFailure",
        actions: &["working"],
    },
    HookRemoval {
        event: "SubagentStop",
        actions: &["working"],
    },
    HookRemoval {
        event: "PermissionRequest",
        actions: &["blocked"],
    },
    HookRemoval {
        event: "SessionStart",
        actions: &["idle", "session"],
    },
    HookRemoval {
        event: "UserPromptSubmit",
        actions: &["working"],
    },
    HookRemoval {
        event: "PreToolUse",
        actions: &["working"],
    },
    HookRemoval {
        event: "Stop",
        actions: &["idle", "cache"],
    },
    HookRemoval {
        event: "SessionEnd",
        actions: &["release"],
    },
];

pub(crate) fn install(content: &str, settings_path: &Path, hook_path: &Path) -> io::Result<String> {
    let original = parse_value(content, settings_path)?;
    let mut desired = original.clone();
    let hooks = ensure_hooks_object(
        &mut desired,
        settings_path,
        "claude settings",
        "claude settings hooks",
    )?;
    apply_value_removals(hooks, hook_path, true)?;
    ensure_command_hook(
        hooks,
        "SessionStart",
        hook_command(hook_path, Some("session")),
        10,
        Some(SESSION_START_MATCHER),
    )?;
    for hook in CANONICAL_HOOKS.iter().filter(|hook| hook.run_async) {
        ensure_canonical_value_entry(hooks, hook, hook_path)?;
    }

    if desired == original {
        return Ok(content.to_string());
    }

    rewrite(
        content,
        settings_path,
        hook_path,
        EditKind::Install,
        &desired,
    )
}

pub(crate) fn uninstall(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
) -> io::Result<String> {
    let original = parse_value(content, settings_path)?;
    let mut desired = original.clone();
    let mut removed = false;

    if let Some(hooks) = hooks_object_if_present(
        &mut desired,
        settings_path,
        "claude settings",
        "claude settings hooks",
    )? {
        removed = apply_value_removals(hooks, hook_path, false)?;
    }

    if !removed {
        return Ok(content.to_string());
    }

    rewrite(
        content,
        settings_path,
        hook_path,
        EditKind::Uninstall,
        &desired,
    )
}

/// Settings key of Claude Code's statusline object.
#[cfg(not(windows))]
const STATUSLINE_KEY: &str = "statusLine";

/// Wraps an existing `statusLine.command` with the herdr tap; never creates one.
#[cfg(not(windows))]
pub(crate) fn install_statusline(
    content: &str,
    settings_path: &Path,
    tap_path: &Path,
) -> io::Result<String> {
    install_statusline_tap(content, settings_path, STATUSLINE_KEY, tap_path)
}

/// Restores the user's own `statusLine.command` when it is wrapped by a herdr tap.
#[cfg(not(windows))]
pub(crate) fn uninstall_statusline(content: &str, settings_path: &Path) -> io::Result<String> {
    uninstall_statusline_tap(content, settings_path, STATUSLINE_KEY)
}

/// While installing, the first entry equal to an event's canonical hook is
/// kept; every other entry of ours is removed.
fn apply_value_removals(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
    installing: bool,
) -> io::Result<bool> {
    let mut removed = false;
    for policy in HOOK_REMOVALS {
        let commands = removal_commands(policy, hook_path);
        let canonical = canonical_value_for_event(policy.event, hook_path, installing);
        removed |= remove_value_event_commands(hooks, policy.event, &commands, canonical.as_ref())?;
    }
    Ok(removed)
}

fn canonical_value_for_event(event: &str, hook_path: &Path, installing: bool) -> Option<Value> {
    if !installing {
        return None;
    }
    canonical_for_event(event).map(|hook| canonical_hook_value(hook, hook_path))
}

/// Appends the canonical entry of an async hook unless an equal one is present.
fn ensure_canonical_value_entry(
    hooks: &mut Map<String, Value>,
    hook: &CanonicalHook,
    hook_path: &Path,
) -> io::Result<()> {
    let canonical = canonical_hook_value(hook, hook_path);
    let entries = hooks
        .entry(hook.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            io::Error::other(format!("hook entries for {} must be an array", hook.event))
        })?;
    if !entries.contains(&canonical) {
        entries.push(canonical);
    }
    Ok(())
}

fn remove_value_event_commands(
    hooks: &mut Map<String, Value>,
    event: &str,
    commands: &[String],
    canonical: Option<&Value>,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };
    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;
    let mut removed = false;
    let mut canonical_preserved = false;

    entries.retain_mut(|entry| {
        if !canonical_preserved && canonical.is_some_and(|canonical| entry == canonical) {
            canonical_preserved = true;
            return true;
        }
        let Some(command_entries) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = command_entries.len();
        command_entries.retain(|entry| {
            !commands
                .iter()
                .any(|command| is_matching_command_hook(entry, command))
        });
        removed |= command_entries.len() != before;
        !command_entries.is_empty()
    });

    if entries.is_empty() && canonical.is_none() {
        hooks.remove(event);
    }
    Ok(removed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Install,
    Uninstall,
}

fn rewrite(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
    kind: EditKind,
    desired: &Value,
) -> io::Result<String> {
    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })?;
    let root_value = root.value().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    reject_duplicate_keys(&root_value, settings_path)?;
    let root_object = root_value.as_object().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let hooks = match root_object.get("hooks") {
        Some(property) => property.object_value().ok_or_else(|| {
            io::Error::other(format!(
                "claude settings hooks at {} must be a JSON object",
                settings_path.display()
            ))
        })?,
        None if kind == EditKind::Install
            && direct_children_are_compact(&root_object.children()) =>
        {
            let updated = append_hooks_property_compact(content, hook_path, settings_path)?;
            return verify_updated(updated, settings_path, desired);
        }
        None if kind == EditKind::Install => root_object
            .append("hooks", CstInputValue::Object(Vec::new()))
            .object_value()
            .ok_or_else(|| io::Error::other("failed to create claude settings hooks object"))?,
        None => return Ok(content.to_string()),
    };

    let installing = kind == EditKind::Install;
    let mut preserved_events: Vec<&'static str> = Vec::new();
    for policy in HOOK_REMOVALS {
        let commands = removal_commands(policy, hook_path);
        let canonical = canonical_value_for_event(policy.event, hook_path, installing);
        if remove_event_commands(&hooks, policy.event, &commands, canonical.as_ref())? {
            preserved_events.push(policy.event);
        }
    }

    let mut updated = root.to_string();
    if installing {
        for hook in CANONICAL_HOOKS
            .iter()
            .filter(|hook| !preserved_events.contains(&hook.event))
        {
            updated = append_canonical_entry(&updated, hook, hook_path, settings_path)?;
        }
    }

    verify_updated(updated, settings_path, desired)
}

/// Appends one canonical entry to its event array, creating the event when it
/// is absent, and keeps compact containers compact.
fn append_canonical_entry(
    content: &str,
    hook: &CanonicalHook,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })?;
    let hooks = root
        .value()
        .and_then(|value| value.as_object())
        .and_then(|object| object.get("hooks"))
        .and_then(|property| property.object_value())
        .ok_or_else(|| {
            io::Error::other(format!(
                "claude settings hooks at {} must be a JSON object",
                settings_path.display()
            ))
        })?;

    match hooks.get(hook.event) {
        Some(property) => {
            let entries = property.array_value().ok_or_else(|| {
                io::Error::other(format!("hook entries for {} must be an array", hook.event))
            })?;
            if direct_children_are_compact(&entries.children()) {
                return append_event_entry_compact(content, hook, hook_path, settings_path);
            }
            entries.append(canonical_hook_input(hook, hook_path));
        }
        None if direct_children_are_compact(&hooks.children()) => {
            return append_event_property_compact(content, hook, hook_path, settings_path);
        }
        None => {
            let entries = hooks
                .append(hook.event, CstInputValue::Array(Vec::new()))
                .array_value()
                .ok_or_else(|| {
                    io::Error::other(format!("failed to create {} hook array", hook.event))
                })?;
            entries.append(canonical_hook_input(hook, hook_path));
        }
    }
    Ok(root.to_string())
}

fn remove_event_commands(
    hooks: &CstObject,
    event: &str,
    commands: &[String],
    canonical: Option<&Value>,
) -> io::Result<bool> {
    let Some(event_property) = hooks.get(event) else {
        return Ok(false);
    };
    let entries = event_property
        .array_value()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;
    let mut canonical_preserved = false;

    for entry in entries.elements() {
        if !canonical_preserved
            && canonical.is_some_and(|canonical| entry.to_serde_value().as_ref() == Some(canonical))
        {
            canonical_preserved = true;
            continue;
        }

        let Some(entry_object) = entry.as_object() else {
            continue;
        };
        let Some(command_entries) = entry_object
            .get("hooks")
            .and_then(|property| property.array_value())
        else {
            continue;
        };

        for command_entry in command_entries.elements() {
            let matches = command_entry.to_serde_value().is_some_and(|value| {
                commands
                    .iter()
                    .any(|command| is_matching_command_hook(&value, command))
            });
            if matches {
                command_entry.remove();
            }
        }

        if command_entries.elements().is_empty() {
            entry.remove();
        }
    }

    if entries.elements().is_empty() && canonical.is_none() {
        event_property.remove();
    }

    Ok(canonical_preserved)
}

fn removal_commands(policy: &HookRemoval, hook_path: &Path) -> Vec<String> {
    policy
        .actions
        .iter()
        .flat_map(|action| hook_command_variants(hook_path, Some(action)))
        .collect()
}

fn append_hooks_property_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let mut events = Vec::with_capacity(CANONICAL_HOOKS.len());
    for hook in CANONICAL_HOOKS {
        events.push(format!(
            "\"{}\":[{}]",
            hook.event,
            canonical_hook_json(hook, hook_path)?
        ));
    }
    let value = format!("{{{}}}", events.join(","));
    Ok(append_object_property(content, &root, "hooks", &value))
}

fn append_event_property_compact(
    content: &str,
    hook: &CanonicalHook,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let hooks = root.get_object("hooks").ok_or_else(|| {
        io::Error::other(format!(
            "claude settings hooks at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    let value = format!("[{}]", canonical_hook_json(hook, hook_path)?);
    Ok(append_object_property(content, hooks, hook.event, &value))
}

fn append_event_entry_compact(
    content: &str,
    hook: &CanonicalHook,
    hook_path: &Path,
    settings_path: &Path,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let entries = root
        .get_object("hooks")
        .and_then(|hooks| hooks.get_array(hook.event))
        .ok_or_else(|| {
            io::Error::other(format!("hook entries for {} must be an array", hook.event))
        })?;
    Ok(append_array_element(
        content,
        entries,
        &canonical_hook_json(hook, hook_path)?,
    ))
}

fn parse_ast_root_object<'a>(content: &'a str, settings_path: &Path) -> io::Result<AstObject<'a>> {
    let parsed = parse_to_ast(content, &CollectOptions::default(), &strict_parse_options())
        .map_err(|err| {
            io::Error::other(format!(
                "failed to parse {}: {err}",
                settings_path.display()
            ))
        })?;
    match parsed.value {
        Some(AstValue::Object(object)) => Ok(object),
        _ => Err(io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))),
    }
}

fn append_object_property(
    content: &str,
    object: &AstObject<'_>,
    name: &str,
    value: &str,
) -> String {
    let key = serde_json::to_string(name).expect("JSON object keys are serializable");
    let key_value_separator = object
        .properties
        .first()
        .map(|property| &content[property.name.range().end..property.value.range().start])
        .unwrap_or(":");
    let insertion = format!("{key}{key_value_separator}{value}");
    let delimiter = object_delimiter(content, object);
    append_to_container(
        content,
        object.range,
        !object.properties.is_empty(),
        delimiter,
        &insertion,
    )
}

fn append_array_element(content: &str, array: &AstArray<'_>, value: &str) -> String {
    let delimiter = array_delimiter(content, array);
    append_to_container(
        content,
        array.range,
        !array.elements.is_empty(),
        delimiter,
        value,
    )
}

fn object_delimiter<'a>(content: &'a str, object: &AstObject<'_>) -> &'a str {
    match object.properties.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range.end..second.range.start]),
        [first] => &content[object.range.start + 1..first.range.start],
        [] => "",
    }
}

fn array_delimiter<'a>(content: &'a str, array: &AstArray<'_>) -> &'a str {
    match array.elements.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range().end..second.range().start]),
        [first] => &content[array.range.start + 1..first.range().start],
        [] => "",
    }
}

fn delimiter_suffix(delimiter: &str) -> &str {
    delimiter
        .split_once(',')
        .map(|(_, suffix)| suffix)
        .unwrap_or(delimiter)
}

fn append_to_container(
    content: &str,
    range: jsonc_parser::common::Range,
    has_elements: bool,
    delimiter: &str,
    value: &str,
) -> String {
    let closing = range.end - 1;
    let insertion_index = if has_elements {
        content[..closing].trim_end_matches([' ', '\t']).len()
    } else {
        closing
    };
    let mut updated = String::with_capacity(content.len() + delimiter.len() + value.len() + 1);
    updated.push_str(&content[..insertion_index]);
    if has_elements {
        updated.push(',');
        updated.push_str(delimiter);
    }
    updated.push_str(value);
    updated.push_str(&content[insertion_index..]);
    updated
}

fn verify_updated(updated: String, settings_path: &Path, desired: &Value) -> io::Result<String> {
    let actual = parse_value(&updated, settings_path)?;
    if &actual != desired {
        return Err(io::Error::other(format!(
            "failed to safely update claude settings at {}",
            settings_path.display()
        )));
    }
    Ok(updated)
}

fn direct_children_are_compact(children: &[CstNode]) -> bool {
    !children.iter().any(CstNode::is_newline)
}

fn parse_value(content: &str, settings_path: &Path) -> io::Result<Value> {
    serde_json::from_str(content).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })
}

fn reject_duplicate_keys(node: &CstNode, settings_path: &Path) -> io::Result<()> {
    if let Some(object) = node.as_object() {
        let mut names = HashSet::new();
        for property in object.properties() {
            let name = property
                .name()
                .ok_or_else(|| io::Error::other("JSON object property is missing a name"))?
                .decoded_value()
                .map_err(|err| io::Error::other(format!("failed to decode JSON key: {err}")))?;
            if !names.insert(name.clone()) {
                return Err(io::Error::other(format!(
                    "claude settings at {} contains duplicate key {name:?}",
                    settings_path.display()
                )));
            }
            if let Some(value) = property.value() {
                reject_duplicate_keys(&value, settings_path)?;
            }
        }
    } else if let Some(array) = node.as_array() {
        for element in array.elements() {
            reject_duplicate_keys(&element, settings_path)?;
        }
    }
    Ok(())
}

fn strict_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: false,
        allow_loose_object_property_names: false,
        allow_trailing_commas: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (&'static Path, &'static Path) {
        (
            Path::new("/home/test/.claude/settings.json"),
            Path::new("/home/test/.claude/hooks/herdr-agent-state.sh"),
        )
    }

    #[test]
    fn install_preserves_untouched_formatting_and_complete_trailing_suffix() {
        let (settings_path, hook_path) = paths();
        let input = concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"Notification\" : [{\"matcher\":\"keep\",\"hooks\":[]}]\r\n",
            "    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        );

        let updated = install(input, settings_path, hook_path).unwrap();

        assert!(updated.starts_with(concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"Notification\" : [{\"matcher\":\"keep\",\"hooks\":[]}],\r\n",
        )));
        assert!(updated.ends_with(concat!(
            "\r\n    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        )));
        assert!(!updated.replace("\r\n", "").contains('\n'));
        assert!(updated.contains("\"SessionStart\""));
        assert!(updated.contains("\"PostToolUse\""));
        assert!(updated.contains("\"Stop\""));
        assert_eq!(
            serde_json::from_str::<Value>(&updated).unwrap()["zeta"]["number"],
            100.0
        );
    }

    #[test]
    fn install_keeps_compact_containers_compact() {
        let (settings_path, hook_path) = paths();
        let canonical = canonical_hook_json(&CANONICAL_HOOKS[0], hook_path).unwrap();
        let post = canonical_hook_json(&CANONICAL_HOOKS[1], hook_path).unwrap();
        let stop = canonical_hook_json(&CANONICAL_HOOKS[2], hook_path).unwrap();
        let cases = [
            (
                "{\"zeta\":{\"escaped\":\"\\u0061\",\"n\":1e+02},\"alpha\":1}\r\n",
                format!(
                    "{{\"zeta\":{{\"escaped\":\"\\u0061\",\"n\":1e+02}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}],\"PostToolUse\":[{post}],\"Stop\":[{stop}]}}}}\r\n"
                ),
            ),
            (
                "{\"hooks\":{\"Notification\":[{\"matcher\":\"keep\",\"hooks\":[]}]}, \"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"Notification\":[{{\"matcher\":\"keep\",\"hooks\":[]}}],\"SessionStart\":[{canonical}],\"PostToolUse\":[{post}],\"Stop\":[{stop}]}}, \"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"keep\",\"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"keep\",\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]}},{canonical}],\"PostToolUse\":[{post}],\"Stop\":[{stop}]}}}}"
                ),
            ),
            (
                "{\"zeta\":{\n  \"x\":1\n},\"alpha\":1}",
                format!(
                    "{{\"zeta\":{{\n  \"x\":1\n}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}],\"PostToolUse\":[{post}],\"Stop\":[{stop}]}}}}"
                ),
            ),
            (
                "{\"hooks\":{\"Notification\":[\n  {\"matcher\":\"keep\",\"hooks\":[]}\n]},\"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"Notification\":[\n  {{\"matcher\":\"keep\",\"hooks\":[]}}\n],\"SessionStart\":[{canonical}],\"PostToolUse\":[{post}],\"Stop\":[{stop}]}},\"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\n  \"matcher\":\"keep\",\n  \"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]\n}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\n  \"matcher\":\"keep\",\n  \"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]\n}},{canonical}],\"PostToolUse\":[{post}],\"Stop\":[{stop}]}}}}"
                ),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(install(input, settings_path, hook_path).unwrap(), expected);
        }
    }

    #[test]
    fn install_scopes_claude_session_start_sources() {
        let (settings_path, hook_path) = paths();
        let installed = install("{}", settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let matcher = settings["hooks"]["SessionStart"][0]["matcher"]
            .as_str()
            .unwrap();
        assert_eq!(matcher, "^(startup|resume|clear|compact|fork)$");
        let pattern = regex::Regex::new(matcher).unwrap();
        for source in ["startup", "resume", "clear", "compact", "fork"] {
            assert!(pattern.is_match(source), "Claude source: {source}");
        }
        for source in ["new", "load", "", "future-source", "startup-extra"] {
            assert!(!pattern.is_match(source), "non-Claude source: {source}");
        }
    }

    #[test]
    fn install_is_a_byte_exact_noop_for_a_canonical_hook() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let cache_command = serde_json::to_string(&hook_command(hook_path, Some("cache"))).unwrap();
        let input = format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"hooks\":[{{\"timeout\":10,\"command\":{command},\"type\":\"command\"}}],\"matcher\":\"{SESSION_START_MATCHER}\"}}],\"PostToolUse\":[{{\"hooks\":[{{\"async\":true,\"timeout\":10,\"command\":{cache_command},\"type\":\"command\"}}]}}],\"Stop\":[{{\"hooks\":[{{\"async\":true,\"timeout\":10,\"command\":{cache_command},\"type\":\"command\"}}]}}]}},\"escaped\":\"\\u0061\"}}  \r\n\r\n"
        );

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert_eq!(updated, input);
    }

    #[test]
    fn install_migrates_wildcard_session_start_and_preserves_user_hook() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let user_hook = r#"{ "type" : "command", "command" : "echo keep", "timeout" : 3 }"#;
        let input = format!(
            "{{\n  \"hooks\": {{\n    \"SessionStart\": [{{\"matcher\":\"*\",\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":10}},{user_hook}]}}]\n  }}\n}}\n\n"
        );
        let installed = install(&input, settings_path, hook_path).unwrap();
        assert!(installed.contains(user_hook));
        assert!(installed.ends_with("}\n\n"));
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["matcher"], "*");
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "echo keep");
        assert_eq!(
            groups[1],
            canonical_hook_value(&CANONICAL_HOOKS[0], hook_path)
        );
        assert_eq!(
            install(&installed, settings_path, hook_path).unwrap(),
            installed
        );

        let removed = uninstall(&installed, settings_path, hook_path).unwrap();
        assert!(removed.contains(user_hook));
        assert!(!removed.contains(&command));
        let settings: Value = serde_json::from_str(&removed).unwrap();
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn install_preserves_canonical_session_start_position_during_migration() {
        let (settings_path, hook_path) = paths();
        let canonical = canonical_hook_json(&CANONICAL_HOOKS[0], hook_path).unwrap();
        let post = canonical_hook_json(&CANONICAL_HOOKS[1], hook_path).unwrap();
        let stop = canonical_hook_json(&CANONICAL_HOOKS[2], hook_path).unwrap();
        let old_command = serde_json::to_string(&hook_command(hook_path, Some("working"))).unwrap();
        let session_start = format!(
            "\"SessionStart\":[{canonical},{{\"matcher\":\"foreign\",\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]}}]"
        );
        let old_event = [
            "\"PostToolUse\":[{\"matcher\":\"*\",\"hooks\":[{\"type\":\"command\",\"command\":",
            &old_command,
            "}]}]",
        ]
        .concat();
        let input = ["{\"hooks\":{", &session_start, ",", &old_event, "}}"].concat();
        let expected = [
            "{\"hooks\":{",
            &session_start,
            ",\"PostToolUse\":[",
            &post,
            "],\"Stop\":[",
            &stop,
            "]}}",
        ]
        .concat();

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert_eq!(updated, expected);
    }

    #[test]
    fn install_removes_only_owned_commands_from_shared_hook_groups() {
        let (settings_path, hook_path) = paths();
        let old_command = serde_json::to_string(&hook_command(hook_path, Some("working"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "  \"hooks\": {{\n",
                "    \"PostToolUse\": [{{\n",
                "      \"matcher\": \"*\",\n",
                "      \"hooks\": [\n",
                "        {{\"type\":\"command\",\"command\":{old_command},\"timeout\":10}},\n",
                "        {{  \"type\" : \"command\", \"command\" : \"echo keep\", \"timeout\" : 3  }}\n",
                "      ]\n",
                "    }}],\n",
                "    \"Notification\": [{{\"matcher\":\"keep\",\"hooks\":[]}}]\n",
                "  }}\n",
                "}}\n",
            ),
            old_command = old_command,
        );

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert!(!updated.contains(&old_command));
        assert!(updated.contains(
            "        {  \"type\" : \"command\", \"command\" : \"echo keep\", \"timeout\" : 3  }"
        ));
        assert!(updated.contains("    \"Notification\": [{\"matcher\":\"keep\",\"hooks\":[]}]"));
        let parsed: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "echo keep"
        );
        assert_eq!(
            parsed["hooks"]["PostToolUse"][1],
            canonical_hook_value(&CANONICAL_HOOKS[1], hook_path)
        );
        assert_eq!(parsed["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn uninstall_preserves_unrelated_hook_text() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "    \"before\" : \"\\u0061\",\n",
                "    \"hooks\" : {{\n",
                "        \"SessionStart\" : [{{\n",
                "            \"matcher\" : \"*\",\n",
                "            \"hooks\" : [\n",
                "                {{\"type\":\"command\",\"command\":{command},\"timeout\":10}},\n",
                "                {{  \"type\" : \"command\", \"command\" : \"echo keep\"  }}\n",
                "            ]\n",
                "        }}]\n",
                "    }},\n",
                "    \"after\" : 1e+02\n",
                "}}\n\n",
            ),
            command = command,
        );

        let updated = uninstall(&input, settings_path, hook_path).unwrap();

        assert_ne!(updated, input);
        assert!(!updated.contains(&command));
        assert!(updated
            .contains("                {  \"type\" : \"command\", \"command\" : \"echo keep\"  }"));
        assert!(updated.starts_with("{\n    \"before\" : \"\\u0061\","));
        assert!(updated.ends_with("    \"after\" : 1e+02\n}\n\n"));
    }

    #[test]
    fn install_adds_async_cache_hooks_for_stop_and_post_tool_use() {
        let (settings_path, hook_path) = paths();
        let installed = install("{}", settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let cache_entry = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": hook_command(hook_path, Some("cache")),
                "timeout": 10,
                "async": true,
            }],
        });

        assert_eq!(settings["hooks"]["PostToolUse"][0], cache_entry);
        assert_eq!(settings["hooks"]["Stop"][0], cache_entry);
        assert!(settings["hooks"]["PostToolUse"][0].get("matcher").is_none());
        assert_eq!(
            settings["hooks"]["SessionStart"][0],
            canonical_hook_value(&CANONICAL_HOOKS[0], hook_path)
        );
    }

    #[test]
    fn install_with_cache_hooks_is_idempotent_and_uninstall_removes_them() {
        let (settings_path, hook_path) = paths();
        let once = install("{}", settings_path, hook_path).unwrap();

        assert_eq!(install(&once, settings_path, hook_path).unwrap(), once);

        let removed = uninstall(&once, settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&removed).unwrap();
        for event in ["PostToolUse", "Stop", "SessionStart"] {
            assert!(settings["hooks"].get(event).is_none(), "{event} remains");
        }
    }

    #[test]
    fn install_replaces_stale_cache_hook_variant() {
        let (settings_path, hook_path) = paths();
        let cache_command = serde_json::to_string(&hook_command(hook_path, Some("cache"))).unwrap();
        for input in [
            format!(
                "{{\"hooks\":{{\"Stop\":[{{\"hooks\":[{{\"type\":\"command\",\"command\":{cache_command},\"timeout\":5}}]}}]}}}}"
            ),
            format!(
                "{{\n  \"hooks\": {{\n    \"Stop\": [\n      {{\"hooks\":[{{\"type\":\"command\",\"command\":{cache_command},\"timeout\":5}}]}}\n    ]\n  }}\n}}\n"
            ),
        ] {
            let installed = install(&input, settings_path, hook_path).unwrap();
            let settings: Value = serde_json::from_str(&installed).unwrap();
            let stop = settings["hooks"]["Stop"].as_array().unwrap();

            assert_eq!(stop.len(), 1);
            assert_eq!(stop[0], canonical_hook_value(&CANONICAL_HOOKS[2], hook_path));
        }
    }

    #[test]
    fn install_rejects_duplicate_keys() {
        let (settings_path, hook_path) = paths();
        let error = install(
            r#"{"alpha": 1, "alpha": 2, "hooks": {}}"#,
            settings_path,
            hook_path,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("duplicate key \"alpha\""), "{error}");
    }

    #[cfg(not(windows))]
    #[test]
    fn install_statusline_wraps_with_the_tap_path() {
        let (settings_path, _) = paths();
        let tap_path = Path::new("/home/test/.claude/hooks/herdr-statusline-tap.sh");
        let input = r#"{"statusLine":{"type":"command","command":"bash ~/.claude/tokenline.sh"}}"#;

        let installed = install_statusline(input, settings_path, tap_path).unwrap();
        let command = serde_json::from_str::<Value>(&installed).unwrap()["statusLine"]["command"]
            .as_str()
            .unwrap()
            .to_string();

        assert_eq!(
            super::super::statusline_tap::unwrap_statusline_command(&command),
            Some("bash ~/.claude/tokenline.sh".to_string())
        );
        assert!(
            command.contains(&tap_path.display().to_string()),
            "{command}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn uninstall_statusline_restores_the_user_command() {
        let (settings_path, _) = paths();
        let tap_path = Path::new("/home/test/.claude/hooks/herdr-statusline-tap.sh");
        let input = "{\n  \"statusLine\": {\"type\": \"command\", \"command\": \"bash ~/.claude/tokenline.sh\"}\n}\n";

        let installed = install_statusline(input, settings_path, tap_path).unwrap();

        assert_ne!(installed, input);
        assert_eq!(
            uninstall_statusline(&installed, settings_path).unwrap(),
            input
        );
    }

    #[test]
    fn install_keeps_structurally_invalid_content_unchanged() {
        let (settings_path, hook_path) = paths();
        for input in ["[]", r#"{"hooks": []}"#, r#"{"hooks":{"SessionStart":{}}}"#] {
            assert!(install(input, settings_path, hook_path).is_err());
        }
    }
}
