//! Remote-string sanitization (P7, requirement 1 / S11.1 blocker).
//!
//! Every remote-sourced *chrome* string (workspace/tab/pane labels, cwd,
//! agent name, terminal title, …) is neutralized of terminal control/ANSI/
//! OSC sequences before it is allowed to reach the ratatui buffer. Applied
//! at the single P4 ingest choke point (`reducer::namespace_workspace`/
//! `namespace_tab`/`namespace_pane`) so nothing downstream must remember to
//! escape a field.
//!
//! Explicitly NOT applied to raw PTY bytes destined for the ghostty pane
//! emulator (`pane_source::RemoteOnRead`) — those are legitimate terminal
//! content and the emulator is the correct sandbox for them (phase context,
//! requirement 1 note).
//!
//! # DRY check (deviation — logged in `implementation-notes.md`)
//! `crate::terminal_notify::sanitize_text` implements a near-identical
//! contract (strip ESC/BEL/ST, fold newlines to spaces) but is `fn`-private
//! to that module and scoped to single-line desktop-notification text. Not
//! reused directly: exposing it would couple an unrelated notification
//! helper to this new adversarial trust boundary's contract (this function
//! also strips the full C0/C1 control range and DEL, not just three bytes),
//! and this module owns its contract exclusively per the phase's file
//! ownership. The stripping *strategy* (filter, don't escape/encode) is the
//! same.

/// Strips every C0 control byte (`0x00..=0x1F`, which includes ESC `0x1B`
/// and BEL `0x07` — the two bytes that open every ANSI/OSC/DCS sequence and
/// terminate an OSC-BEL form), DEL (`0x7F`), and the C1 control range
/// (`0x80..=0x9F`, which includes the single-byte CSI/OSC/ST introducers
/// some terminals accept) from `s`. Everything else — including the
/// printable characters that made up the *body* of an injected sequence,
/// e.g. the `2J` in `\x1b[2J` — is preserved verbatim: a remote workspace
/// name can no longer move the cursor, clear the screen, hide text, or fire
/// an OSC 52 clipboard write, but legitimate visible text (including
/// non-ASCII) is never altered.
///
/// Idempotent: sanitizing an already-sanitized string is a no-op.
pub(crate) fn sanitize_remote_string(s: &str) -> String {
    s.chars()
        .filter(|ch| {
            let cp = *ch as u32;
            !(cp <= 0x1F || cp == 0x7F || (0x80..=0x9F).contains(&cp))
        })
        .collect()
}

/// [`sanitize_remote_string`] applied in place to an `Option<String>` field,
/// for the common case of an optional chrome string (e.g. `PaneInfo::cwd`).
pub(crate) fn sanitize_remote_string_opt(s: Option<String>) -> Option<String> {
    s.map(|s| sanitize_remote_string(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test 1 (phase TDD plan, S11.1): a crafted chrome string carrying a
    // screen-clear, a cursor move, an OSC 52 clipboard write, and a
    // conceal/hidden-text SGR is neutralized — no active control sequence
    // survives — while the visible text around it is preserved.
    #[test]
    fn strips_ansi_osc_and_cursor_control_while_preserving_visible_text() {
        let clear_screen = "before\x1b[2Jafter";
        assert_eq!(sanitize_remote_string(clear_screen), "before[2Jafter");
        assert!(!sanitize_remote_string(clear_screen).contains('\x1b'));

        let cursor_move = "name\x1b[10;20Htail";
        assert_eq!(sanitize_remote_string(cursor_move), "name[10;20Htail");

        let osc52 = "evil\x1b]52;c;ZXZpbA==\x07done";
        let sanitized = sanitize_remote_string(osc52);
        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\x07'));
        assert_eq!(sanitized, "evil]52;c;ZXZpbA==done");

        let hidden = "visible\x1b[8mhidden\x1b[0mtail";
        let sanitized_hidden = sanitize_remote_string(hidden);
        assert!(!sanitized_hidden.contains('\x1b'));
        assert_eq!(sanitized_hidden, "visible[8mhidden[0mtail");

        // A C1 single-byte control (e.g. 0x9B == CSI) is stripped too.
        let c1 = "a\u{9b}31mb";
        assert_eq!(sanitize_remote_string(c1), "a31mb");

        // Ordinary, non-control text (including non-ASCII) is untouched.
        assert_eq!(sanitize_remote_string("héllo wörld ✅"), "héllo wörld ✅");
    }

    #[test]
    fn sanitizing_an_already_sanitized_string_is_a_no_op() {
        let once = sanitize_remote_string("before\x1b[2Jafter");
        let twice = sanitize_remote_string(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn opt_variant_maps_through_none_and_some() {
        assert_eq!(sanitize_remote_string_opt(None), None);
        assert_eq!(
            sanitize_remote_string_opt(Some("x\x1b[2Jy".to_string())),
            Some("x[2Jy".to_string())
        );
    }
}
