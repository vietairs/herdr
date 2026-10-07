//! The executable statusline wrapper for agents that may run their
//! `statusLine.command` without a shell (Copilot and Cursor; macOS/Linux only).
//!
//! The agent config points at a bare wrapper path. The user's original command
//! is kept twice: inside the wrapper, and in a backup file next to the agent
//! config, so losing the wrapper (a deleted hooks directory, a moved config
//! directory) never loses the original. Uninstall prefers the wrapper's copy
//! and falls back to the backup, which names the wrapper it belongs to. Both
//! copies are kept while the agent config may still name the wrapper.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
/// Backup key of the original command:
/// `{"statusLine.command": <original>, "wrapper": <wrapper path>}`.
const STATUSLINE_BACKUP_KEY: &str = "statusLine.command";
/// Backup key of the wrapper path the backup belongs to.
const STATUSLINE_BACKUP_WRAPPER_KEY: &str = "wrapper";

/// What uninstall found in `statusLine.command`.
pub(crate) enum WrapperRestore {
    /// Not a herdr wrapper: nothing to restore.
    NotWrapped,
    /// A herdr wrapper; the user's original command to put back.
    Restored(String),
    /// A herdr wrapper whose file and its own backup are both gone.
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

/// The original command in `config_dir`'s backup file when that backup
/// belongs to `wrapper_command`; `None` when there is no backup file or it
/// belongs to another wrapper, whose original this one never wrapped.
fn read_backup(config_dir: &Path, wrapper_command: &str) -> io::Result<Option<String>> {
    let backup_path = config_dir.join(STATUSLINE_BACKUP_FILE_NAME);
    let content = match fs::read_to_string(&backup_path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let backup = serde_json::from_str::<Value>(&content)
        .ok()
        .and_then(|value| {
            let original = value.get(STATUSLINE_BACKUP_KEY)?.as_str()?.to_string();
            let wrapper = value
                .get(STATUSLINE_BACKUP_WRAPPER_KEY)
                .and_then(Value::as_str)
                .map(str::to_string);
            Some((original, wrapper))
        })
        .ok_or_else(|| {
            io::Error::other(format!(
                "cannot read the original statusLine.command from {}; restore the command in the agent config file by hand",
                backup_path.display()
            ))
        })?;
    Ok(match backup {
        (original, Some(wrapper)) if wrapper == wrapper_command => Some(original),
        _ => None,
    })
}

fn write_backup(config_dir: &Path, wrapper_command: &str, original: &str) -> io::Result<()> {
    let mut backup = Map::new();
    backup.insert(
        STATUSLINE_BACKUP_KEY.to_string(),
        Value::String(original.to_string()),
    );
    backup.insert(
        STATUSLINE_BACKUP_WRAPPER_KEY.to_string(),
        Value::String(wrapper_command.to_string()),
    );
    fs::write(
        config_dir.join(STATUSLINE_BACKUP_FILE_NAME),
        serde_json::to_string_pretty(&Value::Object(backup))? + "\n",
    )
}

/// Writes the wrapper beside `wrapper_path` and renames it into place, so an
/// agent running the statusline during a reinstall runs the old script or the
/// new one, never a partly written one.
fn replace_wrapper_file(wrapper_path: &Path, script: &str) -> io::Result<()> {
    let staging = wrapper_path.with_file_name(format!(
        ".{STATUSLINE_WRAPPER_FILE_NAME}.{}.tmp",
        std::process::id()
    ));
    let replaced = fs::write(&staging, script)
        .and_then(|()| make_executable(&staging))
        .and_then(|()| fs::rename(&staging, wrapper_path));
    if replaced.is_err() {
        // Best effort: the staging file is herdr's own and never run.
        let _ = remove_file_if_exists(&staging);
    }
    replaced
}

/// The original command behind the wrapper path `wrapper_command`: the
/// wrapper's own copy, else its backup in `config_dir`. `None` only when the
/// wrapper file and its backup are both gone. A wrapper file that exists but
/// cannot be read, with no backup, is an error: it holds the only copy.
fn wrapped_original(wrapper_command: &str, config_dir: &Path) -> io::Result<Option<String>> {
    let wrapper_path = Path::new(wrapper_command);
    let wrapper_error = match read_wrapper_original(wrapper_path) {
        Ok(original) => return Ok(Some(original)),
        Err(err) => err,
    };
    match read_backup(config_dir, wrapper_command)? {
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
    write_backup(config_dir, &wrapper_command, &original)?;
    replace_wrapper_file(&wrapper_path, &statusline_wrapper_script(agent, &original)?)?;
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

/// Removes the tap in `wrapper_dir`, then the wrapper and the backup in
/// `config_dir` unless `config_path` may still name the wrapper: those two
/// hold the only copies of the original command. The wrapper runs the
/// original without the tap, so the tap always goes.
pub(crate) fn remove_statusline_wrapper_files(
    wrapper_dir: &Path,
    config_dir: &Path,
    config_path: &Path,
) -> io::Result<Option<String>> {
    remove_file_if_exists(&wrapper_dir.join(STATUSLINE_TAP_FILE_NAME))?;
    remove_files_unless_named(
        config_path,
        STATUSLINE_WRAPPER_FILE_NAME,
        &[
            wrapper_dir.join(STATUSLINE_WRAPPER_FILE_NAME),
            config_dir.join(STATUSLINE_BACKUP_FILE_NAME),
        ],
    )
}

/// Removes `files` unless `config_path` may still name `name`. They are then
/// kept, and the warning says so when any of them exists.
pub(crate) fn remove_files_unless_named(
    config_path: &Path,
    name: &str,
    files: &[PathBuf],
) -> io::Result<Option<String>> {
    if !config_may_name(config_path, name) {
        for file in files {
            remove_file_if_exists(file)?;
        }
        return Ok(None);
    }
    let kept = files
        .iter()
        .filter(|file| fs::symlink_metadata(file).is_ok())
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>();
    if kept.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "{INSTALL_WARNING_PREFIX} kept {} because {} may still name {name}; remove them by hand once it no longer does",
        kept.join(" and "),
        config_path.display()
    )))
}

/// Whether `config_path` may still name `name`: true unless the path is
/// absent, or the file was read and does not contain the name. A dangling
/// symlink is not absent, because its target may come back. The raw bytes are
/// searched, so a config that is not UTF-8 still counts.
fn config_may_name(config_path: &Path, name: &str) -> bool {
    match fs::read(config_path) {
        Ok(bytes) => bytes
            .windows(name.len())
            .any(|window| window == name.as_bytes()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::symlink_metadata(config_path).is_ok()
        }
        Err(_) => true,
    }
}
