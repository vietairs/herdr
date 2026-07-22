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

use crate::api::schema::common::AgentStatus;
use crate::api::schema::events::{EventData, EventKind};
use crate::api::schema::panes::PaneInfo;
use crate::api::schema::session::SessionSnapshot;
use crate::api::schema::tabs::TabInfo;
use crate::api::schema::workspaces::WorkspaceInfo;
use crate::api::schema::EventEnvelope;
use crate::api::EventHub;

use super::id::{fence, map_in, FenceResult, Mount};
use super::protocol::{AgentStatusMessage, EventChannelMessage, EventCursor};
use super::sanitize::{sanitize_remote_string, sanitize_remote_string_opt};

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

/// Honest rendering of a mirrored pane's agent status (S14.1/S14.2):
/// distinguishes a value the mirror believes is currently live from one
/// that is only "last known" because the mount is disconnected. A UI
/// consumer (P8/P9) must never render `Stale` as if it were `Live`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentStatusDisplay {
    Live(AgentStatus),
    Stale(AgentStatus),
}

/// Per-mount replica mirror: namespaced (`FedRef`-encoded) copies of the
/// remote's workspaces/tabs/panes. Metadata only — no pane bytes (P5).
///
/// `Clone` (REVISED Phase A): the server-owned mount handler needs a
/// point-in-time snapshot for `AppState.remote_mirror` (bookkeeping /
/// `double_attach_conflict`) while the ORIGINAL mirror moves into the
/// ongoing drive task, which is the one that stays live-synced — mirrors
/// `run_federated_session`'s existing materialize-then-move disposition.
/// `AppState.remote_mirror` is therefore a mount-time snapshot, not
/// continuously updated post-mount in v1 (logged in implementation-notes.md).
#[derive(Clone)]
pub(crate) struct RemoteMirror {
    mount: Mount,
    cursor: u64,
    workspaces: HashMap<String, WorkspaceInfo>,
    tabs: HashMap<String, TabInfo>,
    panes: HashMap<String, PaneInfo>,
    /// P6/S14.2: set by the (P9-owned) disconnect surface. While `true`,
    /// [`RemoteMirror::agent_status_display`] must report every pane's
    /// agent status as "last known/stale", never live truth — the mirror
    /// itself keeps mutating normally (a message that legitimately arrives
    /// right before/after a flip still applies); only the *display*
    /// decision changes.
    stale: bool,
}

impl RemoteMirror {
    pub(crate) fn new(mount: Mount) -> Self {
        Self {
            mount,
            cursor: 0,
            workspaces: HashMap::new(),
            tabs: HashMap::new(),
            panes: HashMap::new(),
            stale: false,
        }
    }

    /// Flips the disconnect/stale marker (P9 call site). Does not itself
    /// touch mirrored data.
    pub(crate) fn set_stale(&mut self, stale: bool) {
        self.stale = stale;
    }

    pub(crate) fn is_stale(&self) -> bool {
        self.stale
    }

    /// Honest read of one namespaced pane's agent status (S14.1/S14.2):
    /// wraps the mirrored value in [`AgentStatusDisplay::Stale`] whenever
    /// this mount is marked disconnected, so a caller can never render a
    /// stale value as if it were live. Returns `None` if the pane id is
    /// unknown to this mirror.
    pub(crate) fn agent_status_display(&self, pane_id: &str) -> Option<AgentStatusDisplay> {
        let status = self.panes.get(pane_id)?.agent_status;
        Some(if self.stale {
            AgentStatusDisplay::Stale(status)
        } else {
            AgentStatusDisplay::Live(status)
        })
    }

