//! The executable statusline wrapper for agents that may run their
//! `statusLine.command` without a shell (Copilot and Cursor; macOS/Linux only).
//!
//! The agent config points at a bare wrapper path. The user's original command
//! is kept twice: inside the wrapper, and in a backup file next to the agent
//! config, so losing the wrapper (a deleted hooks directory, a moved config
//! directory) never loses the original. Uninstall prefers the wrapper's copy
//! and falls back to the backup.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Map, Value};

use super::command::shell_single_quote;
use super::file_ops::{make_executable, remove_file_if_exists};
use super::statusline_tap::STATUSLINE_TAP_FILE_NAME;
use super::INSTALL_WARNING_PREFIX;

/// File name of the executable wrapper `statusLine.command` points at.
pub(crate) const STATUSLINE_WRAPPER_FILE_NAME: &str = "herdr-statusline-wrap.sh";
/// Wrapper line holding the user's original command as one JSON string.
const STATUSLINE_WRAPPER_BACKUP_PREFIX: &str = "# herdr-statusline-original: ";
/// Backup of the user's original command, next to the agent config file.
pub(crate) const STATUSLINE_BACKUP_FILE_NAME: &str = "herdr-statusline-original.json";
/// The one key of the backup file: `{"statusLine.command": <original>}`.
const STATUSLINE_BACKUP_KEY: &str = "statusLine.command";

/// What uninstall found in `statusLine.command`.
pub(crate) enum WrapperRestore {
    /// Not a herdr wrapper: nothing to restore.
    NotWrapped,
    /// A herdr wrapper; the user's original command to put back.
    Restored(String),
    /// A herdr wrapper whose file and backup are both gone.
    Lost,
}

/// A herdr wrapper is a bare path (no arguments) whose file name is the
/// wrapper name, in any directory, so a moved config dir is still recognised.
pub(crate) fn is_statusline_wrapper_command(command: &str) -> bool {
    !command.contains(char::is_whitespace)
        && Path::new(command).file_name() == Some(OsStr::new(STATUSLINE_WRAPPER_FILE_NAME))
}

/// An agent may exec the statusline command without a shell, so the wrapper
/// path must work unquoted both ways: no whitespace, quoting or expansion
/// characters.
fn is_bare_command_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_alphanumeric() || "/._-+@:,%=".contains(c))
}

/// The executable wrapper: runs the tap beside it while that file is readable,
/// else the original command directly. The original is embedded twice: as the
/// shell literal the script runs and as a JSON backup line uninstall reads.
/// `agent` only names the integration in the file's own explanatory comment.
fn statusline_wrapper_script(agent: &str, original: &str) -> io::Result<String> {
    let backup = serde_json::to_string(original)?;
    Ok(format!(
        "#!/bin/sh\n\
         # installed by herdr\n\
         # managed by herdr; uninstalling the {agent} integration restores the statusline command this file wraps.\n\
         {STATUSLINE_WRAPPER_BACKUP_PREFIX}{backup}\n\
         # runs the herdr tap beside this file when it is readable, else the original command; both read the same stdin.\n\
         statusline_original={quoted}\n\
         statusline_tap=\"$(dirname -- \"$0\")/{tap}\"\n\
         if [ -r \"$statusline_tap\" ]; then\n\
         \x20 exec sh \"$statusline_tap\" \"$statusline_original\"\n\
         fi\n\
         # an executable file runs as it is, the way an agent that runs the command without a shell ran it.\n\
         if [ -f \"$statusline_original\" ] && [ -x \"$statusline_original\" ]; then\n\
         \x20 exec \"$statusline_original\"\n\
         fi\n\
         exec sh -c \"$statusline_original\"\n",
        quoted = shell_single_quote(original),
        tap = STATUSLINE_TAP_FILE_NAME,
    ))
}

/// The original command a herdr wrapper file wraps, from its JSON backup line,
/// cross-checked against the shell literal the script itself runs.
fn read_wrapper_original(wrapper_path: &Path) -> io::Result<String> {
    let unreadable = |detail: &str| {
        io::Error::other(format!(
            "cannot read the original statusLine.command from {} ({detail}); restore the command in the agent config file by hand",
            wrapper_path.display()
        ))
    };
    let content = fs::read_to_string(wrapper_path).map_err(|err| unreadable(&err.to_string()))?;
    let backup = content
        .lines()
        .find_map(|line| line.strip_prefix(STATUSLINE_WRAPPER_BACKUP_PREFIX))
        .ok_or_else(|| unreadable("no backup line"))?;
    let original = serde_json::from_str::<String>(backup)
        .map_err(|err| unreadable(&format!("bad backup line: {err}")))?;
    let literal = format!("\nstatusline_original={}\n", shell_single_quote(&original));
    if !content.contains(&literal) {
        return Err(unreadable("the script does not match its backup"));
    }
    Ok(original)
}

