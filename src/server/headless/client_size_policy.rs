//! Per-tab PTY size arbitration between the clients that view a tab.
//!
//! One terminal has exactly one PTY size, while every attached client lays the
//! tab out at its own surface size. `[server] client_size_policy` decides which
//! size wins:
//!
//! - `latest`: the tab's geometry controller (the client that last claimed the
//!   tab) sizes it; other viewers are cropped or padded. A federation mount's
//!   size is exclusive for the terminals it drives.
//! - `smallest`: the PTY area is the componentwise minimum of every viewer's
//!   surface, so nothing is cropped and larger viewers see blank padding. A
//!   mount's size is one more vote per terminal while host viewers are on the
//!   tab, and applies unclamped once they leave. Every size given to a voted
//!   terminal is published back to the mount (`PtySizeNotice`), so the mount
//!   sizes its mirror to the PTY instead of to its larger pane. Only a mount
//!   that agreed `Capability::PTY_SIZE_NOTICE` votes; a legacy mount ignores
//!   the notice, so its size stays exclusive, as under `latest`.
//!
//! Everything here runs on events (connect, resize, navigation, layout and
//! mount changes), never per frame. The PTY's own size is the per-tab cache and
//! `TerminalRuntime::resize` deduplicates, so an unchanged minimum costs one
//! layout and no `SIGWINCH`.

use super::*;
use crate::config::ClientSizePolicy;
use crate::server::federation_actor::publish_mounted_pty_size;

/// Viewer surfaces narrower than this do not vote under `smallest`. They are
/// zero-size or minimized windows, and letting them vote would collapse the
/// tab for every other viewer.
pub(super) const MIN_SIZE_VOTE_COLS: u16 = 10;
/// Viewer surfaces shorter than this do not vote under `smallest`.
pub(super) const MIN_SIZE_VOTE_ROWS: u16 = 3;

/// The componentwise minimum of the `(cols, rows)` votes that meet the floor,
/// or `None` when no vote is valid.
pub(super) fn smallest_valid_size(
    votes: impl IntoIterator<Item = (u16, u16)>,
) -> Option<(u16, u16)> {
    votes
        .into_iter()
        .filter(|&(cols, rows)| cols >= MIN_SIZE_VOTE_COLS && rows >= MIN_SIZE_VOTE_ROWS)
        .reduce(|(min_cols, min_rows), (cols, rows)| (min_cols.min(cols), min_rows.min(rows)))
}

/// How long a `smallest` tab waits before it grows. Shrinking applies at once,
/// so no viewer is ever cropped; growing waits, so a smaller viewer that
/// reconnects or flips back to the tab within this window costs the programs
/// in it no reflow at all.
pub(super) const SIZE_GROWTH_DEBOUNCE: Duration = Duration::from_millis(150);

/// A `smallest` tab's applied size and any growth it is waiting to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TabSizeGrowth {
    /// `(cols, rows)` the tab is laid out in now.
    pub(super) applied: (u16, u16),
    /// When the deferred growth applies; `None` when the tab is not growing.
    pub(super) due: Option<Instant>,
}

/// Debounces growth of a tab's arbitrated size from `previous` to `candidate`.
///
/// A dimension that shrinks applies at once. Any growth waits until
/// `SIZE_GROWTH_DEBOUNCE` after the first call that saw it, then applies in
/// full; the window is not extended by later growth, so a burst of resizes
/// lands at most that late. A tab with no `previous` size takes `candidate`.
pub(super) fn debounce_tab_growth(
    previous: Option<TabSizeGrowth>,
    candidate: (u16, u16),
    now: Instant,
) -> TabSizeGrowth {
    let settled = TabSizeGrowth {
        applied: candidate,
        due: None,
    };
    let Some(previous) = previous else {
        return settled;
    };
    let (applied_cols, applied_rows) = previous.applied;
    if candidate.0 <= applied_cols && candidate.1 <= applied_rows {
        return settled;
    }
    let due = previous
        .due
        .unwrap_or_else(|| now.checked_add(SIZE_GROWTH_DEBOUNCE).unwrap_or(now));
    if now >= due {
        return settled;
    }
    TabSizeGrowth {
        applied: (candidate.0.min(applied_cols), candidate.1.min(applied_rows)),
        due: Some(due),
    }
}

