/// Handoff-only record of the keyboard modes a pane's app requested, so an
/// update handoff can replay the full Kitty keyboard stack into the new server.
/// It is never consulted for live input: libghostty owns keyboard state and
/// encoding. Remove once handoff restores panes from libghostty snapshots.
///
/// libghostty keeps one Kitty flag stack per screen: push, pop and set act on
/// the active screen only, a first alternate-screen entry starts disabled, the
/// alternate screen keeps its stack across exit and re-entry (47, 1047 and
/// 1049 all reuse it), and RIS returns to the primary screen with both stacks
/// cleared. modifyOtherKeys is terminal-global, so it is tracked once.
#[derive(Debug, Clone, Default)]
pub(crate) struct KittyKeyboardTracker {
    pending: Vec<u8>,
    primary: KittyFlagStack,
    alternate: KittyFlagStack,
    alternate_active: bool,
    modify_other_keys_level: u8,
}

/// Entries libghostty keeps per screen, counting the current flags. A push
/// onto a full stack overwrites the oldest entry, so the replay never grows
/// past this depth however many pushes an app leaks.
const KITTY_FLAG_STACK_DEPTH: usize = 8;

/// Kitty keyboard flags are 5 bits; libghostty ignores a push or set above.
const KITTY_FLAGS_MAX: u16 = 0x1f;

/// One screen's Kitty keyboard flags: the current value and the pushed
/// values underneath it, at most `KITTY_FLAG_STACK_DEPTH - 1` of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct KittyFlagStack {
    stack: Vec<u16>,
    flags: u16,
}

impl KittyFlagStack {
    #[cfg(any(unix, test))]
    fn is_default(&self) -> bool {
        self.stack.is_empty() && self.flags == 0
    }

    /// Bytes that rebuild this stack on a screen that starts disabled.
    #[cfg(unix)]
    fn push_replay_ansi(&self, ansi: &mut String) {
        if self.stack.is_empty() {
            if self.flags != 0 {
                ansi.push_str(&format!("\x1b[={}u", self.flags));
            }
            return;
        }
        let baseline = self.stack[0];
        if baseline != 0 {
            ansi.push_str(&format!("\x1b[={baseline}u"));
        }
        for flags in self.stack.iter().skip(1).copied().chain([self.flags]) {
            ansi.push_str(&format!("\x1b[>{flags}u"));
        }
    }
}

impl KittyKeyboardTracker {
    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        let combined;
        let bytes = if self.pending.is_empty() {
            bytes
        } else {
            combined = self
                .pending
                .iter()
                .copied()
                .chain(bytes.iter().copied())
                .collect::<Vec<_>>();
            self.pending.clear();
            &combined
        };
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != 0x1b {
                index += 1;
                continue;
            }
            if index + 1 >= bytes.len() {
                self.store_pending(&bytes[index..]);
                break;
            }
            if bytes[index + 1] == b'c' {
                self.primary = KittyFlagStack::default();
                self.alternate = KittyFlagStack::default();
                self.alternate_active = false;
                self.modify_other_keys_level = 0;
            }
            if bytes[index + 1] != b'[' {
                index += 1;
                continue;
            }

            let mut end = index + 2;
            while end < bytes.len()
                && !(0x40..=0x7e).contains(&bytes[end])
                && !matches!(bytes[end], 0x1b | 0x18 | 0x1a)
            {
                end += 1;
            }
            if end >= bytes.len() {
                self.store_pending(&bytes[index..]);
                break;
            }
            // A VT parser drops a CSI cut short by ESC (which starts the next
            // sequence) or by CAN/SUB, so the tracker must not read past it.
            if bytes[end] == 0x1b {
                index = end;
                continue;
            }
            if matches!(bytes[end], 0x18 | 0x1a) {
                index = end + 1;
                continue;
            }

