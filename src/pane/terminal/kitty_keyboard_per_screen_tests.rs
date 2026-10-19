//! Kitty keyboard flags are per screen in libghostty: each `Screen` owns its
//! own flag stack, the alternate screen keeps its stack across exit and
//! re-entry, a first alternate entry starts disabled, RIS drops the alternate
//! screen and resets the primary one, and modifyOtherKeys is terminal-global.
//! These tests drive a real source terminal, copy its input modes the way a
//! federation mirror and a live-handoff successor do, and require the copy to
//! behave like the source on both screens.

use super::*;

fn fresh_pane() -> (GhosttyPaneTerminal, mpsc::Sender<Bytes>) {
    let (tx, _rx) = mpsc::channel(16);
    let terminal = crate::ghostty::Terminal::new(80, 24, 0).unwrap();
    let pane = GhosttyPaneTerminal::new(terminal, tx.clone()).unwrap();
    (pane, tx)
}

fn feed(pane: &(GhosttyPaneTerminal, mpsc::Sender<Bytes>), bytes: &[u8]) {
    let _ = pane
        .0
        .process_pty_bytes(PaneId::from_raw(0), 0, bytes, &pane.1);
}

fn source_from(bytes: &[u8]) -> (GhosttyPaneTerminal, mpsc::Sender<Bytes>) {
    let source = fresh_pane();
    feed(&source, bytes);
    source
}

fn kitty_flags(pane: &GhosttyPaneTerminal) -> u16 {
    match pane.keyboard_protocol() {
        Some(crate::input::KeyboardProtocol::Kitty { flags }) => flags,
        _ => 0,
    }
}

/// The bytes a federation mirror parses: `InputState` restore bytes first,
/// then the kitty keyboard replay (same composition as the pane runtime).
fn mirror_bytes(source: &GhosttyPaneTerminal) -> Vec<u8> {
    let (state, kitty) = source.input_modes_snapshot().expect("source input state");
    let mut bytes = input_state_restore_ansi(state);
    if let Some(kitty) = kitty {
        bytes.extend_from_slice(kitty.as_bytes());
    }
    bytes
}

fn mirror_of(source: &GhosttyPaneTerminal) -> (GhosttyPaneTerminal, mpsc::Sender<Bytes>) {
    let mirror = fresh_pane();
    feed(&mirror, &mirror_bytes(source));
    mirror
}

/// A live-handoff successor: input state seeded first, then the kitty replay.
fn handoff_successor_of(
    source: &GhosttyPaneTerminal,
) -> (GhosttyPaneTerminal, mpsc::Sender<Bytes>) {
    let successor = fresh_pane();
    successor
        .0
        .seed_handoff_input_state(source.input_state().expect("source input state"));
    if let Some(kitty) = source.kitty_keyboard_state_ansi() {
        successor.0.seed_keyboard_protocol_ansi(&kitty);
    }
    successor
}

/// Every way a pane's keyboard state is copied, including a second mirror
/// hop, which proves the copy's own tracker understood the replay bytes.
fn copies_of(
    source: &GhosttyPaneTerminal,
) -> Vec<(&'static str, (GhosttyPaneTerminal, mpsc::Sender<Bytes>))> {
    let mirror = mirror_of(source);
    let second_hop = mirror_of(&mirror.0);
    vec![
        ("mirror", mirror),
        ("second-hop mirror", second_hop),
        ("handoff successor", handoff_successor_of(source)),
    ]
}

#[derive(Debug, PartialEq, Eq)]
struct ScreenObservation {
    step: &'static str,
    alternate_screen: bool,
    kitty_flags: u16,
    shift_enter: Vec<u8>,
    modify_other_keys_level: u8,
}

fn observe(
    pane: &(GhosttyPaneTerminal, mpsc::Sender<Bytes>),
    step: &'static str,
) -> ScreenObservation {
    let key = crate::input::parse_terminal_key_sequence("\x1b[13;2u").unwrap();
    ScreenObservation {
        step,
        alternate_screen: pane.0.input_state().is_some_and(|s| s.alternate_screen),
        kitty_flags: kitty_flags(&pane.0),
        shift_enter: pane.0.encode_terminal_key(key),
        modify_other_keys_level: pane.0.modify_other_keys_level(),
    }
}

