//! Wraps an agent's EXISTING statusline command with a herdr tap file (macOS/Linux only).
//!
//! The settings command keeps the user's original verbatim inside a
//! self-healing `sh -c` program, so the statusline still renders when the tap
//! file is missing, and uninstall can restore the original exactly. A
//! statusline is never created: a custom statusline changes the agent's own
//! footer, which is the user's choice, not herdr's.

use std::ffi::OsStr;
use std::io;
use std::path::Path;

use jsonc_parser::cst::{CstInputValue, CstObject, CstObjectProp, CstRootNode, CstStringLit};
use jsonc_parser::ParseOptions;

use super::command::shell_single_quote;

/// File name of every herdr tap asset. A command is a herdr tap iff it has the wrapped form below and
/// its tap path's file name is this name, in any directory (a moved config dir is still recognised).
pub(crate) const STATUSLINE_TAP_FILE_NAME: &str = "herdr-statusline-tap.sh";
/// Self-healing program: run the tap when its file is readable, otherwise run the original directly.
pub(crate) const STATUSLINE_TAP_SCRIPT: &str =
    r#"[ -r "$0" ] && exec sh "$0" "$1"; exec sh -c "$1""#;

/// `sh -c {q(STATUSLINE_TAP_SCRIPT)} {q(tap_path)} {q(original)}` with q = `shell_single_quote`, e.g.
/// `sh -c '[ -r "$0" ] && exec sh "$0" "$1"; exec sh -c "$1"' '/home/u/.claude/hooks/herdr-statusline-tap.sh' 'bash ~/.claude/tokenline.sh'`.
pub(crate) fn wrap_statusline_command(tap_path: &Path, original: &str) -> String {
    format!(
        "{}{} {}",
        wrapped_prefix(),
        shell_single_quote(&tap_path.display().to_string()),
        shell_single_quote(original)
    )
}

/// `Some(original)` when `command` is exactly the wrapped form (any tap path whose file name is
/// STATUSLINE_TAP_FILE_NAME), else `None`.
pub(crate) fn unwrap_statusline_command(command: &str) -> Option<String> {
    let rest = command.strip_prefix(wrapped_prefix().as_str())?;
    let (tap_path, rest) = take_single_quoted(rest)?;
    let rest = rest.strip_prefix(' ')?;
    let (original, rest) = take_single_quoted(rest)?;
    if !rest.is_empty() {
        return None;
    }
    if Path::new(&tap_path).file_name() != Some(OsStr::new(STATUSLINE_TAP_FILE_NAME)) {
        return None;
    }
    Some(original)
}

/// The fixed text in front of the tap path: `sh -c '<script>' `.
fn wrapped_prefix() -> String {
    format!("sh -c {} ", shell_single_quote(STATUSLINE_TAP_SCRIPT))
}

/// Parses ONE token exactly as `shell_single_quote` emits it (`'...'` runs joined by `"'"`) from the start of
/// `input`; returns the value and the rest. Any other text → `None`.
fn take_single_quoted(input: &str) -> Option<(String, &str)> {
    // An embedded quote closes the run, adds a double-quoted quote, and opens the next run.
    const EMBEDDED_QUOTE: &str = "\"'\"'";
    let mut rest = input.strip_prefix('\'')?;
    let mut value = String::new();
    loop {
        let end = rest.find('\'')?;
        value.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        match rest.strip_prefix(EMBEDDED_QUOTE) {
            Some(next_run) => {
                value.push('\'');
                rest = next_run;
            }
            None => return Some((value, rest)),
        }
    }
}

/// Edits only `<key>.command` through the JSONC CST (formatting and comments preserved):
/// key absent → unchanged (a statusline is never created);
/// `{"type": "command", "command": <string>}` that is not a herdr tap → command := wrap(tap_path, original);
/// a herdr tap (any tap path) → command := wrap(tap_path, its original): byte-identical when the path is
///   unchanged, re-pointed (never nested) when it changed;
/// any other shape → unchanged.
pub(crate) fn install_statusline_tap(
    content: &str,
    settings_path: &Path,
    key: &str,
    tap_path: &Path,
) -> io::Result<String> {
    let root = parse_root(content, settings_path)?;
    let Some((literal, command)) = statusline_command(&root, key) else {
        return Ok(content.to_string());
    };
    let original = unwrap_statusline_command(&command).unwrap_or_else(|| command.clone());
    let wrapped = wrap_statusline_command(tap_path, &original);
    if wrapped == command {
        return Ok(content.to_string());
    }
    literal.replace_with(CstInputValue::String(wrapped));
    let updated = root.to_string();

    let written = written_command(&updated, settings_path, key)?;
    if unwrap_statusline_command(&written).as_deref() != Some(original.as_str()) {
        return Err(unsafe_edit_error(settings_path, key));
    }
    Ok(updated)
}

