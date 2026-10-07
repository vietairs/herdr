use super::*;

fn fresh_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("expanded sidebar");
    state
}

fn press(state: &mut ClientShellState, action: crate::input::KeybindAction) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(crate::input::KeybindMatch::Action(action), &mut outcome);
    outcome
}

#[test]
fn grow_sidebar_action_widens_by_step_and_requests_resize() {
    let mut state = fresh_state();
    let start = state.sidebar_width;

    let outcome = press(&mut state, crate::input::KeybindAction::GrowSidebar);

    assert_eq!(state.sidebar_width, start + 2);
    assert!(state.sidebar_width_manual);
    assert!(outcome.repaint);
    assert!(outcome.resize);
}

#[test]
fn shrink_sidebar_action_narrows_by_step() {
    let mut state = fresh_state();
    let start = state.sidebar_width;

    let outcome = press(&mut state, crate::input::KeybindAction::ShrinkSidebar);

    assert_eq!(state.sidebar_width, start - 2);
    assert!(state.sidebar_width_manual);
    assert!(outcome.repaint);
    assert!(outcome.resize);
}

#[test]
fn sidebar_width_keys_clamp_to_bounds() {
    let mut state = fresh_state();
    let (min, max) = state.sidebar_width_bounds();

    state.sidebar_width = max;
    let outcome = press(&mut state, crate::input::KeybindAction::GrowSidebar);
    assert_eq!(state.sidebar_width, max);
    assert!(!outcome.resize);

    state.sidebar_width = min;
    let outcome = press(&mut state, crate::input::KeybindAction::ShrinkSidebar);
    assert_eq!(state.sidebar_width, min);
    assert!(!outcome.resize);
}

#[test]
fn sidebar_width_keys_change_stored_width_without_resize_while_collapsed() {
    let mut state = fresh_state();
    state.sidebar_collapsed = true;
    let start = state.sidebar_width;

    let outcome = press(&mut state, crate::input::KeybindAction::GrowSidebar);

    assert_eq!(state.sidebar_width, start + 2);
    assert!(outcome.repaint);
    assert!(!outcome.resize);
}
