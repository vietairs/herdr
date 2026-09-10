mod args;
mod attach;
#[cfg(unix)]
mod host_unix;
mod process;
mod restart_policy;
mod saved;

pub mod federation;

pub(crate) use args::*;
pub(crate) use attach::*;
#[cfg(unix)]
pub(crate) use host_unix::run_remote_client_bridge;
pub(crate) use saved::*;

#[cfg(windows)]
pub(crate) fn run_remote_client_bridge() -> std::io::Result<()> {
    Err(std::io::Error::other(
        "remote Windows hosts are not supported yet",
    ))
}

#[cfg(windows)]
pub(crate) fn run_federation_serve_bridge() -> std::io::Result<()> {
    Err(std::io::Error::other(
        "federation serve bridge is not supported on Windows yet",
    ))
}

pub(crate) fn print_remote_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_auth_error(err) {
        eprintln!(
            "hint: verify SSH access first with `{}`.",
            ssh_check_command(target)
        );
        eprintln!(
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before running `herdr --remote`."
        );
    }
}

fn is_remote_auth_error(err: &std::io::Error) -> bool {
    reports_ssh_auth_failure(&err.to_string())
}

/// Whether ssh output says it skipped a local private key it found.
///
/// ssh refuses to load a key whose file is readable by accounts other than the
/// owner, reports that on stderr, and then continues as if no key existed — so
/// the connection is rejected exactly like a host that accepts no key at all.
/// Distinguishing the two matters: the fix is local file permissions, not
/// installing a key on the remote.
pub(crate) fn reports_ignored_local_key(message: &str) -> bool {
    (message.contains("Permissions for") && message.contains("are too open"))
        || message.contains("bad permissions")
        || message.contains("UNPROTECTED PRIVATE KEY FILE")
}

/// The key file ssh named while refusing to load it, so a hint can point at the
/// file the user actually has rather than guessing a conventional name.
///
/// Two shapes have to be read. `Load key "<path>": bad permissions` is the one
/// both platforms print. The `Permissions ... for '<path>' are too open` line
/// carries an octal mode on OpenSSH proper and none on Win32-OpenSSH, so the
/// mode is skipped rather than matched.
///
/// Returns `None` when ssh reported the refusal without a usable path, which
/// leaves the caller to fall back to the conventional location.
pub(crate) fn ignored_local_key_path(message: &str) -> Option<&str> {
    if let Some(path) = message
        .split_once("Load key \"")
        .and_then(|(_, after)| after.split_once('"'))
        .filter(|(path, tail)| tail.starts_with(':') && is_safe_key_path(path))
        .map(|(path, _)| path)
    {
        return Some(path);
    }

    let after = message.split_once("ermissions ")?.1;
    let (path, tail) = after.split_once("for '")?.1.split_once('\'')?;
    (tail.trim_start().starts_with("are too open") && is_safe_key_path(path)).then_some(path)
}

/// Whether a path read out of ssh's output is safe to place inside a shell
/// command the user is told to paste.
///
/// A hostile server can write anything to this stderr before authentication
/// fails, and PowerShell expands `$(...)` and backticks inside double quotes.
/// Anything carrying those, a quote, or a newline is discarded in favour of the
/// conventional path.
fn is_safe_key_path(path: &str) -> bool {
    !path.is_empty() && !path.contains(['$', '`', '\'', '"', '\n', '\r', ';', '|', '&'])
}

/// Whether ssh output describes an authentication rejection, as opposed to any
/// other failure (host unreachable, unknown host key, no ssh binary).
///
/// Shared by the post-failure hint above and the pre-attach probe in `attach`,
/// which matches against a child's raw stderr rather than an
/// [`std::io::Error`].
pub(crate) fn reports_ssh_auth_failure(message: &str) -> bool {
    message.contains("Permission denied")
        && (message.contains("(publickey")
            || message.contains("(keyboard-interactive")
            || message.contains("(password"))
}