/// A herdr tap → command := its original; anything else → unchanged.
pub(crate) fn uninstall_statusline_tap(
    content: &str,
    settings_path: &Path,
    key: &str,
) -> io::Result<String> {
    let root = parse_root(content, settings_path)?;
    let Some((literal, command)) = statusline_command(&root, key) else {
        return Ok(content.to_string());
    };
    let Some(original) = unwrap_statusline_command(&command) else {
        return Ok(content.to_string());
    };
    literal.replace_with(CstInputValue::String(original.clone()));
    let updated = root.to_string();

    if written_command(&updated, settings_path, key)? != original {
        return Err(unsafe_edit_error(settings_path, key));
    }
    Ok(updated)
}

fn parse_root(content: &str, settings_path: &Path) -> io::Result<CstRootNode> {
    CstRootNode::parse(content, &ParseOptions::default()).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })
}

/// The command string literal and its decoded value when `<key>` is exactly
/// one `{"type": "command", "command": <string>}` object; `None` for any other shape.
fn statusline_command(root: &CstRootNode, key: &str) -> Option<(CstStringLit, String)> {
    let root_object = root.value()?.as_object()?;
    let statusline = single_property(&root_object, key)?.object_value()?;
    let kind = single_property(&statusline, "type")?
        .value()?
        .as_string_lit()?
        .decoded_value()
        .ok()?;
    if kind != "command" {
        return None;
    }
    let literal = single_property(&statusline, "command")?
        .value()?
        .as_string_lit()?;
    let command = literal.decoded_value().ok()?;
    Some((literal, command))
}

/// The property named `name`, unless it is missing or duplicated (a
/// duplicated key is ambiguous, so it is left alone).
fn single_property(object: &CstObject, name: &str) -> Option<CstObjectProp> {
    let mut matching = object.properties().into_iter().filter(|property| {
        property
            .name()
            .and_then(|property_name| property_name.decoded_value().ok())
            .is_some_and(|property_name| property_name == name)
    });
    let property = matching.next()?;
    matching.next().is_none().then_some(property)
}

/// Re-reads the command an edit produced, failing when the edit lost it.
fn written_command(updated: &str, settings_path: &Path, key: &str) -> io::Result<String> {
    let root = parse_root(updated, settings_path)?;
    statusline_command(&root, key)
        .map(|(_, command)| command)
        .ok_or_else(|| unsafe_edit_error(settings_path, key))
}