/// Per-tab growth debounce state for the `smallest` policy, keyed by stable tab
/// id. Only viewed tabs keep an entry: a tab nobody views is dropped, so a
/// viewer returning to it later starts from its own size instead of a stale one.
#[derive(Debug, Default)]
pub(super) struct TabSizeGrowthTracker {
    tabs: HashMap<String, TabSizeGrowth>,
    /// Test clock; production reads `Instant::now()`.
    #[cfg(test)]
    pub(super) test_now: Option<Instant>,
}

impl TabSizeGrowthTracker {
    pub(super) fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.test_now {
            return now;
        }
        Instant::now()
    }

    fn debounce(&mut self, tab_id: String, candidate: (u16, u16)) -> (u16, u16) {
        let now = self.now();
        let growth = debounce_tab_growth(self.tabs.get(&tab_id).copied(), candidate, now);
        self.tabs.insert(tab_id, growth);
        growth.applied
    }

    fn forget(&mut self, tab_id: &str) {
        self.tabs.remove(tab_id);
    }

    fn retain_tabs(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.tabs.retain(|tab_id, _| keep(tab_id));
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        self.tabs.values().filter_map(|growth| growth.due).min()
    }

    /// Drops every entry whose growth was due by `now`. Called after the due
    /// re-sizing pass, so a tab that pass did not reach cannot keep a past
    /// deadline and spin the server loop.
    fn forget_due(&mut self, now: Instant) {
        self.tabs
            .retain(|_, growth| growth.due.is_none_or(|due| due > now));
    }

    fn clear(&mut self) {
        self.tabs.clear();
    }
}

/// The area a tab's panes are laid out in to size their PTYs, and the cell
/// size used for the PTYs' pixel dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TabSizeArea {
    pub(super) area: Rect,
    pub(super) cell_size: crate::kitty_graphics::HostCellSize,
}

impl HeadlessServer {
    pub(super) fn client_size_policy(&self) -> ClientSizePolicy {
        self.app.state.client_size_policy
    }

    /// Shell clients whose active surface currently shows `tab_id`, in id
    /// order. The same set `reapply_controlled_shell_tab_geometry` groups by.
    pub(super) fn shell_tab_viewers(&self, tab_id: &str) -> Vec<u64> {
        let mut viewers = self
            .clients
            .iter()
            .filter(|(_, client)| client.is_active_shell_client() && client.writer.is_some())
            .filter(|(&client_id, _)| {
                self.shell_tab_id_for_client(client_id).as_deref() == Some(tab_id)
            })
            .map(|(&client_id, _)| client_id)
            .collect::<Vec<_>>();
        viewers.sort_unstable();
        viewers
    }

    /// The single resolver every tab-sizing path goes through.
    ///
    /// `controller_id` is the client sizing the tab: its cell size always sets
    /// the PTYs' pixel dimensions, and under `latest` its surface is the area.
    /// Under `smallest` the area is the minimum over the tab's viewers, falling
    /// back to the controller's surface when no viewer has a valid size, and
    /// its growth is debounced (`debounce_tab_growth`). A tab nobody views is
    /// sized from its controller and keeps no debounce state, so a viewer that
    /// later arrives starts from its own size.
    pub(super) fn tab_size_area(
        &mut self,
        controller_id: u64,
        target: crate::ui::TabSurfaceTarget,
    ) -> Option<TabSizeArea> {
        let controller = self.clients.get(&controller_id)?;
        let cell_size = if controller.cell_size.is_known() {
            controller.cell_size
        } else {
            crate::kitty_graphics::HostCellSize::default()
        };
        let controller_size = controller.terminal_size;
        let (cols, rows) = match self.client_size_policy() {
            ClientSizePolicy::Latest => controller_size,
            ClientSizePolicy::Smallest => match self.tab_id_for_target(target) {
                Some(tab_id) => {
                    let viewers = self.shell_tab_viewers(&tab_id);
                    if viewers.is_empty() {
                        controller_size
                    } else {
                        let candidate = smallest_valid_size(
                            viewers
                                .into_iter()
                                .filter_map(|viewer| self.clients.get(&viewer))
                                .map(|viewer| viewer.terminal_size),
                        )
                        .unwrap_or(controller_size);
                        self.tab_size_growth.debounce(tab_id, candidate)
                    }
                }
                None => controller_size,
            },
        };
        Some(TabSizeArea {
            area: Rect::new(0, 0, cols, rows),
            cell_size,
        })
    }