/// Walks both screens and both stacks one level deep. Consumes the pane's
/// state, so build every copy before walking the source.
fn walk_both_screens(pane: &(GhosttyPaneTerminal, mpsc::Sender<Bytes>)) -> Vec<ScreenObservation> {
    let start_on_alt = pane.0.input_state().is_some_and(|s| s.alternate_screen);
    let (to_other, back): (&[u8], &[u8]) = if start_on_alt {
        (b"\x1b[?1049l", b"\x1b[?1049h")
    } else {
        (b"\x1b[?1049h", b"\x1b[?1049l")
    };
    let mut walk = vec![observe(pane, "active screen")];
    feed(pane, to_other);
    walk.push(observe(pane, "other screen"));
    feed(pane, b"\x1b[<u");
    walk.push(observe(pane, "other screen after one pop"));
    feed(pane, back);
    walk.push(observe(pane, "back on the active screen"));
    feed(pane, b"\x1b[<u");
    walk.push(observe(pane, "active screen after one pop"));
    walk
}

fn assert_copies_match_source_on_both_screens(case: &str, source_bytes: &[u8]) {
    let source = source_from(source_bytes);
    let replay = String::from_utf8_lossy(&mirror_bytes(&source.0)).into_owned();
    let copies = copies_of(&source.0);
    let expected = walk_both_screens(&source);
    for (label, copy) in copies {
        assert_eq!(
            walk_both_screens(&copy),
            expected,
            "{label} diverged from the source for {case}; replay bytes {replay:?}"
        );
    }
}

#[test]
fn alt_screen_push_left_on_exit_does_not_leak_into_the_primary_screen() {
    let cases: [(&str, &[u8]); 4] = [
        ("1049", b"\x1b[?1049h\x1b[>5u\x1b[?1049l"),
        ("1047", b"\x1b[?1047h\x1b[>5u\x1b[?1047l"),
        ("47", b"\x1b[?47h\x1b[>5u\x1b[?47l"),
        (
            "combined params",
            b"\x1b[?1006;1049h\x1b[>5u\x1b[?1049;1006l",
        ),
    ];
    for (case, bytes) in cases {
        let source = source_from(bytes);
        assert!(
            !source.0.input_state().unwrap().alternate_screen,
            "{case}: source is back on the primary screen"
        );
        assert_eq!(
            kitty_flags(&source.0),
            0,
            "{case}: source primary is legacy"
        );

        for (label, copy) in copies_of(&source.0) {
            assert_eq!(
                observe(&copy, "primary"),
                observe(&source, "primary"),
                "{label} for {case}: replay {:?}",
                String::from_utf8_lossy(&mirror_bytes(&source.0))
            );
        }
    }
}

#[test]
fn alt_screen_flags_left_on_exit_are_restored_for_the_next_alt_entry() {
    // libghostty keeps the alternate screen (and its kitty stack) after the
    // app leaves it, so the next alternate entry sees the leftover flags.
    let source = source_from(b"\x1b[?1049h\x1b[>5u\x1b[?1049l");
    let copies = copies_of(&source.0);
    feed(&source, b"\x1b[?1049h");
    assert_eq!(
        kitty_flags(&source.0),
        5,
        "source alt screen kept its flags"
    );

    for (label, copy) in copies {
        feed(&copy, b"\x1b[?1049h");
        assert_eq!(
            observe(&copy, "re-entered alt"),
            observe(&source, "re-entered alt"),
            "{label}"
        );
    }
}

#[test]
fn primary_push_survives_an_alt_screen_round_trip_that_leaks_a_push() {
    assert_copies_match_source_on_both_screens(
        "primary push, leaky alt app",
        b"\x1b[>1u\x1b[?1049h\x1b[>5u\x1b[?1049l",
    );
}

#[test]
fn primary_push_survives_a_clean_alt_screen_round_trip() {
    assert_copies_match_source_on_both_screens(
        "primary push, clean alt app",
        b"\x1b[>1u\x1b[?1049h\x1b[>5u\x1b[<u\x1b[?1049l",
    );
}

#[test]
fn primary_flags_do_not_carry_into_a_fresh_alt_screen() {
    let source = source_from(b"\x1b[>1u\x1b[?1049h");
    assert_eq!(kitty_flags(&source.0), 0, "first alt entry starts legacy");
    assert_copies_match_source_on_both_screens(
        "primary push, then alt entry",
        b"\x1b[>1u\x1b[?1049h",
    );
}

#[test]
fn source_on_alt_screen_restores_flags_on_both_screens() {
    for (case, bytes) in [
        ("1049", b"\x1b[>1u\x1b[?1049h\x1b[>5u".as_slice()),
        ("1047", b"\x1b[>1u\x1b[?1047h\x1b[>5u\x1b[>7u".as_slice()),
        ("47", b"\x1b[>1u\x1b[?47h\x1b[=3u".as_slice()),
        (
            "re-entry after a leaky exit",
            b"\x1b[>1u\x1b[?1049h\x1b[>5u\x1b[?1049l\x1b[>3u\x1b[?1049h\x1b[>7u".as_slice(),
        ),
    ] {
        assert_copies_match_source_on_both_screens(case, bytes);
    }
}