fn unsafe_edit_error(settings_path: &Path, key: &str) -> io::Error {
    io::Error::other(format!(
        "failed to safely update {key}.command in {}",
        settings_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integration::command::shell_single_quote;

    const P: &str = "/h/.claude/hooks/herdr-statusline-tap.sh";
    const SETTINGS: &str = "/h/.claude/settings.json";
    const KEY: &str = "statusLine";

    /// Every tap asset; agents with a tap append their asset here.
    const TAP_ASSETS: &[(&str, &str)] = &[
        (
            "claude",
            include_str!("assets/claude/herdr-statusline-tap.sh"),
        ),
        (
            "antigravity_cli",
            include_str!("assets/antigravity_cli/herdr-statusline-tap.sh"),
        ),
        (
            "copilot",
            include_str!("assets/copilot/herdr-statusline-tap.sh"),
        ),
        (
            "cursor",
            include_str!("assets/cursor/herdr-statusline-tap.sh"),
        ),
    ];
    const PASSTHROUGH_BEGIN: &str = "# --- herdr statusline passthrough begin ---";
    const PASSTHROUGH_END: &str = "# --- herdr statusline passthrough end ---";

    fn hostile_commands() -> [&'static str; 5] {
        [
            "",
            "bash ~/.claude/tokenline.sh",
            "echo 'a'\"b\" $HOME \\ `x`",
            "printf 'line1\nline2'",
            "echo \u{e9}\u{1f600}",
        ]
    }

    fn tap_path() -> &'static Path {
        Path::new(P)
    }

    fn install(content: &str) -> String {
        install_statusline_tap(content, Path::new(SETTINGS), KEY, tap_path()).unwrap()
    }

    fn uninstall(content: &str) -> String {
        uninstall_statusline_tap(content, Path::new(SETTINGS), KEY).unwrap()
    }

    fn command_of(content: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(content).unwrap();
        value[KEY]["command"].as_str().unwrap().to_string()
    }

    fn statusline_settings(command: &str) -> String {
        format!(
            "{{\n  \"model\": \"opus\",\n  \"statusLine\": {{\n    \"type\": \"command\",\n    \"command\": {},\n    \"padding\": 0\n  }}\n}}\n",
            serde_json::to_string(command).unwrap()
        )
    }

    fn passthrough_block(asset: &str) -> &str {
        let begin = asset.find(PASSTHROUGH_BEGIN).unwrap();
        let end = asset.find(PASSTHROUGH_END).unwrap();
        &asset[begin + PASSTHROUGH_BEGIN.len()..end]
    }

    #[test]
    fn single_quoted_tokens_round_trip_hostile_commands() {
        for command in hostile_commands() {
            assert_eq!(
                take_single_quoted(&shell_single_quote(command)),
                Some((command.to_string(), "")),
                "{command:?}"
            );
        }
        assert_eq!(take_single_quoted("'unterminated"), None);
        assert_eq!(take_single_quoted("plain"), None);
    }

    #[test]
    fn wrap_matches_the_self_healing_form() {
        assert_eq!(
            wrap_statusline_command(tap_path(), "x"),
            r#"sh -c '[ -r "$0" ] && exec sh "$0" "$1"; exec sh -c "$1"' '/h/.claude/hooks/herdr-statusline-tap.sh' 'x'"#
        );
    }

    #[test]
    fn wrap_and_unwrap_are_inverse() {
        for command in hostile_commands() {
            assert_eq!(
                unwrap_statusline_command(&wrap_statusline_command(tap_path(), command)),
                Some(command.to_string()),
                "{command:?}"
            );
        }
    }

    #[test]
    fn unwrap_recognises_a_tap_at_any_path_and_rejects_foreign_commands() {
        let moved =
            wrap_statusline_command(Path::new("/old/dir/herdr-statusline-tap.sh"), "bash ~/x.sh");
        assert_eq!(
            unwrap_statusline_command(&moved),
            Some("bash ~/x.sh".to_string())
        );

        let foreign_file = wrap_statusline_command(Path::new("/old/dir/other.sh"), "bash ~/x.sh");
        let trailing = format!(
            "{} extra",
            wrap_statusline_command(tap_path(), "bash ~/x.sh")
        );
        for command in ["bash ~/x.sh", foreign_file.as_str(), trailing.as_str()] {
            assert_eq!(unwrap_statusline_command(command), None, "{command}");
        }
    }

    #[test]
    fn install_without_statusline_leaves_content_untouched() {
        let pretty = "{\n  // keep me\n  \"model\": \"opus\",\n  \"hooks\": {}\n}\n";
        for input in ["{}", pretty] {
            assert_eq!(install(input), input);
        }
    }

    #[test]
    fn install_wraps_existing_command_and_preserves_other_keys() {
        let input = r#"{"statusLine": {"type": "command", "command": "bash ~/.claude/tokenline.sh", "padding": 0, "refreshInterval": 5}}"#;
        let wrapped = wrap_statusline_command(tap_path(), "bash ~/.claude/tokenline.sh");

        let installed = install(input);

        assert_eq!(command_of(&installed), wrapped);
        let expected = input.replace(
            "\"bash ~/.claude/tokenline.sh\"",
            &serde_json::to_string(&wrapped).unwrap(),
        );
        assert_eq!(installed, expected);
        let value: serde_json::Value = serde_json::from_str(&installed).unwrap();
        assert_eq!(value[KEY]["padding"], 0);
        assert_eq!(value[KEY]["refreshInterval"], 5);
    }

    #[test]
    fn install_is_idempotent_and_never_wraps_twice() {
        let once = install(&statusline_settings("bash ~/.claude/tokenline.sh"));

        assert_eq!(install(&once), once);
        assert_eq!(
            unwrap_statusline_command(&command_of(&once)),
            Some("bash ~/.claude/tokenline.sh".to_string())
        );
    }

    #[test]
    fn install_repoints_a_tap_from_another_path_instead_of_nesting() {
        let old = wrap_statusline_command(
            Path::new("/old/herdr-statusline-tap.sh"),
            "bash ~/.claude/tokenline.sh",
        );

        let command = command_of(&install(&statusline_settings(&old)));

        assert_eq!(
            unwrap_statusline_command(&command),
            Some("bash ~/.claude/tokenline.sh".to_string())
        );
        assert_eq!(command.matches(P).count(), 1);
        assert!(!command.contains("/old/"), "{command}");
    }

    #[test]
    fn install_leaves_unknown_shapes_untouched() {
        for input in [
            r#"{"statusLine": "bash ~/x.sh"}"#,
            r#"{"statusLine": {"type":"static"}}"#,
            r#"{"statusLine": {"type": "command", "command": 5}}"#,
        ] {
            assert_eq!(install(input), input);
        }
    }

    #[test]
    fn uninstall_restores_original_bytes() {
        let input = concat!(
            "{\n",
            "  // the user's own statusline\n",
            "  \"statusLine\": {\n",
            "    \"type\": \"command\",\n",
            "    \"command\": \"bash ~/.claude/tokenline.sh\",\n",
            "    \"padding\": 0\n",
            "  },\n",
            "  \"permissions\": { \"allow\": [\"Read\"] }\n",
            "}\n",
        );

        let installed = install(input);

        assert_ne!(installed, input);
        assert_eq!(uninstall(&installed), input);
    }

    #[test]
    fn uninstall_leaves_a_user_command_alone() {
        let input = statusline_settings("bash ~/.claude/tokenline.sh");
        assert_eq!(uninstall(&input), input);
    }

    #[test]
    fn tap_assets_share_the_passthrough_block() {
        let (_, claude_asset) = TAP_ASSETS[0];
        let reference = passthrough_block(claude_asset);
        assert!(!reference.trim().is_empty());
        for (agent, asset) in TAP_ASSETS {
            assert_eq!(passthrough_block(asset), reference, "{agent}");
            assert!(
                asset.contains(&format!("# HERDR_INTEGRATION_ID={agent}")),
                "{agent}"
            );
        }
    }
}
