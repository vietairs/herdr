mod attach;
#[cfg(unix)]
mod host_unix;

pub mod federation;

pub(crate) use attach::*;
#[cfg(unix)]
pub(crate) use host_unix::run_remote_client_bridge;

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

/// Whether ssh output describes an authentication rejection, as opposed to any
/// other failure (host unreachable, unknown host key, no ssh binary).
///
/// Shared by the post-failure hint above and the pre-attach probe in `attach`,
/// which matches against a child's raw stderr rather than an
/// [`std::io::Error`].
/// Whether ssh output says it skipped a local private key it found.
///
/// ssh refuses to load a key whose file is readable by accounts other than the
/// owner, reports that on stderr, and then continues as if no key existed —
/// so the connection is rejected exactly like a host that accepts no key at
/// all. Distinguishing the two matters: the fix is local file permissions, not
/// installing a key on the remote.
pub(crate) fn reports_ignored_local_key(message: &str) -> bool {
    message.contains("bad permissions")
        || message.contains("UNPROTECTED PRIVATE KEY FILE")
        || message.contains("Permissions for")
}

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
        // Real Win32-OpenSSH stderr: the key is found, skipped, and the
        // connection then fails with the same rejection a keyless host gives.
        let skipped = concat!(
            "Permissions for '/home/u/.ssh/id_ed25519' are too open.\n",
            "Load key \"/home/u/.ssh/id_ed25519\": bad permissions\n",
            "u@host: Permission denied (publickey,password).\n",
        );
        assert!(reports_ssh_auth_failure(skipped));
        assert!(reports_ignored_local_key(skipped));

        let no_key = "u@host: Permission denied (publickey,password).\n";
        assert!(reports_ssh_auth_failure(no_key));
        assert!(!reports_ignored_local_key(no_key));
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