#[test]
fn ris_clears_kitty_flags_on_both_screens_and_returns_to_primary() {
    for (case, bytes) in [
        ("ris on alt", b"\x1b[>1u\x1b[?1049h\x1b[>5u\x1bc".as_slice()),
        (
            "ris after a leaky alt exit",
            b"\x1b[?1049h\x1b[>5u\x1b[?1049l\x1b[>1u\x1bc".as_slice(),
        ),
        (
            "push on primary after ris on alt",
            b"\x1b[?1049h\x1b[>5u\x1bc\x1b[>1u".as_slice(),
        ),
        (
            "alt entry after ris on alt",
            b"\x1b[?1049h\x1b[>5u\x1bc\x1b[>1u\x1b[?1049h".as_slice(),
        ),
    ] {
        assert_copies_match_source_on_both_screens(case, bytes);
    }
}

#[test]
fn modify_other_keys_level_is_shared_by_both_screens() {
    assert_copies_match_source_on_both_screens(
        "modifyOtherKeys set on alt",
        b"\x1b[?1049h\x1b[>4;2m\x1b[?1049l",
    );
    assert_copies_match_source_on_both_screens(
        "modifyOtherKeys set on primary, source on alt",
        b"\x1b[>4;2m\x1b[?1049h\x1b[>5u",
    );
}

#[test]
fn screen_switches_split_across_chunks_are_tracked() {
    let sequence: &[u8] = b"\x1b[>1u\x1b[?1006;1049h\x1b[>5u\x1b[?1049l";
    for split in 0..=sequence.len() {
        let source = fresh_pane();
        feed(&source, &sequence[..split]);
        feed(&source, &sequence[split..]);
        let replay = String::from_utf8_lossy(&mirror_bytes(&source.0)).into_owned();
        let copies = copies_of(&source.0);
        let expected = walk_both_screens(&source);
        for (label, copy) in copies {
            assert_eq!(
                walk_both_screens(&copy),
                expected,
                "{label} diverged at split {split}; replay {replay:?}"
            );
        }
    }
}