fn ssh_check_command(target: &str) -> String {
    format!("ssh {}", shell_quote(target))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let err = std::io::Error::other(
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let err = std::io::Error::other(
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let err = std::io::Error::other("remote platform detection failed: unsupported platform");

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn ignored_local_key_is_told_apart_from_a_host_that_accepts_no_key() {
        // Captured from OpenSSH_for_Windows_9.5p2. Note the mixed separators
        // and the absence of an octal mode, both of which the parser must take.
        let win32 = concat!(
            "Bad permissions. Try removing permissions for user: EXAMPLE on file ",
            "C:/Users/u/.ssh/id_ed25519.\n",
            "@         WARNING: UNPROTECTED PRIVATE KEY FILE!          @\n",
            r"Permissions for 'C:\Users\u/.ssh/id_ed25519' are too open.",
            "\n",
            "It is required that your private key files are NOT accessible by others.\n",
            "This private key will be ignored.\n",
            r#"Load key "C:\Users\u/.ssh/id_ed25519": bad permissions"#,
            "\n",
            "u@host: Permission denied (publickey,password).\n",
        );
        assert!(reports_ssh_auth_failure(win32));
        assert!(reports_ignored_local_key(win32));
        assert_eq!(
            ignored_local_key_path(win32),
            Some(r"C:\Users\u/.ssh/id_ed25519")
        );

        // OpenSSH proper puts the octal mode between "Permissions" and "for".
        let posix = concat!(
            "@         WARNING: UNPROTECTED PRIVATE KEY FILE!          @\n",
            "Permissions 0644 for '/home/u/.ssh/id_rsa' are too open.\n",
            r#"Load key "/home/u/.ssh/id_rsa": bad permissions"#,
            "\n",
            "u@host: Permission denied (publickey,password).\n",
        );
        assert!(reports_ignored_local_key(posix));
        assert_eq!(ignored_local_key_path(posix), Some("/home/u/.ssh/id_rsa"));

        // A host that simply accepts no key looks the same from the exit
        // status, and must not be sent to fix file permissions.
        let no_key = "u@host: Permission denied (publickey,password).\n";
        assert!(reports_ssh_auth_failure(no_key));
        assert!(!reports_ignored_local_key(no_key));
        assert_eq!(ignored_local_key_path(no_key), None);
    }

    #[test]
    fn a_key_path_that_could_break_out_of_a_pasted_command_is_not_reported() {
        // The server writes this stderr, so it must not reach a hint the user
        // is told to paste into PowerShell, where `$(...)` expands.
        let hostile = concat!(
            r#"Load key "/home/u/$(calc).key": bad permissions"#,
            "\n",
            "Permissions 0644 for '/home/u/$(calc).key' are too open.\n",
            "u@host: Permission denied (publickey,password).\n",
        );
        assert!(reports_ignored_local_key(hostile));
        assert_eq!(ignored_local_key_path(hostile), None);
    }

    #[test]
    fn a_truncated_permissions_line_yields_no_path_instead_of_panicking() {
        for message in [
            "Permissions for '",
            "Permissions 0644 for '/home/u/id_rsa",
            "Permissions for '/home/u/id_rsa' are fine.",
            r#"Load key ""#,
            r#"Load key "/home/u/id_rsa" bad permissions"#,
        ] {
            assert_eq!(ignored_local_key_path(message), None, "{message}");
        }
    }

    #[test]
    fn auth_failure_matcher_reads_raw_ssh_stderr() {
        // The pre-attach probe matches a child's stderr, not an io::Error.
        assert!(reports_ssh_auth_failure(
            "user@host: Permission denied (publickey,password).
"
        ));
        assert!(reports_ssh_auth_failure(
            "user@host: Permission denied (keyboard-interactive).
"
        ));
    }

    #[test]
    fn auth_failure_matcher_ignores_non_auth_ssh_stderr() {
        // These must stay silent: the provisioning step that follows reports
        // them far better than the probe could.
        assert!(!reports_ssh_auth_failure(
            "ssh: connect to host host port 22: Connection refused
"
        ));
        assert!(!reports_ssh_auth_failure(
            "Host key verification failed.
"
        ));
        assert!(!reports_ssh_auth_failure(""));
    }

    #[test]
    fn ssh_check_command_quotes_remote_target() {
        assert_eq!(ssh_check_command("host name"), "ssh 'host name'");
    }
}
