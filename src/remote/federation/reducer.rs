//! Per-mount replica reducer (P4): mirrors a remote `SessionSnapshot` into
//! namespaced (`FedRef`) metadata entries and, on resync, emits ordinary
//! local `EventHub::push` calls so local consumers (`events.wait`,
//! subscriptions) see the mirror exactly like any locally-created entity.
//! `EventHub` stays single-source (RT-F5) — this reducer owns its own
//! resumable cursor and never touches `EventHub` internals directly.
//!
//! No pane bytes here (P5 on focus) — metadata only.
//!
//! # Wire-payload limitation (deviation — logged in `implementation-notes.md`)
//! `protocol::EventFrame` (P1, locked) carries only `{source_seq, kind}` —
//! no entity id or payload. A normal in-order `Frame` therefore cannot be
//! turned into a valid, per-field `EventData` (every `EventData` variant
//! requires real typed fields the wire never sends). This reducer:
//! - applies `Frame`/`Gap`/`Reset` purely for *cursor* bookkeeping and gap
//!   detection (never blind-applies past a hole — S6.2);
//! - performs the actual entity-level mirroring, and is the *only* place
//!   that emits local `EventHub::push` calls, via [`RemoteMirror::reconcile_by_diff`],
//!   driven off a freshly fetched `MountSnapshot` (initial mount, or a
//!   remount after `Gap`/`Reset`). Extending `EventFrame` to carry the
//!   changed entity's public id (or a small delta) would let per-event
//!   pushes fire without a full remount; that protocol change is outside
//!   P4's file ownership (owned by P1).
//!
//! Per the plan's own risk/rollback note ("new modules unused by any live
//! path until P8 triggers a mount"), nothing in production `App`/`AppState`
//! constructs a `RemoteMirror` yet — only this module's own tests do — so
//! several accessors below are dead code outside `#[cfg(test)]` until P8/P9
//! wire a real call site; allowed at module scope rather than sprinkled
//! per-item.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::api::schema::events::{EventData, EventKind};
use crate::api::schema::panes::PaneInfo;
use crate::api::schema::session::SessionSnapshot;
use crate::api::schema::tabs::TabInfo;
use crate::api::schema::workspaces::WorkspaceInfo;
use crate::api::schema::EventEnvelope;
use crate::api::EventHub;

use super::id::{fence, map_in, FenceResult, Mount};
use super::protocol::{EventChannelMessage, EventCursor};

/// Outcome of applying one [`EventChannelMessage`] to the reducer's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReducerAction {
    /// The delivering task's observed `mount_generation` no longer matches
    /// this mirror's live [`Mount`] (fenced client-side — see module docs on
    /// why `EventFrame` itself carries no wire-level generation tag).
    RejectedStale,
    /// Duplicate/already-applied frame (`source_seq <= cursor`) — ignored,
    /// not an error.
    Ignored,
    /// In-order frame; cursor advanced. Carries the source's `EventKind`
    /// for observability only (see module docs: no local push is derivable
    /// from this alone).
    Applied { source_seq: u64, kind: EventKind },
    /// A hole in `source_seq` was detected (either the source's own `Gap`
    /// marker, or a locally-detected skip-ahead). The caller must re-mount
    /// (fetch a fresh `MountSnapshot`) and call `reconcile_by_diff`.
    GapDetected { from: u64, to: u64 },
    /// The source discarded cursor continuity; the caller must re-mount.
    ResetRequired,
}

/// Per-mount replica mirror: namespaced (`FedRef`-encoded) copies of the
/// remote's workspaces/tabs/panes. Metadata only — no pane bytes (P5).
pub(crate) struct RemoteMirror {
    mount: Mount,
    cursor: u64,
    workspaces: HashMap<String, WorkspaceInfo>,
    tabs: HashMap<String, TabInfo>,
    panes: HashMap<String, PaneInfo>,
}

impl RemoteMirror {
    pub(crate) fn new(mount: Mount) -> Self {
        Self {
            mount,
            cursor: 0,
            workspaces: HashMap::new(),
            tabs: HashMap::new(),
            panes: HashMap::new(),
        }
    }

