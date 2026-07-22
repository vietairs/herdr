//! Render for `Mode::MountRemoteWorkspace` — the `workspace.mount_remote`
//! collector dialog. Copies the **new linked worktree** modal shape
//! (`src/ui/dialogs.rs::render_new_linked_worktree_overlay`): shell, header,
//! single input row, inline error/status, two action buttons. Render is
//! pure — it only reads `&AppState` and draws; it never mutates state.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};

use super::widgets::{
    action_button_row_rects, centered_popup_rect, panel_contrast_fg, render_action_button,
    render_modal_header, render_modal_shell, ActionButtonSpec,
};
use crate::app::AppState;

const REMOTE_MOUNT_POPUP_WIDTH: u16 = 68;
const REMOTE_MOUNT_POPUP_HEIGHT: u16 = 11;

pub(crate) fn remote_mount_inner_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, REMOTE_MOUNT_POPUP_WIDTH, REMOTE_MOUNT_POPUP_HEIGHT).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn remote_mount_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "mount",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(super) fn render_remote_mount_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(remote_mount) = app.remote_mount.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        REMOTE_MOUNT_POPUP_WIDTH,
        REMOTE_MOUNT_POPUP_HEIGHT,
        &app.palette,
    ) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas::<6>(inner);

    render_modal_header(frame, rows[0], "mount remote workspace", &app.palette);

    frame.render_widget(
        Paragraph::new(" target").style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    frame.render_widget(Clear, input_rect);
    frame.render_widget(
        Paragraph::new(format!(" {}█", app.name_input)).style(
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0),
        ),
        input_rect,
    );

    frame.render_widget(
        Paragraph::new(" user@host, space-separated for several")
            .style(Style::default().fg(app.palette.subtext0))
            .wrap(Wrap { trim: false }),
        rows[3],
    );

    if let Some(error) = &remote_mount.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}"))
                .style(Style::default().fg(app.palette.red))
                .wrap(Wrap { trim: false }),
            rows[4],
        );
    }

    let (mount_rect, cancel_rect) = remote_mount_button_rects(inner);
    render_action_button(
        frame,
        mount_rect,
        Some("↵"),
        "mount",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_mount_button_rects_are_disjoint_and_within_inner() {
        let inner = remote_mount_inner_rect(Rect::new(0, 0, 100, 40)).unwrap();
        let (mount, cancel) = remote_mount_button_rects(inner);

        let within = |rect: Rect| {
            rect.x >= inner.x
                && rect.y >= inner.y
                && rect.x + rect.width <= inner.x + inner.width
                && rect.y + rect.height <= inner.y + inner.height
        };
        assert!(
            within(mount),
            "mount rect {mount:?} outside inner {inner:?}"
        );
        assert!(
            within(cancel),
            "cancel rect {cancel:?} outside inner {inner:?}"
        );

        let disjoint = mount.x + mount.width <= cancel.x || cancel.x + cancel.width <= mount.x;
        assert!(disjoint, "mount {mount:?} and cancel {cancel:?} overlap");
    }

    #[test]
    fn remote_mount_inner_rect_is_none_for_a_tiny_screen() {
        assert!(remote_mount_inner_rect(Rect::new(0, 0, 3, 3)).is_none());
    }
}
