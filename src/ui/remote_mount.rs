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
/// Recents rows are capped at `AppState::recent_remote_mount_targets`'s own
/// cap, so the dialog never grows past this regardless of how many targets
/// are stored — and raising the state cap raises the row budget with it
/// instead of leaving navigable-but-invisible rows.
const REMOTE_MOUNT_RECENTS_MAX_ROWS: usize = crate::app::state::RECENT_REMOTE_MOUNT_TARGETS_CAP;

fn remote_mount_recents_rows(recents_count: usize) -> usize {
    recents_count.min(REMOTE_MOUNT_RECENTS_MAX_ROWS)
}

/// Recents rows that actually fit inside `list_rect`, which is what both
/// render and hit-test must use. `centered_popup_rect` clamps the popup to
/// the terminal, so on a short screen ratatui shrinks this row below the
/// requested `Length` — deriving the row count from state instead would
/// paint recents over the button row and the popup border, and make the
/// recents hit-test (consulted before the button rects) swallow the click
/// that should press "mount".
///
/// Row 0 of `list_rect` is the "recent" heading, not a selectable item.
fn remote_mount_visible_recents_rows(list_rect: Rect, recents_count: usize) -> usize {
    (list_rect.height.saturating_sub(1) as usize).min(remote_mount_recents_rows(recents_count))
}

/// Total popup height, growing by one heading row plus one row per recent
/// target — 0 extra when there are no recents, so the dialog stays exactly
/// its original compact size until a first successful mount adds history.
fn remote_mount_popup_height(recents_count: usize) -> u16 {
    let recents_rows = remote_mount_recents_rows(recents_count);
    let extra = if recents_rows > 0 {
        1 + recents_rows as u16
    } else {
        0
    };
    REMOTE_MOUNT_POPUP_HEIGHT + extra
}

/// Shared row layout for both rendering and mouse hit-testing, so the two
/// can never disagree about where the recents rows land.
fn remote_mount_rows(inner: Rect, recents_count: usize) -> [Rect; 7] {
    let recents_rows = remote_mount_recents_rows(recents_count);
    let recents_extra = if recents_rows > 0 {
        1 + recents_rows as u16
    } else {
        0
    };
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(recents_extra),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas::<7>(inner)
}

