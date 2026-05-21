use crate::app;
use crate::app::state::AppState;
use crate::config;
use crate::detect::AgentState;
use crate::layout::PaneId;
use crate::protocol;

pub(crate) fn should_forward_toast_to_clients(delivery: config::ToastDelivery) -> bool {
    toast_notify_kind(delivery).is_some()
}

pub(crate) fn toast_notify_kind(delivery: config::ToastDelivery) -> Option<protocol::NotifyKind> {
    match delivery {
        config::ToastDelivery::Terminal => Some(protocol::NotifyKind::Toast),
        config::ToastDelivery::System => Some(protocol::NotifyKind::SystemToast),
        config::ToastDelivery::Off | config::ToastDelivery::Herdr => None,
    }
}

pub(crate) fn toast_message_from_state_change(
    state: &AppState,
    pane_id: PaneId,
    suppress_active_tab_notifications: bool,
    prev_state: AgentState,
    new_state: AgentState,
) -> Option<String> {
    let kind = app::actions::notification_toast_for_state_change(
        suppress_active_tab_notifications,
        prev_state,
        new_state,
    )?;

    state
        .workspaces
        .iter()
        .enumerate()
        .find_map(|(ws_idx, ws)| {
            ws.tabs.iter().find_map(|tab| {
                let pane = tab.panes.get(&pane_id)?;
                let agent_label = state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .and_then(|terminal| terminal.effective_agent_label())?;
                Some(format!(
                    "{} {}: {}",
                    agent_label,
                    toast_event_text(kind),
                    app::actions::notification_context(ws, ws_idx, pane_id)
                ))
            })
        })
}

fn toast_event_text(kind: app::state::ToastKind) -> &'static str {
    match kind {
        app::state::ToastKind::NeedsAttention => "needs attention",
        app::state::ToastKind::Finished => "finished",
        app::state::ToastKind::UpdateInstalled => "updated",
    }
}
