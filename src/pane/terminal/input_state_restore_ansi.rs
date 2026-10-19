//! Pure byte builder that turns an [`InputState`] back into the DECSET/DECRST
//! sequences that reproduce it on a fresh emulator. Live-handoff seeding and
//! the federation mount share this one source of truth.

use super::InputState;
use crate::input::{MouseProtocolEncoding, MouseProtocolMode};

/// DECSET/DECRST bytes that put a fresh emulator into `state`'s input modes.
/// Pure and allocation-only; same bytes, same order, as live-handoff seeding.
pub(crate) fn input_state_restore_ansi(state: InputState) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(128);

    // No `?1049l` when on the primary screen: the target emulator is fresh.
    if state.alternate_screen {
        out.extend_from_slice(b"\x1b[?1049h");
    }

    let toggles: [(&[u8], &[u8], bool); 5] = [
        (b"\x1b[?1h", b"\x1b[?1l", state.application_cursor),
        (b"\x1b[?2004h", b"\x1b[?2004l", state.bracketed_paste),
        (b"\x1b[?1004h", b"\x1b[?1004l", state.focus_reporting),
        (b"\x1b[?1007h", b"\x1b[?1007l", state.mouse_alternate_scroll),
        (b"\x1b[?2031h", b"\x1b[?2031l", state.color_scheme_reporting),
    ];
    for (set, reset, enabled) in toggles {
        out.extend_from_slice(if enabled { set } else { reset });
    }

    out.extend_from_slice(b"\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l");
    let mouse_mode: Option<&[u8]> = match state.mouse_protocol_mode {
        MouseProtocolMode::None => None,
        MouseProtocolMode::Press => Some(b"\x1b[?9h"),
        MouseProtocolMode::PressRelease => Some(b"\x1b[?1000h"),
        MouseProtocolMode::ButtonMotion => Some(b"\x1b[?1002h"),
        MouseProtocolMode::AnyMotion => Some(b"\x1b[?1003h"),
    };
    if let Some(ansi) = mouse_mode {
        out.extend_from_slice(ansi);
    }

    out.extend_from_slice(b"\x1b[?1005l\x1b[?1006l\x1b[?1016l");
    let mouse_encoding: Option<&[u8]> = match state.mouse_protocol_encoding {
        MouseProtocolEncoding::Default => None,
        MouseProtocolEncoding::Utf8 => Some(b"\x1b[?1005h"),
        MouseProtocolEncoding::Sgr => Some(b"\x1b[?1006h"),
        MouseProtocolEncoding::SgrPixels => Some(b"\x1b[?1016h"),
    };
    if let Some(ansi) = mouse_encoding {
        out.extend_from_slice(ansi);
    }

    if state.modify_other_keys {
        out.extend_from_slice(b"\x1b[>4;2m");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::input_state_restore_ansi;
    use crate::input::{MouseProtocolEncoding, MouseProtocolMode};

    /// What Claude Code's fullscreen UI switches on at startup.
    const CLAUDE_FULLSCREEN_MODES: &[u8] =
        b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?1004h";

    const ALL_MODES: [MouseProtocolMode; 5] = [
        MouseProtocolMode::None,
        MouseProtocolMode::Press,
        MouseProtocolMode::PressRelease,
        MouseProtocolMode::ButtonMotion,
        MouseProtocolMode::AnyMotion,
    ];

    const ALL_ENCODINGS: [MouseProtocolEncoding; 4] = [
        MouseProtocolEncoding::Default,
        MouseProtocolEncoding::Utf8,
        MouseProtocolEncoding::Sgr,
        MouseProtocolEncoding::SgrPixels,
    ];

    fn fresh_terminal() -> (GhosttyPaneTerminal, mpsc::Sender<Bytes>) {
        let (tx, _rx) = mpsc::channel(4);
        let terminal = crate::ghostty::Terminal::new(80, 24, 0).unwrap();
        let pane = GhosttyPaneTerminal::new(terminal, tx.clone()).unwrap();
        (pane, tx)
    }

    fn every_bool_on() -> InputState {
        InputState {
            alternate_screen: true,
            application_cursor: true,
            bracketed_paste: true,
            focus_reporting: true,
            mouse_protocol_mode: MouseProtocolMode::AnyMotion,
            mouse_protocol_encoding: MouseProtocolEncoding::Sgr,
            mouse_alternate_scroll: true,
            modify_other_keys: true,
            color_scheme_reporting: true,
        }
    }

    #[test]
    fn restore_ansi_roundtrips_every_mouse_mode_and_encoding() {
        let mut case = 0usize;
        for mode in ALL_MODES {
            for encoding in ALL_ENCODINGS {
                // Alternate the bools per case (and per field) so every bool is
                // exercised both on and off across the matrix.
                let bit = |field: usize| (case + field).is_multiple_of(2);
                let source = InputState {
                    alternate_screen: bit(0),
                    application_cursor: bit(1),
                    bracketed_paste: bit(2),
                    focus_reporting: bit(3),
                    mouse_protocol_mode: mode,
                    mouse_protocol_encoding: encoding,
                    mouse_alternate_scroll: bit(4),
                    modify_other_keys: bit(5),
                    color_scheme_reporting: bit(6),
                };
                case += 1;

                let bytes = input_state_restore_ansi(source);
                let (pane, tx) = fresh_terminal();
                let _ = pane.process_pty_bytes(PaneId::from_raw(0), 0, &bytes, &tx);

                assert_eq!(
                    pane.input_state(),
                    Some(source),
                    "restore bytes {:?} did not reproduce {source:?}",
                    String::from_utf8_lossy(&bytes)
                );
            }
        }
        assert_eq!(case, ALL_MODES.len() * ALL_ENCODINGS.len());
    }

    #[test]
    fn restore_ansi_from_a_live_source_terminal_matches_its_input_state() {
        let (source, source_tx) = fresh_terminal();
        let _ =
            source.process_pty_bytes(PaneId::from_raw(0), 0, CLAUDE_FULLSCREEN_MODES, &source_tx);
        let source_state = source.input_state().expect("source input state");

        let bytes = input_state_restore_ansi(source_state);
        let (mirror, mirror_tx) = fresh_terminal();
        let _ = mirror.process_pty_bytes(PaneId::from_raw(0), 0, &bytes, &mirror_tx);

        assert_eq!(mirror.input_state(), Some(source_state));
        assert!(mirror.mouse_reporting_enabled());
    }

    #[test]
    fn applying_restore_ansi_through_the_byte_path_yields_no_terminal_responses() {
        let bytes = input_state_restore_ansi(every_bool_on());
        let (pane, tx) = fresh_terminal();

        let result = pane.process_pty_bytes(PaneId::from_raw(0), 0, &bytes, &tx);

        assert!(
            result.terminal_responses.is_empty(),
            "restore bytes produced terminal replies: {:?}",
            result.terminal_responses
        );
    }

    #[cfg(unix)]
    #[test]
    fn seeding_handoff_input_state_leaves_no_pending_pty_response() {
        let (pane, _tx) = fresh_terminal();

        pane.seed_handoff_input_state(every_bool_on());

        let pending = pane.drain_pending_pty_responses();
        assert!(
            pending.is_empty(),
            "handoff seeding queued PTY replies: {pending:?}"
        );
    }

    #[test]
    fn restore_ansi_follows_the_handoff_byte_order() {
        let state = InputState {
            alternate_screen: true,
            application_cursor: true,
            bracketed_paste: false,
            focus_reporting: true,
            mouse_protocol_mode: MouseProtocolMode::ButtonMotion,
            mouse_protocol_encoding: MouseProtocolEncoding::Sgr,
            mouse_alternate_scroll: false,
            modify_other_keys: true,
            color_scheme_reporting: false,
        };

        let expected: &[u8] = b"\x1b[?1049h\
\x1b[?1h\x1b[?2004l\x1b[?1004h\x1b[?1007l\x1b[?2031l\
\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1002h\
\x1b[?1005l\x1b[?1006l\x1b[?1016l\x1b[?1006h\
\x1b[>4;2m";

        let bytes = input_state_restore_ansi(state);
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            String::from_utf8_lossy(expected)
        );
    }
}