    /// Applies one relayed [`AgentStatusMessage`] — the remote's real
    /// foreground-process signal (P3), the only honest source for a remote
    /// pane's process-derived status (requirement 1). Fenced identically to
    /// [`Self::apply_event_message`] (S14.3: same discipline, no ad-hoc
    /// channel). Looks the target pane up by its *namespaced* terminal id
    /// rather than trusting a pane id on the wire (there isn't one — the
    /// message only carries the remote's raw `terminal_id`); an id this
    /// mirror hasn't hydrated (raced with close, or not yet mounted) is
    /// silently dropped rather than fabricating a pane to attach it to
    /// (S14.1: never show wrong status).
    pub(crate) fn apply_agent_status(
        &mut self,
        msg: &AgentStatusMessage,
        generation: u64,
        hub: &EventHub,
    ) -> ReducerAction {
        if let FenceResult::RejectStale { .. } = fence(&self.mount, generation) {
            return ReducerAction::RejectedStale;
        }
        let namespaced_terminal_id = map_in(msg.terminal_id.clone(), &self.mount).to_public_id();
        let Some(pane_id) = self
            .panes
            .values()
            .find(|pane| pane.terminal_id == namespaced_terminal_id)
            .map(|pane| pane.pane_id.clone())
        else {
            return ReducerAction::Ignored;
        };
        let pane = self
            .panes
            .get_mut(&pane_id)
            .expect("pane_id was just looked up from this same map");
        if pane.agent_status == msg.status {
            return ReducerAction::Ignored;
        }
        pane.agent_status = msg.status;
        hub.push(EventEnvelope {
            event: EventKind::PaneUpdated,
            data: EventData::PaneUpdated { pane: pane.clone() },
        });
        ReducerAction::Applied {
            source_seq: self.cursor,
            kind: EventKind::PaneUpdated,
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
            self.workspaces
                .insert(namespaced.workspace_id.clone(), namespaced);
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

/// Namespaces `workspace`'s ids and — the single P7 ingest choke point for
/// this entity kind (S11.1) — neutralizes its remote-sourced chrome string
/// (`label`) of any control/ANSI/OSC sequence before it can ever reach a
/// mirrored `WorkspaceInfo` a caller renders. Ids are namespaced identifiers
/// (`FedRef`-encoded), not free-form remote text, so they are not sanitized
/// here (a raw remote id containing control bytes would already fail to
/// round-trip as a valid public id).
fn namespace_workspace(mount: &Mount, workspace: &WorkspaceInfo) -> WorkspaceInfo {
    let mut namespaced = workspace.clone();
    namespaced.workspace_id = map_in(workspace.workspace_id.clone(), mount).to_public_id();
    namespaced.active_tab_id = map_in(workspace.active_tab_id.clone(), mount).to_public_id();
    namespaced.label = sanitize_remote_string(&namespaced.label);
    if let Some(worktree) = namespaced.worktree.as_mut() {
        worktree.repo_key = sanitize_remote_string(&worktree.repo_key);
        worktree.repo_name = sanitize_remote_string(&worktree.repo_name);
        worktree.repo_root = sanitize_remote_string(&worktree.repo_root);
    }
    namespaced
}

/// Namespaces `tab`'s ids and sanitizes its remote-sourced `label` (S11.1),
/// same choke-point contract as [`namespace_workspace`].
fn namespace_tab(mount: &Mount, tab: &TabInfo) -> TabInfo {
    let mut namespaced = tab.clone();
    namespaced.tab_id = map_in(tab.tab_id.clone(), mount).to_public_id();
    namespaced.workspace_id = map_in(tab.workspace_id.clone(), mount).to_public_id();
    namespaced.label = sanitize_remote_string(&namespaced.label);
    namespaced
}

/// Namespaces `pane`'s ids and sanitizes every remote-sourced chrome string
/// it carries (S11.1: `custom_name`/`identity_cwd`/`agent_name`-class
/// fields) — `cwd`, `foreground_cwd`, `label`, `agent`, `title`,
/// `terminal_title`, `terminal_title_stripped`, `display_agent`, the values
/// of `state_labels`/`tokens`, and a mirrored `agent_session`'s `value`.
/// Raw PTY bytes destined for the ghostty pane emulator never pass through
/// this function (see module docs / `pane_source`); only this
/// `PaneInfo`-shaped metadata does.
fn namespace_pane(mount: &Mount, pane: &PaneInfo) -> PaneInfo {
    let mut namespaced = pane.clone();
    namespaced.pane_id = map_in(pane.pane_id.clone(), mount).to_public_id();
    namespaced.terminal_id = map_in(pane.terminal_id.clone(), mount).to_public_id();
    namespaced.workspace_id = map_in(pane.workspace_id.clone(), mount).to_public_id();
    namespaced.tab_id = map_in(pane.tab_id.clone(), mount).to_public_id();
    namespaced.cwd = sanitize_remote_string_opt(namespaced.cwd);
    namespaced.foreground_cwd = sanitize_remote_string_opt(namespaced.foreground_cwd);
    namespaced.label = sanitize_remote_string_opt(namespaced.label);
    namespaced.agent = sanitize_remote_string_opt(namespaced.agent);
    namespaced.title = sanitize_remote_string_opt(namespaced.title);
    namespaced.terminal_title = sanitize_remote_string_opt(namespaced.terminal_title);
    namespaced.terminal_title_stripped =
        sanitize_remote_string_opt(namespaced.terminal_title_stripped);
    namespaced.display_agent = sanitize_remote_string_opt(namespaced.display_agent);
    for value in namespaced.state_labels.values_mut() {
        *value = sanitize_remote_string(value);
    }
    for value in namespaced.tokens.values_mut() {
        *value = sanitize_remote_string(value);
    }
    if let Some(agent_session) = namespaced.agent_session.as_mut() {
        agent_session.value = sanitize_remote_string(&agent_session.value);
    }
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
    let retired: Vec<String> = mirror
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
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

fn reconcile_tabs(
    mount: &Mount,
    mirror: &mut HashMap<String, TabInfo>,
    incoming: &[TabInfo],
    hub: &EventHub,
) {
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

fn reconcile_panes(
    mount: &Mount,
    mirror: &mut HashMap<String, PaneInfo>,
    incoming: &[PaneInfo],
    hub: &EventHub,
) {
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

    fn pane(id: &str, terminal_id: &str, agent_status: AgentStatus) -> PaneInfo {
        PaneInfo {
            pane_id: id.to_string(),
            terminal_id: terminal_id.to_string(),
            workspace_id: "w1".to_string(),
            tab_id: "w1-tab".to_string(),
            focused: false,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status,
            state_labels: Default::default(),
            tokens: Default::default(),
            agent_session: None,
            scroll: None,
            revision: 0,
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
        assert!(
            !ids.iter().any(|id| id.ends_with(":w2")),
            "w2 must be tombstoned"
        );

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

    // Phase 06 test 2: a relayed `AgentStatus` (P3's foreground-process
    // equivalent) updates the namespaced pane's status and pushes a
    // `PaneUpdated` local event — with zero involvement of any local
    // process probe (this test never touches `pane.rs`/`detect` at all,
    // proving the update is entirely wire-driven).
    #[test]
    fn relayed_agent_status_updates_the_namespaced_pane_and_pushes_pane_updated() {
        let mut snapshot = empty_snapshot();
        snapshot.panes.push(pane("p1", "term_1", AgentStatus::Idle));
        let mut mirror = RemoteMirror::new(mount(1));
        mirror.apply_snapshot(&snapshot, EventCursor(1));
        let hub = EventHub::default();

        let action = mirror.apply_agent_status(
            &AgentStatusMessage {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                status: AgentStatus::Working,
            },
            1,
            &hub,
        );

        assert!(matches!(action, ReducerAction::Applied { .. }));
        let (_, updated) = mirror
            .panes()
            .iter()
            .find(|(_, p)| p.terminal_id.ends_with(":term_1"))
            .expect("namespaced pane must exist");
        assert_eq!(updated.agent_status, AgentStatus::Working);
        let events = hub.events_after(0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1.event, EventKind::PaneUpdated);

        // An unknown terminal id (raced with close / not-yet-hydrated) is
        // dropped silently — S14.1: never fabricate a pane to attach it to.
        let unknown = mirror.apply_agent_status(
            &AgentStatusMessage {
                terminal_id: "term_ghost".to_string(),
                mount_generation: 1,
                status: AgentStatus::Blocked,
            },
            1,
            &hub,
        );
        assert_eq!(unknown, ReducerAction::Ignored);
        assert_eq!(
            hub.events_after(0).len(),
            1,
            "no spurious event for an unknown pane"
        );
    }

    // Phase 06: relayed status is fenced exactly like event-channel traffic
    // (S14.3: same discipline, no ad-hoc channel) — a message observed
    // under a stale generation is rejected, never applied.
    #[test]
    fn relayed_agent_status_under_a_stale_generation_is_fenced_and_rejected() {
        let mut snapshot = empty_snapshot();
        snapshot.panes.push(pane("p1", "term_1", AgentStatus::Idle));
        let mut mirror = RemoteMirror::new(mount(3));
        mirror.apply_snapshot(&snapshot, EventCursor(0));
        let hub = EventHub::default();

        let stale = mirror.apply_agent_status(
            &AgentStatusMessage {
                terminal_id: "term_1".to_string(),
                mount_generation: 2,
                status: AgentStatus::Working,
            },
            2,
            &hub,
        );
        assert_eq!(stale, ReducerAction::RejectedStale);
        assert!(hub.events_after(0).is_empty());
    }

    // Phase 06 test 3 (S14.3 ordering): a full-resync (`reconcile_by_diff`,
    // driven by the gap/reset path) is always the authoritative truth for
    // whatever it carries — it overwrites an in-between relayed delta the
    // resync's own source snapshot didn't yet reflect — but a delta applied
    // *after* a resync still takes effect normally. No channel is allowed
    // to leave the mirror in a causally-backwards state.
    #[test]
    fn snapshot_reconcile_and_relayed_status_apply_in_causal_order() {
        fn status_of(mirror: &RemoteMirror, raw_pane_id: &str) -> Option<AgentStatus> {
            mirror
                .panes()
                .values()
                .find(|p| p.pane_id.ends_with(&format!(":{raw_pane_id}")))
                .map(|p| p.agent_status)
        }

        let mut snapshot = empty_snapshot();
        snapshot.panes.push(pane("p1", "term_1", AgentStatus::Idle));
        let mut mirror = RemoteMirror::new(mount(1));
        mirror.apply_snapshot(&snapshot, EventCursor(1));
        let hub = EventHub::default();

        // A relayed delta arrives first.
        mirror.apply_agent_status(
            &AgentStatusMessage {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                status: AgentStatus::Working,
            },
            1,
            &hub,
        );
        assert_eq!(status_of(&mirror, "p1"), Some(AgentStatus::Working));

        // A gap forces a fresh full resync whose own snapshot still shows
        // Idle (it was taken before the delta) — the resync wins, since it
        // is the mirror's only authoritative resync mechanism.
        let mut fresh = empty_snapshot();
        fresh.panes.push(pane("p1", "term_1", AgentStatus::Idle));
        mirror.reconcile_by_diff(&fresh, EventCursor(5), &hub);
        assert_eq!(status_of(&mirror, "p1"), Some(AgentStatus::Idle));

        // A delta applied after the resync still applies normally.
        mirror.apply_agent_status(
            &AgentStatusMessage {
                terminal_id: "term_1".to_string(),
                mount_generation: 1,
                status: AgentStatus::Blocked,
            },
            1,
            &hub,
        );
        assert_eq!(status_of(&mirror, "p1"), Some(AgentStatus::Blocked));
    }

    // Phase 06 test 4 (S14.2 staleness): while the mount is marked
    // disconnected, a status query must report "last known/stale", never
    // live truth.
    #[test]
    fn agent_status_display_reports_stale_while_disconnected() {
        let mut snapshot = empty_snapshot();
        snapshot
            .panes
            .push(pane("p1", "term_1", AgentStatus::Working));
        let mut mirror = RemoteMirror::new(mount(1));
        mirror.apply_snapshot(&snapshot, EventCursor(1));

        let pane_id = mirror.panes().keys().next().cloned().unwrap();
        assert_eq!(
            mirror.agent_status_display(&pane_id),
            Some(AgentStatusDisplay::Live(AgentStatus::Working))
        );

        mirror.set_stale(true);
        assert!(mirror.is_stale());
        assert_eq!(
            mirror.agent_status_display(&pane_id),
            Some(AgentStatusDisplay::Stale(AgentStatus::Working)),
            "must render last-known, never live truth, while disconnected"
        );

        mirror.set_stale(false);
        assert_eq!(
            mirror.agent_status_display(&pane_id),
            Some(AgentStatusDisplay::Live(AgentStatus::Working))
        );

        assert_eq!(mirror.agent_status_display("no-such-pane"), None);
    }

    // Phase 07 test 1 (S11.1 Blocker): a crafted remote workspace/pane
    // carrying ANSI/OSC injection in every rendered chrome string field is
    // neutralized at the single P4 ingest choke point (`apply_snapshot`) —
    // no active control sequence survives into the mirror, and this is
    // proven for BOTH the initial `apply_snapshot` path and the
    // `reconcile_by_diff` resync path, so neither choke point can be
    // bypassed.
    #[test]
    fn remote_chrome_strings_are_sanitized_on_ingest_via_both_snapshot_paths() {
        let evil = "name\x1b[2J\x1b]52;c;ZXZpbA==\x07tail";
        let expect_sanitized = "name[2J]52;c;ZXZpbA==tail";

        let mut snapshot = empty_snapshot();
        let mut ws = workspace("w1");
        ws.label = evil.to_string();
        snapshot.workspaces.push(ws);
        let mut poisoned_pane = pane("p1", "term_1", AgentStatus::Idle);
        poisoned_pane.cwd = Some(evil.to_string());
        poisoned_pane.label = Some(evil.to_string());
        poisoned_pane.agent = Some(evil.to_string());
        poisoned_pane
            .state_labels
            .insert("k".to_string(), evil.to_string());
        snapshot.panes.push(poisoned_pane);

        // Path 1: initial mount (`apply_snapshot`).
        let mut mirror = RemoteMirror::new(mount(1));
        mirror.apply_snapshot(&snapshot, EventCursor(1));

        let ws_info = mirror.workspaces().values().next().unwrap();
        assert_eq!(ws_info.label, expect_sanitized);
        assert!(!ws_info.label.contains('\x1b'));
        assert!(!ws_info.label.contains('\x07'));

        let pane_info = mirror.panes().values().next().unwrap();
        assert_eq!(pane_info.cwd.as_deref(), Some(expect_sanitized));
        assert_eq!(pane_info.label.as_deref(), Some(expect_sanitized));
        assert_eq!(pane_info.agent.as_deref(), Some(expect_sanitized));
        assert_eq!(
            pane_info.state_labels.get("k").map(String::as_str),
            Some(expect_sanitized)
        );

        // Path 2: gap-triggered resync (`reconcile_by_diff`) — the OTHER
        // choke point a caller reaches after a `Gap`/`Reset`. A hand-rolled
        // second decode path would be exactly how a sanitize step gets
        // silently skipped (the Risks section's named failure mode); prove
        // it is not.
        let mut fresh = empty_snapshot();
        let mut ws2 = workspace("w2");
        ws2.label = evil.to_string();
        fresh.workspaces.push(ws2);
        let hub = EventHub::default();
        mirror.reconcile_by_diff(&fresh, EventCursor(5), &hub);

        let ws2_info = mirror
            .workspaces()
            .values()
            .find(|w| w.label == expect_sanitized || w.workspace_id.ends_with(":w2"))
            .expect("w2 must be present and sanitized");
        assert_eq!(ws2_info.label, expect_sanitized);

        // Visible, non-control text is preserved untouched.
        assert!(expect_sanitized.starts_with("name"));
        assert!(expect_sanitized.ends_with("tail"));
    }

    // Phase 07 test 3 (S11.4 substrate): a mirrored remote entity carries
    // the correct `HostKey`/`server_instance_id` end to end via
    // `RemoteMirror::mount()` — the substrate P8 will render as an
    // unspoofable badge. Distinct hosts never collide.
    #[test]
    fn mirrored_entity_carries_the_correct_host_key_end_to_end() {
        let mut snapshot = empty_snapshot();
        snapshot.workspaces.push(workspace("w1"));
        let alice_mount = Mount {
            host_key: HostKey::new("alice@10.0.0.1", "s1"),
            server_instance_id: ServerInstanceId("inst-alice".to_string()),
            mount_generation: 1,
        };
        let mut mirror = RemoteMirror::new(alice_mount.clone());
        mirror.apply_snapshot(&snapshot, EventCursor(1));

        assert_eq!(mirror.mount().host_key, alice_mount.host_key);
        assert_eq!(
            mirror.mount().server_instance_id,
            alice_mount.server_instance_id
        );

        let public_id = mirror.workspaces().keys().next().unwrap();
        assert!(public_id.starts_with("r:alice@10.0.0.1#s1:"));

        // A different host's mirror never produces a colliding id prefix.
        let bob_mount = Mount {
            host_key: HostKey::new("bob@10.0.0.2", "s1"),
            server_instance_id: ServerInstanceId("inst-bob".to_string()),
            mount_generation: 1,
        };
        let mut bob_mirror = RemoteMirror::new(bob_mount);
        bob_mirror.apply_snapshot(&snapshot, EventCursor(1));
        let bob_public_id = bob_mirror.workspaces().keys().next().unwrap();
        assert_ne!(public_id, bob_public_id);
        assert!(bob_public_id.starts_with("r:bob@10.0.0.2#s1:"));
    }
}