/// Pushes past libghostty's stack depth, then pops one at a time and in
/// batches: every copy must report the source's flags after each pop, which
/// ties the tracker's depth to the vendored emulator.
#[test]
fn copies_match_the_source_on_every_pop_past_the_stack_depth() {
    // (case name, source bytes, pops applied to the source and every copy)
    type Case = (String, Vec<u8>, Vec<&'static [u8]>);
    let mut cases: Vec<Case> = Vec::new();
    for pushes in [6usize, 7, 8, 9, 12, 20] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[=3u");
        for f in 1..=pushes {
            bytes.extend_from_slice(format!("\x1b[>{}u", f).as_bytes());
        }
        cases.push((
            format!("primary {pushes}"),
            bytes.clone(),
            vec![b"\x1b[<u"; 10],
        ));
        cases.push((
            format!("primary {pushes} pop3"),
            bytes.clone(),
            vec![b"\x1b[<3u", b"\x1b[<3u", b"\x1b[<3u", b"\x1b[<u"],
        ));
        cases.push((
            format!("primary {pushes} pop7"),
            bytes.clone(),
            vec![b"\x1b[<7u", b"\x1b[<u", b"\x1b[<u"],
        ));
        let mut alt = b"\x1b[>1u\x1b[?1049h".to_vec();
        alt.extend_from_slice(&bytes);
        cases.push((format!("alt {pushes}"), alt, vec![b"\x1b[<u"; 10]));
    }
    for (case, bytes, pops) in cases {
        let source = source_from(&bytes);
        let copies = copies_of(&source.0);
        let mut expected = vec![kitty_flags(&source.0)];
        for pop in &pops {
            feed(&source, pop);
            expected.push(kitty_flags(&source.0));
        }
        for (label, copy) in copies {
            let mut got = vec![kitty_flags(&copy.0)];
            for pop in &pops {
                feed(&copy, pop);
                got.push(kitty_flags(&copy.0));
            }
            assert_eq!(
                got,
                expected,
                "{label} diverged for {case}; replay {:?}",
                String::from_utf8_lossy(&mirror_bytes(&source.0))
            );
        }
    }
}

#[test]
fn copies_match_the_source_after_popping_past_the_stack_depth() {
    for (pushes, pops) in [
        (20usize, 8usize),
        (20, 9),
        (12, 9),
        (9, 8),
        (8, 8),
        (8, 7),
        (30, 3),
    ] {
        for alt in [false, true] {
            let mut bytes: Vec<u8> = if alt {
                b"\x1b[>1u\x1b[?1049h".to_vec()
            } else {
                Vec::new()
            };
            bytes.extend_from_slice(b"\x1b[=3u");
            for f in 1..=pushes {
                bytes.extend_from_slice(format!("\x1b[>{}u", f).as_bytes());
            }
            for _ in 0..pops {
                bytes.extend_from_slice(b"\x1b[<u");
            }
            let case = format!("pushes {pushes} pops {pops} alt {alt}");
            let source = source_from(&bytes);
            let replay = String::from_utf8_lossy(&mirror_bytes(&source.0)).into_owned();
            let copies = copies_of(&source.0);
            let mut expected = vec![kitty_flags(&source.0)];
            for _ in 0..10 {
                feed(&source, b"\x1b[<u");
                expected.push(kitty_flags(&source.0));
            }
            for (label, copy) in copies {
                let mut got = vec![kitty_flags(&copy.0)];
                for _ in 0..10 {
                    feed(&copy, b"\x1b[<u");
                    got.push(kitty_flags(&copy.0));
                }
                assert_eq!(
                    got, expected,
                    "{label} diverged for {case}; replay {replay:?}"
                );
            }
        }
    }
}

#[test]
fn out_of_range_kitty_flags_never_reach_the_copy() {
    for (case, bytes) in [
        (
            "valid then 6 invalid",
            b"\x1b[>1u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u".as_slice(),
        ),
        (
            "valid then 7 invalid",
            b"\x1b[>1u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u".as_slice(),
        ),
        (
            "valid then 8 invalid",
            b"\x1b[>1u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u\x1b[>33u"
                .as_slice(),
        ),
        ("set invalid", b"\x1b[>1u\x1b[=64u".as_slice()),
        (
            "set modes",
            b"\x1b[=1u\x1b[=4;2u\x1b[=1;3u\x1b[>2u\x1b[=8;2u".as_slice(),
        ),
    ] {
        let source = source_from(bytes);
        let replay = String::from_utf8_lossy(&mirror_bytes(&source.0)).into_owned();
        for (label, copy) in copies_of(&source.0) {
            assert_eq!(
                kitty_flags(&copy.0),
                kitty_flags(&source.0),
                "{label} diverged for {case}; replay {replay:?}"
            );
        }
    }
}

#[test]
fn copy_stays_on_the_source_screen_when_the_tracker_loses_track() {
    // XTSAVE/XTRESTORE of 1049 switches libghostty's screen without the
    // tracker seeing a 1049 set or reset.
    let source = source_from(b"\x1b[>1u\x1b[?1049h\x1b[>5u\x1b[?1049s\x1b[?1049l\x1b[?1049r");
    let alternate = source.0.input_state().unwrap().alternate_screen;
    for (label, copy) in copies_of(&source.0) {
        assert_eq!(
            copy.0.input_state().unwrap().alternate_screen,
            alternate,
            "{label} switched screens"
        );
        assert_eq!(
            kitty_flags(&copy.0),
            kitty_flags(&source.0),
            "{label} flags"
        );
    }
}

#[test]
fn copy_keeps_the_source_flags_when_a_malformed_csi_u_misleads_the_tracker() {
    // libghostty saturates a parameter above 65535 and then ignores the
    // out-of-range flags; the tracker cannot parse it and reads 0.
    for bytes in [&b"\x1b[>1u\x1b[>70000u"[..], &b"\x1b[>1u\x1b[=70000u"[..]] {
        let source = source_from(bytes);
        assert_eq!(kitty_flags(&source.0), 1, "source flags");
        for (label, copy) in copies_of(&source.0) {
            assert_eq!(kitty_flags(&copy.0), 1, "{label} flags");
        }
    }
}

#[test]
fn csi_cut_short_by_esc_does_not_hide_the_next_screen_switch() {
    for split in [None, Some(13)] {
        let bytes: &[u8] = b"\x1b[>1u\x1b[?1049h\x1b[1\x1b[?1049l";
        let source = fresh_pane();
        match split {
            Some(at) => {
                feed(&source, &bytes[..at]);
                feed(&source, &bytes[at..]);
            }
            None => feed(&source, bytes),
        }
        for (label, copy) in copies_of(&source.0) {
            assert!(
                !copy.0.input_state().unwrap().alternate_screen,
                "{label} landed on the alternate screen (split {split:?})"
            );
            assert_eq!(kitty_flags(&copy.0), 1, "{label} flags (split {split:?})");
        }
    }
}