    pub(crate) fn mount(&self) -> &Mount {
        &self.mount
    }

    pub(crate) fn cursor(&self) -> u64 {
        self.cursor
    }

    pub(crate) fn workspaces(&self) -> &HashMap<String, WorkspaceInfo> {
        &self.workspaces
    }

    pub(crate) fn tabs(&self) -> &HashMap<String, TabInfo> {
        &self.tabs
    }

    pub(crate) fn panes(&self) -> &HashMap<String, PaneInfo> {
        &self.panes
    }

    /// Applies the atomic mount payload: replaces every namespaced entry
    /// with the remote's current content. Safe as a full replace (not a
    /// diff) here specifically because this is called once, right after a
    /// *fresh* mount whose generation has never had any prior content in
    /// this mirror.
    pub(crate) fn apply_snapshot(&mut self, snapshot: &SessionSnapshot, cursor: EventCursor) {
        self.workspaces.clear();
        self.tabs.clear();
        self.panes.clear();
        for workspace in &snapshot.workspaces {
            let namespaced = namespace_workspace(&self.mount, workspace);
            self.workspaces.insert(namespaced.workspace_id.clone(), namespaced);
        }
        for tab in &snapshot.tabs {
            let namespaced = namespace_tab(&self.mount, tab);
            self.tabs.insert(namespaced.tab_id.clone(), namespaced);
        }
        for pane in &snapshot.panes {
            let namespaced = namespace_pane(&self.mount, pane);
            self.panes.insert(namespaced.pane_id.clone(), namespaced);
        }
        self.cursor = cursor.0;
    }

    /// Applies one event-channel message purely for cursor/ordering
    /// purposes (see module docs). `generation` is the mount generation the
    /// delivering task observed when it read the message off the wire —
    /// fenced against this mirror's live [`Mount`] so traffic from a task
    /// superseded by a reconnect is rejected (codex #2 / top risk 3).
    pub(crate) fn apply_event_message(
        &mut self,
        msg: &EventChannelMessage,
        generation: u64,
    ) -> ReducerAction {
        if let FenceResult::RejectStale { .. } = fence(&self.mount, generation) {
            return ReducerAction::RejectedStale;
        }
        match msg {
            EventChannelMessage::Frame(frame) => {
                if frame.source_seq <= self.cursor {
                    return ReducerAction::Ignored;
                }
                if frame.source_seq != self.cursor + 1 {
                    return ReducerAction::GapDetected {
                        from: self.cursor,
                        to: frame.source_seq - 1,
                    };
                }
                self.cursor = frame.source_seq;
                ReducerAction::Applied {
                    source_seq: frame.source_seq,
                    kind: frame.kind,
                }
            }
            EventChannelMessage::Gap { from, to } => ReducerAction::GapDetected {
                from: *from,
                to: *to,
            },
            EventChannelMessage::Reset => ReducerAction::ResetRequired,
        }
    }

    /// Reconciles the mirror against a freshly fetched `MountSnapshot`
    /// (called for the initial mount, and again after every `Gap`/`Reset`
    /// re-mount) — diffing by namespaced id so retired entities are
    /// tombstoned and new ones are added, rather than a blind
    /// append/replace (S6.2). Every change is pushed onto `hub` as an
    /// ordinary local event, so local consumers see the mirror exactly like
    /// any local mutation — this is the reducer's only local-push path (see
    /// module docs).
    pub(crate) fn reconcile_by_diff(
        &mut self,
        snapshot: &SessionSnapshot,
        cursor: EventCursor,
        hub: &EventHub,
    ) {
        reconcile_workspaces(&self.mount, &mut self.workspaces, &snapshot.workspaces, hub);
        reconcile_tabs(&self.mount, &mut self.tabs, &snapshot.tabs, hub);
        reconcile_panes(&self.mount, &mut self.panes, &snapshot.panes, hub);
        self.cursor = cursor.0;
    }
}