pub(crate) fn remote_mount_inner_rect(area: Rect, recents_count: usize) -> Option<Rect> {
    centered_popup_rect(
        area,
        REMOTE_MOUNT_POPUP_WIDTH,
        remote_mount_popup_height(recents_count),
    )
    .map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

/// Row index (into `AppState::recent_remote_mount_targets`) at `(col, row)`,
/// or `None` outside the list or on its heading row. Mirrors
/// `global_menu_item_at`'s bounds-check-then-row-relative-index shape
/// (`src/app/input/modal.rs`).
pub(crate) fn remote_mount_recent_at(
    inner: Rect,
    recents_count: usize,
    col: u16,
    row: u16,
) -> Option<usize> {
    let list_rect = remote_mount_rows(inner, recents_count)[4];
    let visible_rows = remote_mount_visible_recents_rows(list_rect, recents_count);
    if visible_rows == 0 {
        return None;
    }
    if col < list_rect.x || col >= list_rect.x + list_rect.width {
        return None;
    }
    // Row 0 of `list_rect` is the "recent" heading, not a selectable item.
    let idx = row.checked_sub(list_rect.y + 1)? as usize;
    (idx < visible_rows).then_some(idx)
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

    let recents_count = app.recent_remote_mount_targets.len();
    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        REMOTE_MOUNT_POPUP_WIDTH,
        remote_mount_popup_height(recents_count),
        &app.palette,
    ) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    let rows = remote_mount_rows(inner, recents_count);

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

    let list_rect = rows[4];
    if remote_mount_recents_rows(recents_count) > 0 && list_rect.height > 0 {
        frame.render_widget(
            Paragraph::new(" recent").style(Style::default().fg(app.palette.overlay0)),
            Rect::new(list_rect.x, list_rect.y, list_rect.width, 1),
        );
        // Row budget comes from the (possibly clamped) rect, not from the
        // stored recents count — see `remote_mount_visible_recents_rows`.
        let visible_rows = remote_mount_visible_recents_rows(list_rect, recents_count);
        for (idx, target) in app
            .recent_remote_mount_targets
            .iter()
            .take(visible_rows)
            .enumerate()
        {
            let row_rect = Rect::new(
                list_rect.x,
                list_rect.y + 1 + idx as u16,
                list_rect.width,
                1,
            );
            let highlighted = remote_mount.recents_highlighted == Some(idx);
            let style = if highlighted {
                Style::default()
                    .fg(panel_contrast_fg(&app.palette))
                    .bg(app.palette.accent)
            } else {
                Style::default().fg(app.palette.subtext0)
            };
            frame.render_widget(Paragraph::new(format!(" {target}")).style(style), row_rect);
        }
    }

    // A failed target's error must stay visible even while sibling targets
    // are still resolving (2+ target submission) — render it whenever it is
    // set, instead of an `else if` that lets `submitting` shadow it away
    // until the last target resolves.
    if let Some(error) = &remote_mount.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}"))
                .style(Style::default().fg(app.palette.red))
                .wrap(Wrap { trim: false }),
            rows[5],
        );
    } else if remote_mount.submitting {
        frame.render_widget(
            Paragraph::new(" mounting…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
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
        let inner = remote_mount_inner_rect(Rect::new(0, 0, 100, 40), 0).unwrap();
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
        assert!(remote_mount_inner_rect(Rect::new(0, 0, 3, 3), 0).is_none());
    }

    #[test]
    fn remote_mount_popup_grows_only_when_recents_are_present() {
        let no_recents = remote_mount_inner_rect(Rect::new(0, 0, 100, 40), 0).unwrap();
        let with_recents = remote_mount_inner_rect(Rect::new(0, 0, 100, 40), 3).unwrap();
        assert!(with_recents.height > no_recents.height);
        // Capped at REMOTE_MOUNT_RECENTS_MAX_ROWS (5): more than 5 stored
        // recents must not keep growing the popup further.
        let with_many_recents = remote_mount_inner_rect(Rect::new(0, 0, 100, 40), 9).unwrap();
        assert_eq!(with_recents.height + 2, with_many_recents.height);
    }

    #[test]
    fn remote_mount_recent_at_finds_rows_and_skips_the_heading() {
        let inner = remote_mount_inner_rect(Rect::new(0, 0, 100, 40), 2).unwrap();
        let list_rect = remote_mount_rows(inner, 2)[4];

        assert_eq!(
            remote_mount_recent_at(inner, 2, list_rect.x, list_rect.y),
            None,
            "heading row is not selectable"
        );
        assert_eq!(
            remote_mount_recent_at(inner, 2, list_rect.x, list_rect.y + 1),
            Some(0)
        );
        assert_eq!(
            remote_mount_recent_at(inner, 2, list_rect.x, list_rect.y + 2),
            Some(1)
        );
        assert_eq!(
            remote_mount_recent_at(inner, 2, list_rect.x, list_rect.y + 3),
            None,
            "third row is out of range for only 2 recents"
        );
    }

    #[test]
    fn recents_never_overlap_the_buttons_at_a_clamped_popup_height() {
        // `centered_popup_rect` clamps the popup to the terminal, so a short
        // screen shrinks the recents rect below its requested `Length`. Both
        // render and hit-test must follow the rect: otherwise a recents row
        // is painted onto (and steals the click from) the mount button, and
        // the dialog's primary action becomes unreachable by mouse.
        for screen_rows in [12u16, 13, 14, 16, 40] {
            let recents_count = 5;
            let inner = remote_mount_inner_rect(Rect::new(0, 0, 100, screen_rows), recents_count)
                .unwrap_or_else(|| panic!("popup must fit at {screen_rows} rows"));
            let list_rect = remote_mount_rows(inner, recents_count)[4];
            let visible = remote_mount_visible_recents_rows(list_rect, recents_count);

            // The last drawn row (heading at offset 0, items from offset 1)
            // stays inside the list rect.
            if visible > 0 {
                assert!(
                    list_rect.y + (visible as u16) < list_rect.y + list_rect.height,
                    "{screen_rows} rows: {visible} recents + heading overflow {list_rect:?}"
                );
            }

            let (mount, cancel) = remote_mount_button_rects(inner);
            for (name, button) in [("mount", mount), ("cancel", cancel)] {
                assert_eq!(
                    remote_mount_recent_at(inner, recents_count, button.x, button.y),
                    None,
                    "{screen_rows} rows: the {name} button row must not hit-test as a recent"
                );
            }

            // The last visible row is still selectable, so clamping did not
            // disable the list outright when it does have space.
            if visible > 0 {
                assert_eq!(
                    remote_mount_recent_at(
                        inner,
                        recents_count,
                        list_rect.x,
                        list_rect.y + visible as u16
                    ),
                    Some(visible - 1),
                    "{screen_rows} rows: last visible recents row must stay clickable"
                );
            }
        }
    }

    #[test]
    fn remote_mount_recent_at_is_none_with_no_recents() {
        let inner = remote_mount_inner_rect(Rect::new(0, 0, 100, 40), 0).unwrap();
        assert_eq!(remote_mount_recent_at(inner, 0, inner.x, inner.y), None);
    }
}