            match bytes[end] {
                b'u' => self.observe_csi_u(&bytes[index + 2..end]),
                b'm' => self.observe_modify_other_keys(&bytes[index + 2..end]),
                b'h' => self.observe_dec_private_mode(&bytes[index + 2..end], true),
                b'l' => self.observe_dec_private_mode(&bytes[index + 2..end], false),
                b'n' if bytes[index + 2..end]
                    .strip_prefix(b">")
                    .is_some_and(|params| {
                        !params.contains(&b';') && parse_kitty_keyboard_flags(params) == 4
                    }) =>
                {
                    self.modify_other_keys_level = 0;
                }
                _ => {}
            }
            index = end + 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn modify_other_keys_level(&self) -> u8 {
        self.modify_other_keys_level
    }

    fn active_stack(&mut self) -> &mut KittyFlagStack {
        if self.alternate_active {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    /// DECSET/DECRST: any of 47, 1047 or 1049 in the parameter list switches
    /// the active screen, as libghostty applies each listed mode in turn.
    fn observe_dec_private_mode(&mut self, params: &[u8], enabled: bool) {
        let Some(params) = params.strip_prefix(b"?") else {
            return;
        };
        // One pass with no allocation or string parsing: this runs for every
        // DECSET/DECRST in every pane's output.
        let mut switches_screen = false;
        let mut mode: u32 = 0;
        for &byte in params.iter().chain([b';'].iter()) {
            match byte {
                b'0'..=b'9' => {
                    mode = (mode * 10 + u32::from(byte - b'0')).min(u32::from(u16::MAX) + 1);
                }
                b';' => {
                    switches_screen |= matches!(mode, 47 | 1047 | 1049);
                    mode = 0;
                }
                _ => return,
            }
        }
        if switches_screen {
            self.alternate_active = enabled;
        }
    }

    fn observe_modify_other_keys(&mut self, params: &[u8]) {
        let Some(params) = params.strip_prefix(b">") else {
            return;
        };
        if params.is_empty() {
            self.modify_other_keys_level = 0;
            return;
        }

        let mut parts = params.split(|byte| *byte == b';');
        let resource = parts.next().unwrap_or_default();
        let value = parts.next();
        if parts.next().is_some() {
            return;
        }
        if parse_kitty_keyboard_flags(resource) == 4 {
            self.modify_other_keys_level =
                value.map(parse_kitty_keyboard_flags).unwrap_or(0).min(2) as u8;
        }
    }

    fn store_pending(&mut self, bytes: &[u8]) {
        self.pending.clear();
        if bytes.len() <= 64 {
            self.pending.extend_from_slice(bytes);
        }
    }

    fn observe_csi_u(&mut self, params: &[u8]) {
        let Some((&kind, rest)) = params.split_first() else {
            return;
        };
        let screen = self.active_stack();
        match kind {
            b'>' => {
                // libghostty drops a push whose flags do not fit in 5 bits.
                let flags = parse_kitty_keyboard_flags(rest);
                if flags > KITTY_FLAGS_MAX {
                    return;
                }
                if screen.stack.len() + 1 >= KITTY_FLAG_STACK_DEPTH {
                    screen.stack.remove(0);
                }
                screen.stack.push(screen.flags);
                screen.flags = flags;
            }
            b'=' => {
                let mut params = rest.split(|byte| *byte == b';');
                let flags = parse_kitty_keyboard_flags(params.next().unwrap_or_default());
                if flags > KITTY_FLAGS_MAX {
                    return;
                }
                match params.next().map(parse_kitty_keyboard_flags).unwrap_or(1) {
                    1 => screen.flags = flags,
                    2 => screen.flags |= flags,
                    3 => screen.flags &= !flags,
                    _ => {}
                }
            }
            b'<' => {
                // No parameter pops one entry; an explicit 0 pops none. Popping
                // the whole stack or more always ends disabled, so libghostty
                // resets in one step and so does the tracker.
                let count = if rest.is_empty() {
                    1
                } else {
                    usize::from(parse_kitty_keyboard_flags(rest))
                };
                if count >= KITTY_FLAG_STACK_DEPTH {
                    *screen = KittyFlagStack::default();
                } else {
                    for _ in 0..count {
                        screen.flags = screen.stack.pop().unwrap_or(0);
                    }
                }
            }
            _ => {}
        }
    }

    /// Returns [`Self::replay_ansi`] when the tracker agrees with libghostty:
    /// same active screen and same current flags. When they disagree (a mode
    /// the tracker does not follow, such as an XTRESTORE of 1049, or a
    /// malformed `CSI u` the tracker reads differently), the stacks cannot be
    /// trusted, so only libghostty's current flags are set, with no screen
    /// switches: the copy may lose stack depth but never lands on the wrong
    /// screen or with the wrong flags.
    #[cfg(unix)]
    pub(crate) fn replay_ansi_on_screen(
        &self,
        alternate_screen: bool,
        current_flags: u16,
    ) -> Option<String> {
        let active = if self.alternate_active {
            &self.alternate
        } else {
            &self.primary
        };
        if self.alternate_active == alternate_screen && active.flags == current_flags {
            return self.replay_ansi();
        }
        let mut ansi = String::new();
        if current_flags != 0 {
            ansi.push_str(&format!("\x1b[={current_flags}u"));
        }
        if self.modify_other_keys_level > 0 {
            ansi.push_str(&format!("\x1b[>4;{}m", self.modify_other_keys_level));
        }
        (!ansi.is_empty()).then_some(ansi)
    }

    /// Bytes that rebuild both screens' stacks and modifyOtherKeys on a
    /// target that already parsed the `InputState` restore bytes, so it is on
    /// the source's active screen with both stacks disabled and a fresh
    /// alternate screen.
    ///
    /// Source on primary: the primary stack, then, only when the alternate
    /// screen holds leftover flags, `?47h` + alternate stack + `?47l`. Mode
    /// 47 neither erases nor saves the cursor, and it copies the cursor both
    /// ways, so the round trip leaves the primary screen untouched.
    ///
    /// Source on alternate: the alternate stack alone when the primary stack
    /// is disabled; otherwise `?1049l` + primary stack + `?1049h` + alternate
    /// stack. 1049 matches the restore bytes' own `?1049h`, so the mode bits
    /// end as the restore left them; the cursor it saves and restores is the
    /// target's own, and the alternate screen it erases is still blank.
    #[cfg(unix)]
    pub(crate) fn replay_ansi(&self) -> Option<String> {
        let mut ansi = String::new();
        if self.alternate_active {
            if !self.primary.is_default() {
                ansi.push_str("\x1b[?1049l");
                self.primary.push_replay_ansi(&mut ansi);
                ansi.push_str("\x1b[?1049h");
            }
            self.alternate.push_replay_ansi(&mut ansi);
        } else {
            self.primary.push_replay_ansi(&mut ansi);
            if !self.alternate.is_default() {
                ansi.push_str("\x1b[?47h");
                self.alternate.push_replay_ansi(&mut ansi);
                ansi.push_str("\x1b[?47l");
            }
        }
        if self.modify_other_keys_level > 0 {
            ansi.push_str(&format!("\x1b[>4;{}m", self.modify_other_keys_level));
        }
        (!ansi.is_empty()).then_some(ansi)
    }
}

fn parse_kitty_keyboard_flags(bytes: &[u8]) -> u16 {
    let first_param = bytes.split(|byte| *byte == b';').next().unwrap_or_default();
    std::str::from_utf8(first_param)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_split_csi_sequences() {
        let mut tracker = KittyKeyboardTracker::default();

        tracker.observe(b"\x1b[>1u\x1b[>4;");
        tracker.observe(b"01m\x1b[>5u\x1b[<");
        tracker.observe(b"u");

        assert_eq!(tracker.primary.flags, 1);
        assert_eq!(tracker.primary.stack, vec![0]);
        assert_eq!(tracker.modify_other_keys_level(), 1);
        tracker.observe(b"\x1b[>1m");
        assert_eq!(tracker.modify_other_keys_level(), 1);
        tracker.observe(b"\x1b[>04n");
        assert_eq!(tracker.modify_other_keys_level(), 0);
    }

    #[test]
    fn push_onto_a_full_stack_drops_the_oldest_entry() {
        let mut tracker = KittyKeyboardTracker::default();
        for flags in 1..=20 {
            tracker.observe(format!("\x1b[>{flags}u").as_bytes());
        }

        assert_eq!(tracker.primary.flags, 20);
        assert_eq!(tracker.primary.stack, (13..=19).collect::<Vec<_>>());
        tracker.observe(b"\x1b[<7u");
        assert_eq!(tracker.primary.flags, 13);
        tracker.observe(b"\x1b[<u");
        assert!(tracker.primary.is_default());
        #[cfg(unix)]
        assert_eq!(tracker.replay_ansi(), None);
    }

    #[test]
    fn csi_cut_short_by_esc_can_or_sub_is_dropped() {
        for cut in [b"\x1b".as_slice(), b"\x18", b"\x1a"] {
            let mut bytes = b"\x1b[>1u\x1b[?1049h\x1b[1".to_vec();
            bytes.extend_from_slice(cut);
            if cut != b"\x1b" {
                bytes.push(0x1b);
            }
            bytes.extend_from_slice(b"[?1049l");
            for split in 0..=bytes.len() {
                let mut tracker = KittyKeyboardTracker::default();
                tracker.observe(&bytes[..split]);
                tracker.observe(&bytes[split..]);
                assert!(!tracker.alternate_active, "cut {cut:?}, split {split}");
                assert_eq!(tracker.primary.flags, 1, "cut {cut:?}, split {split}");
            }
        }
    }

    #[test]
    fn pop_counts_follow_libghostty() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[>1u\x1b[>2u\x1b[<0u");
        assert_eq!(tracker.primary.flags, 2, "an explicit 0 pops nothing");
        tracker.observe(b"\x1b[<u");
        assert_eq!(tracker.primary.flags, 1, "no parameter pops one");
        tracker.observe(b"\x1b[>2u\x1b[<65535u");
        assert!(tracker.primary.is_default(), "a huge pop resets the stack");
    }

    #[test]
    fn set_honours_the_mode_parameter_and_ignores_out_of_range_flags() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[=1u\x1b[=4;2u");
        assert_eq!(tracker.primary.flags, 5);
        tracker.observe(b"\x1b[=1;3u");
        assert_eq!(tracker.primary.flags, 4);
        tracker.observe(b"\x1b[=64u\x1b[>33u");
        assert_eq!(tracker.primary.flags, 4);
        assert!(tracker.primary.stack.is_empty());
        tracker.observe(b"\x1b[=3;1u");
        assert_eq!(tracker.primary.flags, 3);
    }

    #[test]
    fn ris_clears_kitty_flags_stack_and_modify_other_keys() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[>1u\x1b[>5u\x1b[>4;2m\x1bc");

        assert_eq!(tracker.primary.flags, 0);
        assert!(tracker.primary.stack.is_empty());
        assert_eq!(tracker.modify_other_keys_level(), 0);
        #[cfg(unix)]
        assert_eq!(tracker.replay_ansi(), None);
    }

    #[test]
    fn tracks_and_replays_exact_modify_other_keys_level() {
        let mut tracker = KittyKeyboardTracker::default();

        tracker.observe(b"\x1b[>4;1m");
        assert_eq!(tracker.modify_other_keys_level(), 1);
        #[cfg(unix)]
        assert_eq!(tracker.replay_ansi().as_deref(), Some("\x1b[>4;1m"));

        tracker.observe(b"\x1b[>4;2m");
        assert_eq!(tracker.modify_other_keys_level(), 2);
        tracker.observe(b"\x1b[>4;0m");
        assert_eq!(tracker.modify_other_keys_level(), 0);
    }

    #[test]
    fn modify_other_keys_disable_clears_state_across_chunks() {
        for level in [1, 2] {
            for sequence in [b"\x1b[>4n".as_slice(), b"\x1b[>04n".as_slice()] {
                for split in 0..=sequence.len() {
                    let mut tracker = KittyKeyboardTracker::default();
                    tracker.observe(b"\x1b[>5u");
                    tracker.observe(format!("\x1b[>4;{level}m").as_bytes());
                    tracker.observe(b"\x1b[4n\x1b[>1n\x1b[>4;2n");
                    assert_eq!(tracker.modify_other_keys_level(), level);

                    tracker.observe(&sequence[..split]);
                    tracker.observe(&sequence[split..]);

                    assert_eq!(
                        tracker.modify_other_keys_level(),
                        0,
                        "mode {level}, sequence {sequence:?}, split {split}"
                    );
                    assert_eq!(tracker.primary.flags, 5);
                    assert_eq!(tracker.primary.stack, vec![0]);
                    #[cfg(unix)]
                    assert_eq!(tracker.replay_ansi().as_deref(), Some("\x1b[>5u"));
                }
            }
        }
    }

    #[test]
    fn screen_switch_modes_select_the_active_stack() {
        for (enter, leave) in [
            (b"\x1b[?1049h".as_slice(), b"\x1b[?1049l".as_slice()),
            (b"\x1b[?1047h", b"\x1b[?1047l"),
            (b"\x1b[?47h", b"\x1b[?47l"),
            (b"\x1b[?1006;1049h", b"\x1b[?1049;1006l"),
        ] {
            let mut tracker = KittyKeyboardTracker::default();
            tracker.observe(b"\x1b[>1u");
            tracker.observe(enter);
            tracker.observe(b"\x1b[>5u");
            tracker.observe(leave);

            assert!(!tracker.alternate_active, "{enter:?}");
            assert_eq!(tracker.primary.flags, 1, "{enter:?}");
            assert_eq!(tracker.primary.stack, vec![0], "{enter:?}");
            assert_eq!(tracker.alternate.flags, 5, "{enter:?}");
            assert_eq!(tracker.alternate.stack, vec![0], "{enter:?}");
        }
    }

    #[test]
    fn non_screen_dec_private_modes_leave_the_active_screen_alone() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[?1006h\x1b[?1048h\x1b[1049h\x1b[?1049$p\x1b[>5u");

        assert!(!tracker.alternate_active);
        assert_eq!(tracker.primary.flags, 5);
        assert!(tracker.alternate.is_default());
    }

    #[test]
    fn ris_returns_to_the_primary_screen_and_clears_both_stacks() {
        let mut tracker = KittyKeyboardTracker::default();
        tracker.observe(b"\x1b[>1u\x1b[?1049h\x1b[>5u\x1bc");

        assert!(!tracker.alternate_active);
        assert!(tracker.primary.is_default());
        assert!(tracker.alternate.is_default());
    }

    #[cfg(unix)]
    #[test]
    fn replay_rebuilds_the_inactive_screen_without_leaving_the_active_one() {
        let replay = |bytes: &[u8]| {
            let mut tracker = KittyKeyboardTracker::default();
            tracker.observe(bytes);
            tracker.replay_ansi()
        };

        assert_eq!(replay(b"\x1b[?1049h\x1b[?1049l"), None);
        assert_eq!(
            replay(b"\x1b[>1u\x1b[?1049h\x1b[>5u\x1b[?1049l").as_deref(),
            Some("\x1b[>1u\x1b[?47h\x1b[>5u\x1b[?47l")
        );
        assert_eq!(replay(b"\x1b[?1049h\x1b[>5u").as_deref(), Some("\x1b[>5u"));
        assert_eq!(
            replay(b"\x1b[>1u\x1b[?1049h\x1b[=3u\x1b[>4;2m").as_deref(),
            Some("\x1b[?1049l\x1b[>1u\x1b[?1049h\x1b[=3u\x1b[>4;2m")
        );
    }
}