/// The original command in `config_dir`'s backup file; `None` when there is no
/// backup file.
fn read_backup(config_dir: &Path) -> io::Result<Option<String>> {
    let backup_path = config_dir.join(STATUSLINE_BACKUP_FILE_NAME);
    let content = match fs::read_to_string(&backup_path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    serde_json::from_str::<Value>(&content)
        .ok()
        .and_then(|value| {
            value
                .get(STATUSLINE_BACKUP_KEY)
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(Some)
        .ok_or_else(|| {
            io::Error::other(format!(
                "cannot read the original statusLine.command from {}; restore the command in the agent config file by hand",
                backup_path.display()
            ))
        })
}

fn write_backup(config_dir: &Path, original: &str) -> io::Result<()> {
    let mut backup = Map::new();
    backup.insert(
        STATUSLINE_BACKUP_KEY.to_string(),
        Value::String(original.to_string()),
    );
    fs::write(
        config_dir.join(STATUSLINE_BACKUP_FILE_NAME),
        serde_json::to_string_pretty(&Value::Object(backup))? + "\n",
    )
}

/// The original command behind the wrapper path `wrapper_command`: the
/// wrapper's own copy, else the backup in `config_dir`. `None` only when the
/// wrapper file and the backup are both gone. A wrapper file that exists but
/// cannot be read, with no backup, is an error: it holds the only copy.
fn wrapped_original(wrapper_command: &str, config_dir: &Path) -> io::Result<Option<String>> {
    let wrapper_path = Path::new(wrapper_command);
    let wrapper_error = match read_wrapper_original(wrapper_path) {
        Ok(original) => return Ok(Some(original)),
        Err(err) => err,
    };
    match read_backup(config_dir)? {
        Some(original) => Ok(Some(original)),
        None if fs::symlink_metadata(wrapper_path).is_ok() => Err(wrapper_error),
        None => Ok(None),
    }
}

/// The wrapper path to store in place of the EXISTING `current` command, or
/// `None` to leave it as it is. Writes the backup into `config_dir`, then the
/// wrapper into `wrapper_dir`. An already wrapped command is re-pointed at
/// this directory's wrapper, never nested. An error means the statusline was
/// left untouched.
pub(crate) fn install_statusline_wrapper(
    agent: &str,
    current: &str,
    wrapper_dir: &Path,
    config_dir: &Path,
) -> io::Result<Option<String>> {
    let original = if is_statusline_wrapper_command(current) {
        wrapped_original(current, config_dir)?.ok_or_else(|| {
            io::Error::other(format!(
                "the herdr statusline wrapper {current} and its backup in {} are both gone; set statusLine.command back by hand",
                config_dir.display()
            ))
        })?
    } else {
        current.to_string()
    };
    let wrapper_path = wrapper_dir.join(STATUSLINE_WRAPPER_FILE_NAME);
    let wrapper_command = wrapper_path.display().to_string();
    if original.is_empty() || !is_bare_command_path(&wrapper_command) {
        tracing::warn!(
            path = %wrapper_path.display(),
            "left the {agent} statusline untouched: nothing to wrap or the wrapper path needs quoting"
        );
        return Ok(None);
    }
    // The backup is written first, so the original survives any later failure.
    write_backup(config_dir, &original)?;
    fs::write(&wrapper_path, statusline_wrapper_script(agent, &original)?)?;
    make_executable(&wrapper_path)?;
    Ok(Some(wrapper_command))
}

/// What to put back in place of `current` on uninstall. Only a wrapper file
/// that exists but cannot be read, with no usable backup, is an error.
pub(crate) fn restore_statusline_wrapper(
    current: &str,
    config_dir: &Path,
) -> io::Result<WrapperRestore> {
    if !is_statusline_wrapper_command(current) {
        return Ok(WrapperRestore::NotWrapped);
    }
    Ok(match wrapped_original(current, config_dir)? {
        Some(original) => WrapperRestore::Restored(original),
        None => WrapperRestore::Lost,
    })
}

/// The uninstall warning for `WrapperRestore::Lost`.
pub(crate) fn lost_wrapper_warning(config_path: &Path, current: &str) -> String {
    format!(
        "{INSTALL_WARNING_PREFIX} left statusLine.command in {} as {current}: the herdr statusline wrapper and its backup are both gone; set the command back by hand",
        config_path.display()
    )
}

/// The install warning for a statusline left unwrapped by an error.
pub(crate) fn unwrapped_statusline_warning(agent: &str, err: &io::Error) -> String {
    format!("{INSTALL_WARNING_PREFIX} left the {agent} statusline as it was: {err}")
}

/// Removes the wrapper and tap in `wrapper_dir` and the backup in `config_dir`.
pub(crate) fn remove_statusline_wrapper_files(
    wrapper_dir: &Path,
    config_dir: &Path,
) -> io::Result<()> {
    remove_file_if_exists(&wrapper_dir.join(STATUSLINE_WRAPPER_FILE_NAME))?;
    remove_file_if_exists(&wrapper_dir.join(STATUSLINE_TAP_FILE_NAME))?;
    remove_file_if_exists(&config_dir.join(STATUSLINE_BACKUP_FILE_NAME))?;
    Ok(())
}