fn namespace_workspace(mount: &Mount, workspace: &WorkspaceInfo) -> WorkspaceInfo {
    let mut namespaced = workspace.clone();
    namespaced.workspace_id = map_in(workspace.workspace_id.clone(), mount).to_public_id();
    namespaced.active_tab_id = map_in(workspace.active_tab_id.clone(), mount).to_public_id();
    namespaced
}

fn namespace_tab(mount: &Mount, tab: &TabInfo) -> TabInfo {
    let mut namespaced = tab.clone();
    namespaced.tab_id = map_in(tab.tab_id.clone(), mount).to_public_id();
    namespaced.workspace_id = map_in(tab.workspace_id.clone(), mount).to_public_id();
    namespaced
}

fn namespace_pane(mount: &Mount, pane: &PaneInfo) -> PaneInfo {
    let mut namespaced = pane.clone();
    namespaced.pane_id = map_in(pane.pane_id.clone(), mount).to_public_id();
    namespaced.terminal_id = map_in(pane.terminal_id.clone(), mount).to_public_id();
    namespaced.workspace_id = map_in(pane.workspace_id.clone(), mount).to_public_id();
    namespaced.tab_id = map_in(pane.tab_id.clone(), mount).to_public_id();
    namespaced
}

fn reconcile_workspaces(
    mount: &Mount,
    mirror: &mut HashMap<String, WorkspaceInfo>,
    incoming: &[WorkspaceInfo],
    hub: &EventHub,
) {
    let mut seen: HashSet<String> = HashSet::new();
    for workspace in incoming {
        let namespaced = namespace_workspace(mount, workspace);
        let id = namespaced.workspace_id.clone();
        seen.insert(id.clone());
        match mirror.get(&id) {
            Some(existing) if existing == &namespaced => {}
            Some(_) => {
                mirror.insert(id, namespaced.clone());
                hub.push(EventEnvelope {
                    event: EventKind::WorkspaceUpdated,
                    data: EventData::WorkspaceUpdated {
                        workspace: namespaced,
                    },
                });
            }
            None => {
                mirror.insert(id, namespaced.clone());
                hub.push(EventEnvelope {
                    event: EventKind::WorkspaceCreated,
                    data: EventData::WorkspaceCreated {
                        workspace: namespaced,
                    },
                });
            }
        }
    }
    let retired: Vec<String> = mirror.keys().filter(|id| !seen.contains(*id)).cloned().collect();
    for id in retired {
        mirror.remove(&id);
        hub.push(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id: id,
                workspace: None,
            },
        });
    }
}

fn reconcile_tabs(mount: &Mount, mirror: &mut HashMap<String, TabInfo>, incoming: &[TabInfo], hub: &EventHub) {
    let mut seen: HashSet<String> = HashSet::new();
    for tab in incoming {
        let namespaced = namespace_tab(mount, tab);
        let id = namespaced.tab_id.clone();
        seen.insert(id.clone());
        match mirror.get(&id) {
            Some(existing) if existing == &namespaced => {}
            Some(_) => {
                let workspace_id = namespaced.workspace_id.clone();
                let label = namespaced.label.clone();
                mirror.insert(id.clone(), namespaced);
                hub.push(EventEnvelope {
                    event: EventKind::TabRenamed,
                    data: EventData::TabRenamed {
                        tab_id: id,
                        workspace_id,
                        label,
                    },
                });
            }
            None => {
                mirror.insert(id, namespaced.clone());
                hub.push(EventEnvelope {
                    event: EventKind::TabCreated,
                    data: EventData::TabCreated { tab: namespaced },
                });
            }
        }
    }
    let retired: Vec<TabInfo> = mirror
        .values()
        .filter(|tab| !seen.contains(&tab.tab_id))
        .cloned()
        .collect();
    for tab in retired {
        mirror.remove(&tab.tab_id);
        hub.push(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id: tab.tab_id,
                workspace_id: tab.workspace_id,
            },
        });
    }
}