    /// A shell client's surface size changed. Under `latest` only the tab's
    /// controller resizes it; under `smallest` the client's tab is
    /// re-arbitrated whoever controls it, so a smaller viewer shrinks the tab
    /// without having to claim it first.
    pub(super) fn resize_shell_tab_for_client_size(
        &mut self,
        client_id: u64,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if self.client_size_policy() == ClientSizePolicy::Latest {
            return self.resize_shell_tab_if_controller(client_id, start_pending_agent_resumes);
        }
        if !self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.shell_surface_active)
        {
            return false;
        }
        let Some(target) = self.shell_target_for_client(client_id) else {
            return false;
        };
        let Some(tab_id) = self.tab_id_for_target(target) else {
            return false;
        };
        let controller_id = self
            .tab_geometry_controllers
            .get(&tab_id)
            .copied()
            .filter(|controller_id| self.clients.contains_key(controller_id))
            .unwrap_or(client_id);
        self.apply_shell_tab_geometry_to_target(controller_id, target, start_pending_agent_resumes)
    }

    /// A shell client resized its own surface. When it is the only viewer of
    /// its tab there is no other viewer to spare a reflow, so the tab takes
    /// the new size at once instead of after `SIZE_GROWTH_DEBOUNCE`. Growth
    /// caused by another viewer leaving is still debounced, so a viewer that
    /// reconnects costs no reflow.
    pub(super) fn grow_sole_viewer_tab_at_once(&mut self, client_id: u64) {
        if self.client_size_policy() != ClientSizePolicy::Smallest {
            return;
        }
        let Some(tab_id) = self.shell_tab_id_for_client(client_id) else {
            return;
        };
        if self.shell_tab_viewers(&tab_id) == [client_id] {
            self.tab_size_growth.forget(&tab_id);
        }
    }

    /// Drops the growth state of every tab `keep` rejects (see
    /// `TabSizeGrowthTracker`).
    pub(super) fn retain_tab_size_growth(&mut self, keep: impl FnMut(&str) -> bool) {
        self.tab_size_growth.retain_tabs(keep);
    }

    /// When the next deferred tab growth is due, for the server loop's timer.
    pub(super) fn next_tab_size_growth_due(&self) -> Option<Instant> {
        self.tab_size_growth.next_due()
    }

    /// Applies every tab growth that is due. Returns whether a tab was
    /// re-sized, which needs a render.
    pub(super) fn apply_due_tab_size_growth(&mut self) -> bool {
        let now = self.tab_size_growth.now();
        if self.tab_size_growth.next_due().is_none_or(|due| now < due) {
            return false;
        }
        let resized = self.reapply_controlled_shell_tab_geometry(true);
        self.tab_size_growth.forget_due(now);
        resized
    }

    /// Under `smallest`, re-arbitrates every viewed tab after a client joined,
    /// left, or moved between tabs. Under `latest` the controller keeps its
    /// size until the next claim, so this does nothing.
    pub(super) fn rearbitrate_shell_tabs_after_viewer_change(
        &mut self,
        start_pending_agent_resumes: bool,
    ) -> bool {
        self.client_size_policy() == ClientSizePolicy::Smallest
            && self.reapply_controlled_shell_tab_geometry(start_pending_agent_resumes)
    }

    fn has_sized_mount_votes(&self) -> bool {
        self.app
            .state
            .federation_owned_terminal_sizes
            .values()
            .any(Option::is_some)
    }

    /// Under `smallest`, sizes every mount-voted terminal laid out in `target`
    /// to the smaller of its pane here and the mount's vote.
    ///
    /// The generic tab resize skips these terminals (`host_owns_terminal_size`),
    /// so this is the only host path that sizes them while host viewers are on
    /// the tab. A vote of `None` stays mount-exclusive and is left alone. A tab
    /// nobody views is left to `settle_unviewed_mount_size_votes`, which gives
    /// the vote back unclamped; clamping it here first would resize the
    /// terminal twice in one pass.
    ///
    /// Every path that sizes a voted terminal passes no cell pixel size: a
    /// mount's vote carries none, and one shared value keeps
    /// `PaneRuntime::resize` (which compares pixels too) from treating the
    /// same grid as a new size.
    pub(super) fn clamp_mount_size_votes_in_tab(
        &self,
        target: crate::ui::TabSurfaceTarget,
        size: TabSizeArea,
    ) {
        if self.client_size_policy() != ClientSizePolicy::Smallest || !self.has_sized_mount_votes()
        {
            return;
        }
        if self
            .tab_id_for_target(target)
            .is_none_or(|tab_id| self.shell_tab_viewers(&tab_id).is_empty())
        {
            return;
        }
        let Some(tab) = self
            .app
            .state
            .workspaces
            .get(target.workspace_index)
            .and_then(|workspace| workspace.tabs.get(target.tab_index))
        else {
            return;
        };
        let layout = crate::ui::compute_tab_surface_for(
            &self.app.state,
            &self.app.terminal_runtimes,
            Some(target),
            size.area,
            false,
            size.cell_size,
        );
        for pane in &layout.pane_infos {
            let Some(terminal_id) = tab.terminal_id(pane.id) else {
                continue;
            };
            let Some(Some((vote_cols, vote_rows))) = self
                .app
                .state
                .federation_owned_terminal_sizes
                .get(terminal_id)
                .copied()
            else {
                continue;
            };
            let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                target.workspace_index,
                pane.id,
            ) else {
                continue;
            };
            runtime.resize(
                pane.inner_rect.height.min(vote_rows),
                pane.inner_rect.width.min(vote_cols),
                0,
                0,
            );
            publish_mounted_pty_size(&self.app, terminal_id);
        }
    }

    /// Whether any host shell viewer currently shows the pane attached to
    /// `terminal_id`, returning that pane's location either way.
    fn host_shell_view_of_terminal(
        &self,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Option<(usize, crate::layout::PaneId, bool)> {
        let (workspace_index, pane_id) = self.app.state.workspaces.iter().enumerate().find_map(
            |(workspace_index, workspace)| {
                workspace.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|(_, pane)| pane.attached_terminal_id == *terminal_id)
                        .map(|(&pane_id, _)| (workspace_index, pane_id))
                })
            },
        )?;
        let viewed = self.clients.iter().any(|(&client_id, client)| {
            client.is_active_shell_client()
                && client.writer.is_some()
                && self.shell_client_views_pane(client_id, workspace_index, pane_id)
        });
        Some((workspace_index, pane_id, viewed))
    }

    /// Under `smallest`, hands every mount-voted terminal that no host viewer
    /// shows its own vote size back. Runs at the end of each re-sizing pass, so
    /// a terminal clamped for a host viewer grows back to the mount's size once
    /// that viewer leaves the tab, detaches, or zooms another pane.
    pub(super) fn settle_unviewed_mount_size_votes(&self) {
        if self.client_size_policy() != ClientSizePolicy::Smallest || !self.has_sized_mount_votes()
        {
            return;
        }
        for (terminal_id, vote) in &self.app.state.federation_owned_terminal_sizes {
            let Some((cols, rows)) = *vote else {
                continue;
            };
            let Some((workspace_index, pane_id, viewed)) =
                self.host_shell_view_of_terminal(terminal_id)
            else {
                continue;
            };
            if viewed {
                continue;
            }
            if let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                workspace_index,
                pane_id,
            ) {
                runtime.resize(rows, cols, 0, 0);
            }
            publish_mounted_pty_size(&self.app, terminal_id);
        }
    }

    /// Applies a change in the mount's size votes reported by federation
    /// dispatch (only under `smallest`). Returns whether a host-viewed tab was
    /// re-sized, which needs a render.
    pub(super) fn apply_mount_size_vote_change(
        &mut self,
        change: crate::server::federation_actor::MountSizeVoteChange,
    ) -> bool {
        use crate::server::federation_actor::MountSizeVoteChange;

        match change {
            MountSizeVoteChange::Released => {
                // The host owns those terminals again: size them from its viewers.
                self.reapply_controlled_shell_tab_geometry(true)
            }
            MountSizeVoteChange::Resized(terminal_id) => {
                let viewed_tab = self
                    .shell_tab_target_for_terminal(terminal_id.as_str())
                    .and_then(|target| {
                        let tab_id = self.tab_id_for_target(target)?;
                        let viewers = self.shell_tab_viewers(&tab_id);
                        let controller_id = self
                            .tab_geometry_controllers
                            .get(&tab_id)
                            .copied()
                            .filter(|controller_id| viewers.contains(controller_id))
                            .or_else(|| viewers.first().copied())?;
                        Some((controller_id, target))
                    });
                // With host viewers the tab pass clamps the vote (and its
                // settle pass covers a pane hidden by zoom); without them the
                // vote applies unclamped, as it would under `latest`.
                let rearbitrated = viewed_tab.is_some_and(|(controller_id, target)| {
                    self.apply_shell_tab_geometry_to_target(controller_id, target, true)
                });
                let vote = self
                    .app
                    .state
                    .federation_owned_terminal_sizes
                    .get(&terminal_id)
                    .copied()
                    .flatten();
                let Some(runtime) = self
                    .app
                    .terminal_runtime_for_terminal_id(terminal_id.as_str())
                else {
                    return rearbitrated;
                };
                if let (false, Some((cols, rows))) = (rearbitrated, vote) {
                    runtime.resize(rows, cols, 0, 0);
                }
                publish_mounted_pty_size(&self.app, &terminal_id);
                // Unconditional, including when the resize deduped: the mount's
                // mirror was freshly reflowed and needs a repaint, and an
                // unchanged size sends no `SIGWINCH`.
                crate::server::federation_actor::nudge_mounted_child_redraw(runtime);
                rearbitrated
            }
        }
    }

    /// A config reload switched the policy: re-size every controlled tab under
    /// the new rule. Switching to `latest` makes mount votes exclusive again,
    /// so each voted terminal first gets the mount's size back.
    pub(super) fn apply_reloaded_client_size_policy(&mut self) {
        if self.client_size_policy() == ClientSizePolicy::Latest {
            for (terminal_id, vote) in &self.app.state.federation_owned_terminal_sizes {
                let Some((cols, rows)) = *vote else {
                    continue;
                };
                if let Some(runtime) = self
                    .app
                    .terminal_runtime_for_terminal_id(terminal_id.as_str())
                {
                    runtime.resize(rows, cols, 0, 0);
                }
                // The mount's mirror was bounded by the host PTY; it is the
                // mount's own size again.
                publish_mounted_pty_size(&self.app, terminal_id);
            }
        }
        // Growth debounce state belongs to `smallest` only.
        self.tab_size_growth.clear();
        info!(policy = ?self.client_size_policy(), "client size policy changed");
        self.reapply_controlled_shell_tab_geometry(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smallest_valid_size_is_the_componentwise_minimum() {
        assert_eq!(
            smallest_valid_size([(100, 35), (68, 40), (120, 17)]),
            Some((68, 17))
        );
        assert_eq!(smallest_valid_size([(80, 24)]), Some((80, 24)));
    }

    #[test]
    fn smallest_valid_size_ignores_zero_and_below_floor_votes() {
        assert_eq!(
            smallest_valid_size([(0, 0), (100, 35), (9, 40), (80, 2), (68, 17)]),
            Some((68, 17))
        );
        assert_eq!(
            smallest_valid_size([(MIN_SIZE_VOTE_COLS, MIN_SIZE_VOTE_ROWS), (100, 35)]),
            Some((MIN_SIZE_VOTE_COLS, MIN_SIZE_VOTE_ROWS))
        );
        assert_eq!(smallest_valid_size([(0, 0), (9, 50), (50, 2)]), None);
        assert_eq!(smallest_valid_size(std::iter::empty()), None);
    }

    #[test]
    fn debounce_tab_growth_shrinks_at_once_and_defers_growth() {
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);
        let first = debounce_tab_growth(None, (100, 35), at(0));
        assert_eq!(
            first,
            TabSizeGrowth {
                applied: (100, 35),
                due: None
            }
        );

        let shrunk = debounce_tab_growth(Some(first), (68, 17), at(0));
        assert_eq!(
            shrunk,
            TabSizeGrowth {
                applied: (68, 17),
                due: None
            }
        );

        let deadline = at(0) + SIZE_GROWTH_DEBOUNCE;
        let waiting = debounce_tab_growth(Some(shrunk), (100, 35), at(0));
        assert_eq!(
            waiting,
            TabSizeGrowth {
                applied: (68, 17),
                due: Some(deadline)
            }
        );

        // Later growth keeps the first deadline.
        let still = debounce_tab_growth(Some(waiting), (120, 40), at(100));
        assert_eq!(
            still,
            TabSizeGrowth {
                applied: (68, 17),
                due: Some(deadline)
            }
        );

        let grown = debounce_tab_growth(Some(still), (120, 40), deadline);
        assert_eq!(
            grown,
            TabSizeGrowth {
                applied: (120, 40),
                due: None
            }
        );
    }

    #[test]
    fn debounce_tab_growth_applies_a_shrink_while_deferring_the_other_dimension() {
        let now = Instant::now();
        let applied = TabSizeGrowth {
            applied: (100, 35),
            due: None,
        };
        let mixed = debounce_tab_growth(Some(applied), (80, 40), now);
        assert_eq!(
            mixed,
            TabSizeGrowth {
                applied: (80, 35),
                due: Some(now + SIZE_GROWTH_DEBOUNCE)
            }
        );
        // Returning to a size that no longer grows cancels the deferral.
        let back = debounce_tab_growth(Some(mixed), (80, 30), now);
        assert_eq!(
            back,
            TabSizeGrowth {
                applied: (80, 30),
                due: None
            }
        );
    }

    /// Deterministic xorshift, so the sweep below is reproducible without a
    /// property-testing dependency.
    struct Sweep(u64);

    impl Sweep {
        fn next(&mut self, below: u16) -> u16 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            u16::try_from(self.0 % u64::from(below)).unwrap_or(0)
        }
    }

    /// Five panes: a horizontal split whose halves are split vertically, one
    /// of them twice, with every split ratio drawn from the sweep.
    fn nested_layout_state(sweep: &mut Sweep) -> crate::app::AppState {
        use ratatui::layout::Direction;
        let mut workspace = crate::workspace::Workspace::test_new("size-policy-layout");
        let root = workspace.tabs[0].root_pane;
        let right = workspace.test_split(Direction::Horizontal);
        workspace.tabs[0].layout.focus_pane(root);
        let lower_left = workspace.test_split(Direction::Vertical);
        workspace.tabs[0].layout.focus_pane(lower_left);
        workspace.test_split(Direction::Vertical);
        workspace.tabs[0].layout.focus_pane(right);
        workspace.test_split(Direction::Vertical);
        for path in [
            &[][..],
            &[false][..],
            &[true][..],
            &[false, true][..],
            &[true, false][..],
        ] {
            let ratio = 0.05 + f32::from(sweep.next(91)) / 100.0;
            workspace.tabs[0].layout.set_ratio_at(path, ratio);
        }
        let mut state = crate::app::AppState::test_new();
        state.workspaces = vec![workspace];
        state.active = Some(0);
        state.selected = 0;
        state
    }

    /// The `smallest` policy lays a tab out once, in the componentwise minimum
    /// of its viewers' surfaces. That is only enough if no pane laid out in a
    /// smaller area is larger than the same pane laid out in a larger one, for
    /// every chrome setting, split ratio and zoom. This pins that assumption.
    #[test]
    fn a_smaller_area_never_lays_out_a_larger_pane() {
        let registry = crate::terminal::TerminalRuntimeRegistry::new();
        let mut sweep = Sweep(0x9e37_79b9_7f4a_7c15);
        let (mut checked, mut degenerate) = (0_u32, 0_u32);
        for _ in 0..60 {
            let mut state = nested_layout_state(&mut sweep);
            for borders in [
                crate::config::PaneBordersConfig::Auto,
                crate::config::PaneBordersConfig::Always,
                crate::config::PaneBordersConfig::Off,
            ] {
                for (gaps, outer_borders, zoomed) in [
                    (false, false, false),
                    (true, false, false),
                    (false, true, false),
                    (true, true, false),
                    (true, true, true),
                    (false, false, true),
                ] {
                    state.pane_borders = borders;
                    state.pane_gaps = gaps;
                    state.pane_outer_borders = outer_borders;
                    state.workspaces[0].tabs[0].zoomed = zoomed;
                    let small = Rect::new(
                        0,
                        0,
                        MIN_SIZE_VOTE_COLS + sweep.next(150),
                        MIN_SIZE_VOTE_ROWS + sweep.next(60),
                    );
                    let large = Rect::new(
                        0,
                        0,
                        small.width + sweep.next(80),
                        small.height + sweep.next(30),
                    );
                    let layout = |area| {
                        crate::ui::compute_tab_surface_for(
                            &state,
                            &registry,
                            Some(crate::ui::TabSurfaceTarget {
                                workspace_index: 0,
                                tab_index: 0,
                            }),
                            area,
                            false,
                            crate::kitty_graphics::HostCellSize::default(),
                        )
                        .pane_infos
                    };
                    let large_panes = layout(large);
                    let small_panes = layout(small);
                    // A pane collapsed to zero cells in the smaller area must
                    // not cost a sibling its border there: that sibling would
                    // gain a row the larger viewer lacks, and the PTY sized
                    // from the smaller area would crop that viewer.
                    degenerate += u32::from(
                        small_panes
                            .iter()
                            .any(|pane| pane.rect.width == 0 || pane.rect.height == 0),
                    );
                    checked += 1;
                    for pane in small_panes {
                        let Some(bigger) = large_panes.iter().find(|other| other.id == pane.id)
                        else {
                            panic!("pane {:?} missing from the larger layout", pane.id);
                        };
                        assert!(
                            pane.inner_rect.width <= bigger.inner_rect.width
                                && pane.inner_rect.height <= bigger.inner_rect.height,
                            "{small:?} -> {:?} vs {large:?} -> {:?} \
                             (borders {borders:?}, gaps {gaps}, outer {outer_borders}, \
                             zoomed {zoomed})",
                            pane.inner_rect,
                            bigger.inner_rect,
                        );
                    }
                }
            }
        }
        assert!(
            degenerate > 0 && checked > degenerate,
            "the sweep must cover collapsed panes as well as usable layouts \
             ({checked} checked, {degenerate} with a collapsed pane)"
        );
    }
}