fn reconcile_panes(mount: &Mount, mirror: &mut HashMap<String, PaneInfo>, incoming: &[PaneInfo], hub: &EventHub) {
    let mut seen: HashSet<String> = HashSet::new();
    for pane in incoming {
        let namespaced = namespace_pane(mount, pane);
        let id = namespaced.pane_id.clone();
        seen.insert(id.clone());
        match mirror.get(&id) {
            Some(existing) if existing == &namespaced => {}
            Some(_) => {
                mirror.insert(id, namespaced.clone());
                hub.push(EventEnvelope {
                    event: EventKind::PaneUpdated,
                    data: EventData::PaneUpdated { pane: namespaced },
                });
            }
            None => {
                mirror.insert(id, namespaced.clone());
                hub.push(EventEnvelope {
                    event: EventKind::PaneCreated,
                    data: EventData::PaneCreated { pane: namespaced },
                });
            }
        }
    }
    let retired: Vec<PaneInfo> = mirror
        .values()
        .filter(|pane| !seen.contains(&pane.pane_id))
        .cloned()
        .collect();
    for pane in retired {
        mirror.remove(&pane.pane_id);
        hub.push(EventEnvelope {
            event: EventKind::PaneClosed,
            data: EventData::PaneClosed {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::common::AgentStatus;
    use crate::remote::federation::id::HostKey;
    use crate::remote::federation::id::ServerInstanceId;
    use crate::remote::federation::protocol::EventFrame;

    fn mount(generation: u64) -> Mount {
        Mount {
            host_key: HostKey::new("alice@10.0.0.1", "s1"),
            server_instance_id: ServerInstanceId("inst-a".to_string()),
            mount_generation: generation,
        }
    }

    fn workspace(id: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.to_string(),
            number: 1,
            label: "ws".to_string(),
            focused: false,
            pane_count: 0,
            tab_count: 0,
            active_tab_id: format!("{id}-tab"),
            agent_status: AgentStatus::Idle,
            tokens: Default::default(),
            worktree: None,
        }
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: "0.0.0-test".to_string(),
            protocol: 1,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            workspaces: Vec::new(),
            tabs: Vec::new(),
            panes: Vec::new(),
            layouts: Vec::new(),
            agents: Vec::new(),
        }
    }

    // Test 1 (phase TDD plan): mirror populated with namespaced ids; no
    // collision with a pre-existing local `w1`.
    #[test]
    fn apply_snapshot_namespaces_ids_and_never_collides_with_a_local_id() {
        let mut snapshot = empty_snapshot();
        snapshot.workspaces.push(workspace("w1"));
        let mut mirror = RemoteMirror::new(mount(1));

        mirror.apply_snapshot(&snapshot, EventCursor(5));

        assert_eq!(mirror.cursor(), 5);
        assert_eq!(mirror.workspaces().len(), 1);
        let (public_id, info) = mirror.workspaces().iter().next().unwrap();
        assert_ne!(public_id, "w1", "must be namespaced, not the raw remote id");
        assert_eq!(info.workspace_id, *public_id);
        assert!(public_id.starts_with("r:alice@10.0.0.1#s1:"));
    }

    #[test]
    fn frames_apply_in_order_and_advance_the_cursor_without_reordering() {
        let mut mirror = RemoteMirror::new(mount(1));
        mirror.apply_snapshot(&empty_snapshot(), EventCursor(10));

        let a = mirror.apply_event_message(
            &EventChannelMessage::Frame(EventFrame {
                source_seq: 11,
                kind: EventKind::WorkspaceFocused,
            }),
            1,
        );
        assert_eq!(
            a,
            ReducerAction::Applied {
                source_seq: 11,
                kind: EventKind::WorkspaceFocused
            }
        );
        assert_eq!(mirror.cursor(), 11);

        // A duplicate/already-applied frame must never move the cursor
        // backwards or be treated as a gap.
        let dup = mirror.apply_event_message(
            &EventChannelMessage::Frame(EventFrame {
                source_seq: 11,
                kind: EventKind::WorkspaceFocused,
            }),
            1,
        );
        assert_eq!(dup, ReducerAction::Ignored);
        assert_eq!(mirror.cursor(), 11);

        let b = mirror.apply_event_message(
            &EventChannelMessage::Frame(EventFrame {
                source_seq: 12,
                kind: EventKind::WorkspaceFocused,
            }),
            1,
        );
        assert_eq!(
            b,
            ReducerAction::Applied {
                source_seq: 12,
                kind: EventKind::WorkspaceFocused
            }
        );
        assert_eq!(mirror.cursor(), 12);
    }

    // Test 4: gap forces re-sync via reconcile_by_diff (add/remove/rename,
    // tombstone retired ids) rather than blind-append.
    #[test]
    fn gap_is_detected_and_reconcile_by_diff_adds_removes_and_renames() {
        let mut mirror = RemoteMirror::new(mount(1));
        let mut initial = empty_snapshot();
        initial.workspaces.push(workspace("w1"));
        initial.workspaces.push(workspace("w2"));
        mirror.apply_snapshot(&initial, EventCursor(1));

        let gap = mirror.apply_event_message(
            &EventChannelMessage::Frame(EventFrame {
                source_seq: 5,
                kind: EventKind::WorkspaceUpdated,
            }),
            1,
        );
        assert_eq!(gap, ReducerAction::GapDetected { from: 1, to: 4 });
        // A detected gap must never silently mutate the mirror/cursor.
        assert_eq!(mirror.cursor(), 1);
        assert_eq!(mirror.workspaces().len(), 2);

        // Re-sync: w1 renamed (label changed), w2 retired, w3 added.
        let hub = EventHub::default();
        let mut fresh = empty_snapshot();
        let mut w1_renamed = workspace("w1");
        w1_renamed.label = "renamed".to_string();
        fresh.workspaces.push(w1_renamed);
        fresh.workspaces.push(workspace("w3"));

        mirror.reconcile_by_diff(&fresh, EventCursor(5), &hub);

        assert_eq!(mirror.cursor(), 5);
        let ids: HashSet<String> = mirror.workspaces().keys().cloned().collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().any(|id| id.ends_with(":w1")));
        assert!(ids.iter().any(|id| id.ends_with(":w3")));
        assert!(!ids.iter().any(|id| id.ends_with(":w2")), "w2 must be tombstoned");

        // w1 updated (label changed) + w2 closed (retired) + w3 created.
        let events = hub.events_after(0);
        assert_eq!(events.len(), 3);
        let kinds: Vec<EventKind> = events.iter().map(|(_, envelope)| envelope.event).collect();
        assert!(kinds.contains(&EventKind::WorkspaceUpdated));
        assert!(kinds.contains(&EventKind::WorkspaceClosed));
        assert!(kinds.contains(&EventKind::WorkspaceCreated));
    }

    // Test 5 (codex #2, top risk 3): an event message observed under a
    // stale generation is rejected client-side, never applied.
    #[test]
    fn stale_generation_is_fenced_and_rejected() {
        let mut mirror = RemoteMirror::new(mount(3));
        mirror.apply_snapshot(&empty_snapshot(), EventCursor(0));

        let stale = mirror.apply_event_message(
            &EventChannelMessage::Frame(EventFrame {
                source_seq: 1,
                kind: EventKind::WorkspaceFocused,
            }),
            2,
        );
        assert_eq!(stale, ReducerAction::RejectedStale);
        assert_eq!(mirror.cursor(), 0);

        let fresh = mirror.apply_event_message(
            &EventChannelMessage::Frame(EventFrame {
                source_seq: 1,
                kind: EventKind::WorkspaceFocused,
            }),
            3,
        );
        assert!(matches!(fresh, ReducerAction::Applied { .. }));
    }

    #[test]
    fn reset_message_requires_resync() {
        let mut mirror = RemoteMirror::new(mount(1));
        let action = mirror.apply_event_message(&EventChannelMessage::Reset, 1);
        assert_eq!(action, ReducerAction::ResetRequired);
    }
}
