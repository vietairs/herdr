use std::cell::Cell;
use std::io;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};

use bytes::Bytes;
use portable_pty::CommandBuilder;
#[cfg(all(test, unix))]
use portable_pty::{native_pty_system, PtySize};
use ratatui::{layout::Rect, Frame};
#[cfg(test)]
use tokio::sync::watch;
use tokio::sync::{broadcast, mpsc, Notify};
#[cfg(not(windows))]
use tracing::debug;
use tracing::{error, info, warn};

use crate::api::schema::common::AgentStatus;
use crate::detect::{Agent, AgentState};
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::pty::actor::{PtyIoActorConfig, PtyIoActorHandle, PtyReadResult};
use crate::remote::federation::pane_source::{
    RemoteTerminalSourceConfig, RemoteTerminalSourceHandle,
};
use crate::remote::federation::protocol::{ClipboardMessage, FederationMessage};
use crate::terminal::{LocalChild, TerminalSource};

mod agent_detection;
mod cursor;
mod input;
mod kitty_keyboard;
mod osc;
mod state;
mod terminal;
mod xtgettcap;

use self::agent_detection::{
    decide_detection_screen_read, decide_screen_detection_publish,
    detection_update_for_publish_with_osc, mark_detection_content_changed,
    observe_detection_content_change, DetectionPublishDecision, DetectionScreenReadDecision,
    DetectionScreenReadInput, PendingIdleConfirmation, ScreenDetectionPublishInput,
    AGENT_PENDING_IDLE_RECHECK, AGENT_STARTUP_GRACE_WINDOW,
};
use self::terminal::{GhosttyPaneTerminal, PaneTerminal};
pub(crate) use self::terminal::{
    TerminalDirtyPatch, TerminalDirtyPatchOutcome, TerminalTextMatch, TerminalTextPoint,
    TerminalWordMotion,
};
pub use self::{
    state::PaneState,
    terminal::{InputState, ScrollMetrics, TerminalCursorState},
};

const RELEASE_REACQUIRE_SUPPRESSION: std::time::Duration = std::time::Duration::from_secs(1);
const PANE_TERM: &str = "xterm-256color";
const PANE_COLORTERM: &str = "truecolor";

fn apply_pane_terminal_env(cmd: &mut CommandBuilder) {
    // Each pane is rendered by herdr's own terminal layer, not the outer terminal
    // that launched the app. Advertising the inherited TERM leaks the host terminal
    // identity into shells and across SSH, which breaks redraw and cursor movement
    // when the remote side lacks matching terminfo entries.
    cmd.env("TERM", PANE_TERM);
    cmd.env("COLORTERM", PANE_COLORTERM);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PaneLaunchEnv {
    extra: Vec<(String, String)>,
    identity: Option<PaneLaunchIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneLaunchIdentity {
    workspace_id: String,
    tab_id: String,
    pane_id: String,
}

impl PaneLaunchEnv {
    pub(crate) fn from_extra(extra: Vec<(String, String)>) -> Self {
        Self {
            extra,
            identity: None,
        }
    }

    pub(crate) fn with_identity(
        mut self,
        workspace_id: String,
        tab_id: String,
        pane_id: String,
    ) -> Self {
        self.identity = Some(PaneLaunchIdentity {
            workspace_id,
            tab_id,
            pane_id,
        });
        self
    }
}

fn apply_pane_launch_env(cmd: &mut CommandBuilder, launch_env: &PaneLaunchEnv) {
    for (key, value) in &launch_env.extra {
        cmd.env(key, value);
    }
    cmd.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
    crate::integration::apply_pane_base_env(cmd);
    if let Some(identity) = &launch_env.identity {
        cmd.env(
            crate::integration::HERDR_WORKSPACE_ID_ENV_VAR,
            &identity.workspace_id,
        );
        cmd.env(crate::integration::HERDR_TAB_ID_ENV_VAR, &identity.tab_id);
        cmd.env(crate::integration::HERDR_PANE_ID_ENV_VAR, &identity.pane_id);
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingAgentRelease {
    agent: Agent,
    until: std::time::Instant,
}

#[derive(Clone, Copy, Default)]
struct SpawnInitialState<'a> {
    detected_agent: Option<Agent>,
    history_ansi: Option<&'a str>,
    windows_powershell_prompt_cwd_reporting: bool,
}

fn active_pending_release(
    pending_release: &Mutex<Option<PendingAgentRelease>>,
    now: std::time::Instant,
) -> Option<Agent> {
    let mut pending_release = pending_release.lock().ok()?;
    match *pending_release {
        Some(pending) if now < pending.until => Some(pending.agent),
        Some(_) => {
            *pending_release = None;
            None
        }
        None => None,
    }
}

async fn publish_state_changed_event(
    state_events: mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    agent: Option<Agent>,
    state: AgentState,
    visible_blocker: bool,
    visible_working: bool,
    process_exited: bool,
    observed_at: std::time::Instant,
) {
    // This runs on the async detector task, not the PTY reader thread.
    // Waiting for queue space here preserves correctness-critical state transitions
    // without blocking pane I/O.
    if let Err(e) = state_events
        .send(AppEvent::StateChanged {
            pane_id,
            agent,
            state,
            visible_blocker,
            visible_working,
            process_exited,
            observed_at,
        })
        .await
    {
        warn!(
            pane = pane_id.raw(),
            err = %e,
            "failed to deliver StateChanged event"
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct AgentDetectionPublishUpdate {
    state: AgentState,
    visible_idle: bool,
    visible_blocker: bool,
    visible_working: bool,
    process_exited: bool,
}

async fn apply_agent_detection_publish_update(
    state_events: mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    agent: Option<Agent>,
    update: AgentDetectionPublishUpdate,
    observed_at: std::time::Instant,
    state: &mut AgentState,
    last_visible_idle: &mut bool,
    last_visible_blocker: &mut bool,
    last_visible_working: &mut bool,
    last_visible_signal_refresh: &mut Option<std::time::Instant>,
    foreground_shell_exit_reported: &mut bool,
) {
    *state = update.state;
    *last_visible_idle = update.visible_idle;
    *last_visible_blocker = update.visible_blocker;
    *last_visible_working = update.visible_working;
    *last_visible_signal_refresh = if update.visible_blocker || update.visible_working {
        Some(observed_at)
    } else {
        None
    };
    if update.process_exited {
        *foreground_shell_exit_reported = true;
    }
    publish_state_changed_event(
        state_events,
        pane_id,
        agent,
        update.state,
        update.visible_blocker,
        update.visible_working,
        update.process_exited,
        observed_at,
    )
    .await;
}

const AGENT_MISS_CONFIRMATION_ATTEMPTS: u8 = 6;
const PROCESS_RECHECK_IDENTIFIED: std::time::Duration = std::time::Duration::from_secs(5);
const PROCESS_RECHECK_MISSING_FOREGROUND_GROUP: std::time::Duration =
    std::time::Duration::from_secs(30);
const PROCESS_ACQUISITION_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
const PROCESS_ACQUISITION_FAST_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);
const PROCESS_ACQUISITION_FAST_RECHECK: std::time::Duration = std::time::Duration::from_millis(500);
const PROCESS_ACQUISITION_SLOW_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);
const PROCESS_ACQUISITION_IDLE_RESET: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
struct AgentDetectionPresence {
    current_agent: Option<Agent>,
    consecutive_misses: u8,
}

#[cfg(unix)]
fn usable_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    crate::platform::process_cwd(pid).filter(|cwd| cwd.is_absolute() && cwd.is_dir())
}

#[cfg(unix)]
fn foreground_member_cwd_different_from_shell(
    shell_pid: u32,
    shell_cwd: Option<&std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let job = crate::detect::foreground_job(shell_pid)?;
    for process in job.processes {
        if process.pid == shell_pid {
            continue;
        }
        let Some(cwd) = usable_process_cwd(process.pid) else {
            continue;
        };
        if shell_cwd != Some(&cwd) {
            return Some(cwd);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundShellAgentAction {
    ObserveProbe,
    ReportProcessExit,
    ClearAgent,
}

fn foreground_shell_agent_action(
    previous_agent: Option<Agent>,
    new_agent: Option<Agent>,
    foreground_is_pane_shell: bool,
    process_exit_reported: bool,
) -> ForegroundShellAgentAction {
    if previous_agent.is_none() || new_agent.is_some() {
        return ForegroundShellAgentAction::ObserveProbe;
    }

    if process_exit_reported {
        return ForegroundShellAgentAction::ClearAgent;
    }

    if foreground_is_pane_shell {
        // Do not clear identity immediately. First publish an idle process-exit
        // transition for the previous agent so notifications and wait-agent callers
        // observe completion before the pane becomes unknown.
        return ForegroundShellAgentAction::ReportProcessExit;
    }

    ForegroundShellAgentAction::ObserveProbe
}

#[derive(Debug, Clone, Copy)]
struct ProcessProbeInput {
    current_agent: Option<Agent>,
    suppressed_agent: Option<Agent>,
    foreground_pgid: Option<u32>,
    last_foreground_pgid: Option<u32>,
    has_process_probe: bool,
    acquisition_age: Option<std::time::Duration>,
    pending_foreground_shell_clear: bool,
    pending_restore_probe: bool,
    elapsed_since_process_check: std::time::Duration,
}

fn foreground_group_changed(
    foreground_pgid: Option<u32>,
    last_foreground_pgid: Option<u32>,
) -> bool {
    foreground_pgid != last_foreground_pgid
        && (foreground_pgid.is_some() || last_foreground_pgid.is_some())
}

fn should_skip_process_probe_for_lifecycle_authority(
    full_lifecycle_authority_active: bool,
    input: ProcessProbeInput,
) -> bool {
    full_lifecycle_authority_active
        && !input.pending_foreground_shell_clear
        && input.suppressed_agent.is_none()
        && input.has_process_probe
        && !foreground_group_changed(input.foreground_pgid, input.last_foreground_pgid)
}

fn should_probe_foreground_job(input: ProcessProbeInput) -> bool {
    if input.pending_foreground_shell_clear || input.pending_restore_probe {
        return true;
    }

    let foreground_group_changed =
        foreground_group_changed(input.foreground_pgid, input.last_foreground_pgid);

    if input.suppressed_agent.is_some() {
        return !input.has_process_probe || foreground_group_changed;
    }

    if let Some(acquisition_age) = input.acquisition_age {
        let acquisition_interval = if acquisition_age <= PROCESS_ACQUISITION_FAST_WINDOW {
            PROCESS_ACQUISITION_FAST_RECHECK
        } else {
            PROCESS_ACQUISITION_SLOW_RECHECK
        };
        if acquisition_age <= PROCESS_ACQUISITION_WINDOW
            && input.elapsed_since_process_check >= acquisition_interval
        {
            return true;
        }
    }

    if input.current_agent.is_none() {
        return !input.has_process_probe
            || foreground_group_changed
            || (input.foreground_pgid.is_none()
                && input.elapsed_since_process_check >= PROCESS_RECHECK_MISSING_FOREGROUND_GROUP);
    }

    foreground_group_changed || input.elapsed_since_process_check >= PROCESS_RECHECK_IDENTIFIED
}

fn sync_content_change_acquisition(
    current_agent: Option<Agent>,
    suppressed_agent: Option<Agent>,
    process_group_changed: bool,
    content_changed: bool,
    now: std::time::Instant,
    acquisition_started_at: &mut Option<std::time::Instant>,
    last_content_change_at: &mut Option<std::time::Instant>,
) {
    if current_agent.is_some() || suppressed_agent.is_some() || process_group_changed {
        return;
    }

    if content_changed {
        let should_start = acquisition_started_at.is_none_or(|started| {
            now.duration_since(started) > PROCESS_ACQUISITION_WINDOW
                && last_content_change_at.is_none_or(|last_change| {
                    now.duration_since(last_change) >= PROCESS_ACQUISITION_IDLE_RESET
                })
        });
        if should_start {
            *acquisition_started_at = Some(now);
        }
        *last_content_change_at = Some(now);
        return;
    }

    let Some(acquisition_started) = *acquisition_started_at else {
        return;
    };
    let Some(last_content_change) = *last_content_change_at else {
        return;
    };

    if now.duration_since(acquisition_started) > PROCESS_ACQUISITION_WINDOW
        && now.duration_since(last_content_change) >= PROCESS_ACQUISITION_IDLE_RESET
    {
        *acquisition_started_at = None;
        *last_content_change_at = None;
    }
}

#[derive(Debug, Clone)]
struct ProcessProbeResult {
    process_group_id: Option<u32>,
    foreground_is_pane_shell: bool,
    agent: Option<Agent>,
    process_name: Option<String>,
}

fn agent_hint_for_foreground_job_members(
    job: &crate::platform::ForegroundJob,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<Agent> {
    read_hint(job.process_group_id)
        .or_else(|| agent_hint_for_non_leader_foreground_job_members(job, read_hint))
}

fn agent_hint_for_non_leader_foreground_job_members(
    job: &crate::platform::ForegroundJob,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<Agent> {
    job.processes
        .iter()
        .filter(|process| process.pid != job.process_group_id)
        .find_map(|process| read_hint(process.pid))
}

fn identify_process_group_leader_in_job(
    job: &crate::platform::ForegroundJob,
) -> Option<(Agent, String)> {
    let leader = job
        .processes
        .iter()
        .find(|process| process.pid == job.process_group_id)?;
    let leader_job = crate::platform::ForegroundJob {
        process_group_id: job.process_group_id,
        processes: vec![leader.clone()],
    };
    crate::detect::identify_agent_in_job(&leader_job)
}

fn process_probe_result(
    job: &crate::platform::ForegroundJob,
    pid: u32,
    agent: Agent,
    process_name: String,
) -> ProcessProbeResult {
    ProcessProbeResult {
        process_group_id: Some(job.process_group_id),
        foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
        agent: Some(agent),
        process_name: Some(process_name),
    }
}

fn hinted_process_probe_result(
    job: &crate::platform::ForegroundJob,
    pid: u32,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<ProcessProbeResult> {
    let agent = agent_hint_for_foreground_job_members(job, read_hint)?;
    Some(process_probe_result(
        job,
        pid,
        agent,
        crate::detect::agent_label(agent).to_string(),
    ))
}

fn probe_foreground_process_from_jobs(
    pid: u32,
    foreground_pgid: Option<u32>,
    leader_job: Option<crate::platform::ForegroundJob>,
    foreground_job: impl FnOnce() -> Option<crate::platform::ForegroundJob>,
    read_hint: impl Fn(u32) -> Option<Agent> + Copy,
) -> ProcessProbeResult {
    if let Some(job) = leader_job.as_ref() {
        if let Some(hinted) = hinted_process_probe_result(job, pid, read_hint) {
            return hinted;
        }
        if let Some((agent, process_name)) = crate::detect::identify_agent_in_job(job) {
            return process_probe_result(job, pid, agent, process_name);
        }
    }

    let foreground_job = foreground_job();
    if let Some(job) = foreground_job.as_ref() {
        if let Some(agent) = read_hint(job.process_group_id) {
            return process_probe_result(
                job,
                pid,
                agent,
                crate::detect::agent_label(agent).to_string(),
            );
        }
        if let Some((agent, process_name)) = identify_process_group_leader_in_job(job) {
            return process_probe_result(job, pid, agent, process_name);
        }
        if let Some(agent) = agent_hint_for_non_leader_foreground_job_members(job, read_hint) {
            return process_probe_result(
                job,
                pid,
                agent,
                crate::detect::agent_label(agent).to_string(),
            );
        }

        let identified = crate::detect::identify_agent_in_job(job);
        return ProcessProbeResult {
            process_group_id: Some(job.process_group_id),
            foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
            agent: identified.as_ref().map(|(agent, _)| *agent),
            process_name: identified.map(|(_, process_name)| process_name),
        };
    }

    ProcessProbeResult {
        process_group_id: foreground_pgid,
        foreground_is_pane_shell: false,
        agent: None,
        process_name: None,
    }
}

fn probe_foreground_process(pid: u32, foreground_pgid: Option<u32>) -> ProcessProbeResult {
    probe_foreground_process_from_jobs(
        pid,
        foreground_pgid,
        foreground_pgid.and_then(crate::detect::foreground_group_leader_job),
        || crate::detect::foreground_job(pid),
        crate::platform::process_agent_hint,
    )
}

/// Maps a relayed remote `AgentStatus` (P3's foreground-process signal,
/// relayed over the P1 agent-status stream — see `remote::federation::
/// reducer::RemoteMirror::apply_agent_status`) onto the local `AgentState`
/// vocabulary a pane's detection loop already speaks (P6 requirement 1).
/// `Done` collapses to `Idle` (no local "finished" state — a finished agent
/// reads the same as an idle prompt); `Unknown` passes through unchanged so
/// an ambiguous remote probe never fabricates a stronger claim than the
/// source itself made (S14.1).
/// P6/P9: a relayed `AgentStatus` frame carries the serving host's own
/// agent identity alongside status, so a remote-mirrored pane (no local
/// process to probe, see `spawn_basic_detection_task`'s `pid > 0` gate) can
/// still populate `agent_presence`/`terminal.agent_name` and pass
/// `TerminalState::is_agent_terminal()`. `agent` carries the wire's
/// canonical label string (`crate::detect::agent_label`) rather than
/// `Agent` directly, matching `AgentStatusMessage::agent`'s wire shape;
/// parsed to `Agent` at the one place that consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayedAgentStatus {
    pub(crate) status: AgentStatus,
    pub(crate) agent: Option<String>,
}

fn map_relayed_agent_status(status: AgentStatus) -> AgentState {
    match status {
        AgentStatus::Idle | AgentStatus::Done => AgentState::Idle,
        AgentStatus::Working => AgentState::Working,
        AgentStatus::Blocked => AgentState::Blocked,
        AgentStatus::Unknown => AgentState::Unknown,
    }
}

/// Cadence gate for relayed remote agent-status ingestion (P6 requirement
/// 4 / S12.2). The local per-pane screen-text timer in
/// `spawn_basic_detection_task` already runs unconditionally for every pane
/// (needed regardless of byte source), but the *relay* is additive extra
/// plumbing per remote pane and must not be ported to every backgrounded
/// remote pane — only attended/visible ones stay active. A pane never
/// marked visible here defaults to inactive: relay ingestion is opt-in per
/// visibility, not opt-out.
// Dormant like `RemoteTerminalSourceHandle`/`PaneRuntimeIo::Remote`: no live
// call site constructs this until a future federation caller (P8/P9) wires
// pane visibility into the relay; only this module's own tests do.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct RemoteAgentStatusGate {
    visible: std::collections::HashSet<PaneId>,
}

#[allow(dead_code)]
impl RemoteAgentStatusGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Marks a pane visible/attended (relay active) or hidden (relay
    /// throttled off).
    pub(crate) fn set_visible(&mut self, pane_id: PaneId, visible: bool) {
        if visible {
            self.visible.insert(pane_id);
        } else {
            self.visible.remove(&pane_id);
        }
    }

    pub(crate) fn is_active(&self, pane_id: PaneId) -> bool {
        self.visible.contains(&pane_id)
    }
}

#[cfg(unix)]
fn spawn_basic_detection_task(
    pane_id: PaneId,
    child_pid: Arc<AtomicU32>,
    terminal: Arc<PaneTerminal>,
    detection_content_seq: Arc<AtomicU64>,
    full_lifecycle_authority_active: Arc<AtomicBool>,
    state_events: mpsc::Sender<AppEvent>,
    // P6 requirement 1/2: `Some` only for `spawn_remote`-constructed
    // runtimes (a real local `LocalChild` runtime already has a real
    // process to probe and passes `None`). When present, a relayed
    // `AgentStatus` update is applied directly — never via
    // `probe_foreground_process`, which targets a local PID that simply
    // does not exist for a remote pane.
    mut relayed_status_rx: Option<mpsc::Receiver<RelayedAgentStatus>>,
) -> (
    tokio::task::AbortHandle,
    Arc<Notify>,
    Arc<Mutex<Option<PendingAgentRelease>>>,
) {
    let detect_reset_notify = Arc::new(Notify::new());
    let detect_reset = detect_reset_notify.clone();
    let pending_release = Arc::new(Mutex::new(None));
    let pending_release_for_task = pending_release.clone();

    let handle = tokio::spawn(async move {
        let mut agent_presence = AgentDetectionPresence::from_agent(None);
        let mut state = AgentState::Unknown;
        let mut last_visible_idle = false;
        let mut last_visible_blocker = false;
        let mut last_visible_working = false;
        let mut last_visible_signal_refresh = None;
        let mut last_process_check = std::time::Instant::now();
        let mut last_foreground_pgid = None;
        let mut has_process_probe = false;
        let mut acquisition_started_at = None;
        let mut last_content_change_at = None;
        let mut pending_foreground_shell_clear = false;
        let mut foreground_shell_exit_reported = false;
        let mut release_was_active = false;
        let mut last_detection_text = String::new();
        let mut last_screen_scan_detection_content_seq = None;
        let mut agent_startup_grace_until = None;
        let mut pending_idle = PendingIdleConfirmation::default();

        loop {
            let sleep_duration = if pending_idle.active() {
                AGENT_PENDING_IDLE_RECHECK
            } else {
                std::time::Duration::from_millis(300)
            };
            let relayed_status_recv = async {
                match relayed_status_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                _ = tokio::time::sleep(sleep_duration) => {}
                _ = detect_reset.notified() => {
                    agent_presence = AgentDetectionPresence::from_agent(None);
                    state = AgentState::Unknown;
                    last_visible_idle = false;
                    last_visible_blocker = false;
                    last_visible_working = false;
                    last_visible_signal_refresh = None;
                    last_process_check = std::time::Instant::now();
                    last_foreground_pgid = None;
                    has_process_probe = false;
                    acquisition_started_at = None;
                    last_content_change_at = None;
                    pending_foreground_shell_clear = false;
                    foreground_shell_exit_reported = false;
                    release_was_active = false;
                    last_detection_text.clear();
                    last_screen_scan_detection_content_seq = None;
                    agent_startup_grace_until = None;
                    pending_idle.clear();
                }
                Some(relayed_status) = relayed_status_recv => {
                    // P6 requirement 1/2: the remote's real foreground-
                    // process signal (P3), relayed through P4's reducer —
                    // applied directly, with zero involvement of
                    // `probe_foreground_process`.
                    //
                    // P9 identity fix: the relay also carries the serving
                    // host's own agent identity (`AgentStatusMessage::
                    // agent`) — a remote-mirrored pane has no local process
                    // to probe (`pid` stays 0, `should_check_process` below
                    // is always false), so screen-text detection can never
                    // *identify* an agent for it (it only refines state for
                    // an already-identified one). Apply the relayed
                    // identity directly here, bypassing the process-probe
                    // debounce entirely — the serving host already only
                    // sends a frame on change.
                    let mut identity_changed = false;
                    if let Some(label) = relayed_status.agent.as_deref() {
                        if let Some(identified) = crate::detect::parse_agent_label(label) {
                            if agent_presence.current_agent() != Some(identified) {
                                agent_presence.observe_process_probe(Some(identified));
                                terminal.clear_agent_osc_state();
                                identity_changed = true;
                            }
                        }
                    } else if matches!(relayed_status.status, AgentStatus::Idle | AgentStatus::Done)
                    {
                        // No identified agent AND the remote reports idle/done
                        // (not Working/Blocked, which necessarily implies an
                        // agent is active): the remote agent has most likely
                        // exited and its pane reverted to a plain shell.
                        // A Working/Blocked status paired with `agent: None`
                        // instead means an *old* peer that structurally never
                        // populates this field — never clear on that
                        // combination, or a stale identity would be wiped
                        // out for a still-running remote agent. Route the
                        // idle/done clear through the same debounced
                        // `observe_process_probe(None)` path the local
                        // process-probe loop uses, so a single stray
                        // idle/done frame can't spuriously wipe a real
                        // identity (see `AGENT_MISS_CONFIRMATION_ATTEMPTS`).
                        if agent_presence.observe_process_probe(None) {
                            terminal.clear_agent_osc_state();
                            identity_changed = true;
                        }
                    }
                    let mapped = map_relayed_agent_status(relayed_status.status);
                    // Publish on a status transition OR a fresh identity —
                    // otherwise an identity that arrives without a
                    // simultaneous status change (e.g. the remote was
                    // already `Working` before this pane mounted) would
                    // never reach `terminal.detected_agent`
                    // (`AppEvent::StateChanged` is the only path that sets
                    // it), leaving `is_agent_terminal()` false forever.
                    if mapped != state || identity_changed {
                        state = mapped;
                        last_visible_idle = mapped == AgentState::Idle;
                        last_visible_blocker = mapped == AgentState::Blocked;
                        last_visible_working = mapped == AgentState::Working;
                        publish_state_changed_event(
                            state_events.clone(),
                            pane_id,
                            agent_presence.current_agent(),
                            mapped,
                            last_visible_blocker,
                            last_visible_working,
                            false,
                            std::time::Instant::now(),
                        )
                        .await;
                    }
                    pending_idle.clear();
                    continue;
                }
            }

            let now = std::time::Instant::now();
            let suppressed_agent = active_pending_release(&pending_release_for_task, now);
            if suppressed_agent.is_none() && release_was_active {
                has_process_probe = false;
                acquisition_started_at = None;
                last_content_change_at = None;
            }
            release_was_active = suppressed_agent.is_some();
            let pid = child_pid.load(Ordering::Acquire);
            let mut agent_changed = false;
            let mut agent = agent_presence.current_agent();
            let lifecycle_authority_active =
                full_lifecycle_authority_active.load(Ordering::Acquire);
            let foreground_pgid = (pid > 0)
                .then(|| crate::detect::foreground_process_group_id(pid))
                .flatten();
            let process_group_changed =
                foreground_group_changed(foreground_pgid, last_foreground_pgid);
            let should_check_process = pid > 0 && {
                let process_probe_input = ProcessProbeInput {
                    current_agent: agent,
                    suppressed_agent,
                    foreground_pgid,
                    last_foreground_pgid,
                    has_process_probe,
                    acquisition_age: acquisition_started_at
                        .map(|started| now.duration_since(started)),
                    pending_foreground_shell_clear,
                    pending_restore_probe: false,
                    elapsed_since_process_check: now.duration_since(last_process_check),
                };
                !should_skip_process_probe_for_lifecycle_authority(
                    lifecycle_authority_active,
                    process_probe_input,
                ) && should_probe_foreground_job(process_probe_input)
            };

            if should_check_process {
                last_process_check = now;
                let had_process_probe = has_process_probe;
                has_process_probe = true;
                let probe = probe_foreground_process(pid, foreground_pgid);
                let process_group_id = probe.process_group_id;
                let foreground_is_pane_shell = probe.foreground_is_pane_shell;
                let mut new_agent = probe.agent;
                if let Some(suppressed_agent) = suppressed_agent {
                    if new_agent == Some(suppressed_agent) {
                        new_agent = None;
                    } else if let Ok(mut pending_release) = pending_release_for_task.lock() {
                        *pending_release = None;
                    }
                }
                let previous_agent = agent_presence.current_agent();
                let changed = match foreground_shell_agent_action(
                    previous_agent,
                    new_agent,
                    foreground_is_pane_shell,
                    foreground_shell_exit_reported,
                ) {
                    ForegroundShellAgentAction::ReportProcessExit => {
                        pending_foreground_shell_clear = true;
                        false
                    }
                    ForegroundShellAgentAction::ClearAgent => {
                        pending_foreground_shell_clear = false;
                        foreground_shell_exit_reported = false;
                        agent_presence.clear_current_agent()
                    }
                    ForegroundShellAgentAction::ObserveProbe => {
                        pending_foreground_shell_clear = false;
                        foreground_shell_exit_reported = false;
                        agent_presence.observe_process_probe(new_agent)
                    }
                };
                if new_agent.is_some() {
                    last_foreground_pgid = process_group_id.or(foreground_pgid);
                    acquisition_started_at = None;
                    last_content_change_at = None;
                } else if agent_presence.current_agent().is_none() {
                    last_foreground_pgid = process_group_id.or(foreground_pgid);
                    if had_process_probe && process_group_changed {
                        acquisition_started_at = Some(now);
                    }
                } else {
                    last_foreground_pgid = process_group_id.or(foreground_pgid);
                }
                if changed {
                    agent = agent_presence.current_agent();
                    agent_changed = previous_agent != agent;
                    if agent_changed {
                        pending_idle.clear();
                        last_screen_scan_detection_content_seq = None;
                        // A new foreground agent must not inherit OSC
                        // title/progress evidence from the previous process.
                        terminal.clear_agent_osc_state();
                        if agent.is_some() {
                            agent_startup_grace_until = Some(now + AGENT_STARTUP_GRACE_WINDOW);
                            state = AgentState::Idle;
                            last_visible_idle = true;
                            last_visible_blocker = false;
                            last_visible_working = false;
                            last_visible_signal_refresh = None;
                            publish_state_changed_event(
                                state_events.clone(),
                                pane_id,
                                agent,
                                AgentState::Idle,
                                false,
                                false,
                                false,
                                now,
                            )
                            .await;
                        } else {
                            agent_startup_grace_until = None;
                        }
                    }
                }
            }

            let process_exited = pending_foreground_shell_clear
                && agent.is_some()
                && !foreground_shell_exit_reported;

            if lifecycle_authority_active && !process_exited {
                pending_idle.clear();
                continue;
            }

            if let Some(until) = agent_startup_grace_until {
                if process_exited {
                    agent_startup_grace_until = None;
                    pending_idle.clear();
                } else {
                    if now < until {
                        pending_idle.clear();
                        continue;
                    }
                    agent_startup_grace_until = None;
                    last_screen_scan_detection_content_seq = None;
                    pending_idle.clear();
                    continue;
                }
            }

            let current_detection_content_seq = if agent.is_some() {
                Some(detection_content_seq.load(Ordering::Relaxed))
            } else {
                None
            };
            match decide_detection_screen_read(DetectionScreenReadInput {
                state,
                agent,
                pending_idle_active: pending_idle.active(),
                agent_changed,
                process_exited,
                current_detection_content_seq,
                last_screen_scan_detection_content_seq,
            }) {
                DetectionScreenReadDecision::Read => {}
                DetectionScreenReadDecision::Skip => continue,
            }

            let content = terminal.detection_text();
            last_screen_scan_detection_content_seq = current_detection_content_seq;
            let content_changed = content != last_detection_text;
            last_detection_text.clone_from(&content);
            if !process_exited && crate::detect::should_skip_state_update(agent, &content) {
                pending_idle.clear();
                continue;
            }
            sync_content_change_acquisition(
                agent_presence.current_agent(),
                suppressed_agent,
                process_group_changed,
                content_changed,
                now,
                &mut acquisition_started_at,
                &mut last_content_change_at,
            );

            let osc_title = terminal.agent_osc_title();
            let osc_progress = terminal.agent_osc_progress();
            let Some(screen_detection) = detection_update_for_publish_with_osc(
                agent,
                &content,
                &osc_title,
                &osc_progress,
                process_exited,
            ) else {
                pending_idle.clear();
                continue;
            };
            match decide_screen_detection_publish(
                ScreenDetectionPublishInput {
                    screen_detection,
                    current_state: state,
                    last_visible_idle,
                    last_visible_blocker,
                    last_visible_working,
                    last_visible_signal_refresh,
                    process_exited,
                    agent_changed,
                    now,
                },
                &mut pending_idle,
            ) {
                DetectionPublishDecision::NoPublish => {}
                DetectionPublishDecision::Publish {
                    state: new_state,
                    visible_idle,
                    visible_blocker,
                    visible_working,
                    process_exited: publish_process_exited,
                } => {
                    apply_agent_detection_publish_update(
                        state_events.clone(),
                        pane_id,
                        agent,
                        AgentDetectionPublishUpdate {
                            state: new_state,
                            visible_idle,
                            visible_blocker,
                            visible_working,
                            process_exited: publish_process_exited,
                        },
                        now,
                        &mut state,
                        &mut last_visible_idle,
                        &mut last_visible_blocker,
                        &mut last_visible_working,
                        &mut last_visible_signal_refresh,
                        &mut foreground_shell_exit_reported,
                    )
                    .await;
                }
            }
        }
    });

    (handle.abort_handle(), detect_reset_notify, pending_release)
}

impl AgentDetectionPresence {
    fn from_agent(current_agent: Option<Agent>) -> Self {
        Self {
            current_agent,
            consecutive_misses: 0,
        }
    }

    fn current_agent(&self) -> Option<Agent> {
        self.current_agent
    }

    fn clear_current_agent(&mut self) -> bool {
        if self.current_agent.is_none() {
            self.consecutive_misses = 0;
            return false;
        }
        self.current_agent = None;
        self.consecutive_misses = 0;
        true
    }

    fn observe_process_probe(&mut self, identified_agent: Option<Agent>) -> bool {
        match identified_agent {
            Some(agent) => {
                self.consecutive_misses = 0;
                if Some(agent) == self.current_agent {
                    return false;
                }
                self.current_agent = Some(agent);
                true
            }
            None => {
                if self.current_agent.is_none() {
                    self.consecutive_misses = 0;
                    return false;
                }
                self.consecutive_misses = self.consecutive_misses.saturating_add(1);
                if self.consecutive_misses < AGENT_MISS_CONFIRMATION_ATTEMPTS {
                    return false;
                }
                self.current_agent = None;
                self.consecutive_misses = 0;
                true
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PaneRuntime — PTY, parser, channels, background tasks
// ---------------------------------------------------------------------------

/// PTY runtime for a pane. Owns the terminal, I/O channels, and background tasks.
/// Dropping this shuts down all background tasks and closes the PTY.
pub struct PaneRuntime {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    io: PaneRuntimeIo,
    current_size: Cell<(u16, u16, u32, u32)>,
    child_pid: Arc<AtomicU32>,
    reported_cwd: Arc<Mutex<Option<std::path::PathBuf>>>,
    child_wait_completed: Option<Arc<AtomicBool>>,
    kitty_keyboard_flags: Arc<AtomicU16>,
    detection_content_seq: Arc<AtomicU64>,
    full_lifecycle_authority_active: Arc<AtomicBool>,
    detect_reset_notify: Arc<Notify>,
    pending_release: Arc<Mutex<Option<PendingAgentRelease>>>,
    preserve_processes_on_drop: bool,
    // Task handles for deterministic shutdown
    detect_handle: tokio::task::AbortHandle,
    // Broadcast tee of raw PTY bytes, forked at the `on_read` source (the
    // exact bytes `process_pty_bytes` consumes). Additive: dormant unless a
    // federation channel subscribes (`subscribe_output_bytes`); a `send`
    // with zero subscribers is a cheap no-op and never perturbs the local
    // render path. See `remote::federation::tee`.
    output_tee: broadcast::Sender<Bytes>,
    // P6: sender for relaying a remote's foreground-process-equivalent
    // `AgentStatus` into this pane's detection loop. `Some` only for
    // `spawn_remote`-constructed runtimes (a real local runtime already has
    // a real process to probe); `None` everywhere else. Dormant until a
    // live federation call site (P8/P9) drives it from
    // `RemoteMirror::apply_agent_status`. Read only by
    // `relayed_agent_status_sender()` (dormant, same reason) and this
    // module's own tests, so it's dead code outside `#[cfg(test)]` until
    // then — same precedent as `PaneRuntimeIo::Remote`.
    #[allow(dead_code)]
    relayed_agent_status_tx: Option<mpsc::Sender<RelayedAgentStatus>>,
}

/// Capacity (in messages, not bytes) of each pane's raw-output broadcast
/// tee. Sized generously so a federation consumer reading slightly slower
/// than the PTY produces bytes does not immediately lag; a consumer that
/// falls further behind than this observes `RecvError::Lagged` and must
/// treat it as a gap (never silently missed).
const OUTPUT_TEE_CAPACITY: usize = 4096;

/// Bounded capacity of a `spawn_remote` pane's relayed-agent-status queue
/// (P6). Deliberately small: this carries coarse state transitions
/// (idle/working/blocked), not a byte stream — a slow consumer only ever
/// needs the latest status, not a long backlog.
const RELAYED_AGENT_STATUS_CHANNEL_CAPACITY: usize = 8;

enum PaneRuntimeIo {
    Actor(PtyIoActorHandle),
    /// P5: fed by the federation raw terminal channel instead of a local
    /// PTY. See `RemoteTerminalSourceHandle`'s doc comment for the pinned
    /// lifecycle contract (never spawns/kills a local child, never emits a
    /// local `PaneDied` on reader-exit). Unconstructed outside tests until
    /// P8 wires a real `spawn_remote` call site (dormant, like
    /// `remote::federation::pane_source` itself).
    #[allow(dead_code)]
    Remote(RemoteTerminalSourceHandle),
    #[cfg(test)]
    TestChannel {
        sender: mpsc::Sender<Bytes>,
        resize_tx: watch::Sender<(u16, u16, u32, u32)>,
    },
}

impl PaneRuntimeIo {
    /// True when the pane's authoritative PTY lives on a remote host and
    /// repaints arrive asynchronously over the federation link (see
    /// `remote::federation::pane_source`'s RT-F10 comment). Used to gate
    /// local-only resize recovery heuristics that assume a near-synchronous
    /// same-machine repaint (`PaneTerminal::resize`'s replay-recovery block).
    fn is_remote(&self) -> bool {
        matches!(self, PaneRuntimeIo::Remote(_))
    }

    fn shutdown(&self) {
        match self {
            // Non-`Actor` variants of `PaneRuntimeIo` (`Remote` and the
            // `#[cfg(test)]` `TestChannel`) no-op the `#[cfg(unix)]` handoff
            // registry methods below exactly as `TestChannel` already did —
            // that pattern is the template a non-PTY variant follows
            // (phase-02 req. 5).
            PaneRuntimeIo::Actor(actor) => TerminalSource::shutdown(actor),
            PaneRuntimeIo::Remote(remote) => TerminalSource::shutdown(remote),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => {}
        }
    }

    #[cfg(unix)]
    fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.duplicate_for_handoff(),
            PaneRuntimeIo::Remote(_) => Err(std::io::Error::other(
                "remote-backed panes have no PTY master fd",
            )),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => {
                Err(std::io::Error::other("test runtime has no PTY master fd"))
            }
        }
    }

    #[cfg(unix)]
    fn foreground_process_group_id(&self) -> Option<u32> {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.foreground_process_group_id(),
            PaneRuntimeIo::Remote(_) => None,
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => None,
        }
    }

    #[cfg(unix)]
    fn begin_handoff(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.begin_handoff(timeout),
            PaneRuntimeIo::Remote(_) => Ok(()),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => Ok(()),
        }
    }

    #[cfg(unix)]
    fn set_handoff_paused(&self, paused: bool) -> std::io::Result<()> {
        match self {
            PaneRuntimeIo::Actor(actor) => {
                if paused {
                    actor.begin_handoff(std::time::Duration::from_secs(1))
                } else {
                    actor.rollback_handoff()
                }
            }
            PaneRuntimeIo::Remote(_) => Ok(()),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => Ok(()),
        }
    }

    #[cfg(unix)]
    fn release_after_commit(&self) -> std::io::Result<()> {
        match self {
            PaneRuntimeIo::Actor(actor) => actor.release_after_commit(),
            PaneRuntimeIo::Remote(_) => Ok(()),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => Ok(()),
        }
    }

    fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    ) {
        match self {
            PaneRuntimeIo::Actor(actor) => {
                TerminalSource::resize(
                    actor,
                    rows,
                    cols,
                    cell_width_px,
                    cell_height_px,
                    terminal_responses,
                );
            }
            PaneRuntimeIo::Remote(remote) => {
                TerminalSource::resize(
                    remote,
                    rows,
                    cols,
                    cell_width_px,
                    cell_height_px,
                    terminal_responses,
                );
            }
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { resize_tx, .. } => {
                let _ = resize_tx.send((rows, cols, cell_width_px, cell_height_px));
            }
        }
    }

    #[cfg(unix)]
    fn nudge_child_redraw_after_handoff(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) {
        match self {
            PaneRuntimeIo::Actor(actor) => {
                actor.nudge_child_redraw_after_handoff(rows, cols, cell_width_px, cell_height_px);
            }
            PaneRuntimeIo::Remote(_) => {}
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => {}
        }
    }

    async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        match self {
            PaneRuntimeIo::Actor(actor) => TerminalSource::write_user_input(actor, bytes).await,
            PaneRuntimeIo::Remote(remote) => TerminalSource::write_user_input(remote, bytes).await,
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { sender, .. } => sender.send(bytes).await,
        }
    }

    fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        match self {
            PaneRuntimeIo::Actor(actor) => TerminalSource::try_write_user_input(actor, bytes),
            PaneRuntimeIo::Remote(remote) => TerminalSource::try_write_user_input(remote, bytes),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { sender, .. } => sender.try_send(bytes),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

impl Drop for PaneRuntime {
    fn drop(&mut self) {
        // Abort detection task immediately and terminate the owned session.
        // The PTY actor shuts down before the process/session policy runs.
        self.detect_handle.abort();
        self.io.shutdown();
        if !self.preserve_processes_on_drop {
            shutdown_pane_processes(
                self.pane_id,
                self.child_pid.load(Ordering::Acquire),
                self.child_wait_completed.as_deref(),
            );
        }
    }
}

fn process_alive_for_shutdown(
    pid: u32,
    child_pid: u32,
    child_wait_completed: bool,
    process_exists: impl FnOnce(u32) -> bool,
) -> bool {
    if pid == child_pid && child_wait_completed {
        return false;
    }
    process_exists(pid)
}

fn wait_for_processes_to_exit(
    pids: &[u32],
    child_pid: u32,
    child_wait_completed: Option<&AtomicBool>,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let child_wait_completed =
            child_wait_completed.is_some_and(|flag| flag.load(Ordering::Acquire));
        if pids.iter().all(|pid| {
            !process_alive_for_shutdown(
                *pid,
                child_pid,
                child_wait_completed,
                crate::platform::process_exists,
            )
        }) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn shutdown_pane_processes(
    pane_id: PaneId,
    child_pid: u32,
    child_wait_completed: Option<&AtomicBool>,
) {
    if child_pid == 0 {
        return;
    }

    let mut pids = crate::platform::session_processes(child_pid);
    if pids.is_empty() {
        pids.push(child_pid);
    }
    pids.sort_unstable();
    pids.dedup();

    for (signal, grace) in [
        (
            crate::platform::Signal::Hangup,
            std::time::Duration::from_millis(250),
        ),
        (
            crate::platform::Signal::Terminate,
            std::time::Duration::from_millis(250),
        ),
        (
            crate::platform::Signal::Kill,
            std::time::Duration::from_millis(250),
        ),
    ] {
        crate::platform::signal_processes(&pids, signal);
        if wait_for_processes_to_exit(&pids, child_pid, child_wait_completed, grace) {
            info!(
                pane = pane_id.raw(),
                pid = child_pid,
                ?signal,
                "pane session terminated"
            );
            return;
        }
    }

    warn!(
        pane = pane_id.raw(),
        pid = child_pid,
        pids = ?pids,
        "pane session still alive after forced shutdown"
    );
}

#[cfg(unix)]
fn truncate_handoff_history(history: String, max_bytes: usize) -> String {
    if history.len() <= max_bytes {
        return history;
    }
    let mut start = history.len().saturating_sub(max_bytes);
    while !history.is_char_boundary(start) {
        start += 1;
    }
    let Some(newline_offset) = history[start..].find('\n') else {
        return String::new();
    };
    start += newline_offset + 1;
    history[start..].to_owned()
}

fn pane_shell(configured_shell: &str) -> String {
    pane_shell_from(configured_shell, std::env::var("SHELL").ok())
}

fn pane_shell_from(configured_shell: &str, env_shell: Option<String>) -> String {
    let configured_shell = configured_shell.trim();
    if !configured_shell.is_empty() {
        return configured_shell.to_string();
    }

    #[cfg(windows)]
    {
        let _ = env_shell;
        default_pane_shell()
    }

    #[cfg(not(windows))]
    env_shell
        .map(|shell| shell.trim().to_string())
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(default_pane_shell)
}

#[cfg(windows)]
fn default_pane_shell() -> String {
    "powershell.exe".into()
}

#[cfg(not(windows))]
fn default_pane_shell() -> String {
    "/bin/sh".into()
}

#[derive(Clone, Copy)]
pub(crate) struct PaneShellConfig<'a> {
    pub(crate) default_shell: &'a str,
    pub(crate) mode: crate::config::ShellModeConfig,
}

impl<'a> PaneShellConfig<'a> {
    pub(crate) fn new(default_shell: &'a str, mode: crate::config::ShellModeConfig) -> Self {
        Self {
            default_shell,
            mode,
        }
    }
}

/// Target platform for shell launch policy. Parameterized (instead of raw
/// `cfg!` checks at each decision point) so every branch stays testable on
/// every host platform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShellLaunchTarget {
    Windows,
    Macos,
    OtherUnix,
}

impl ShellLaunchTarget {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::OtherUnix
        }
    }
}

fn shell_mode_uses_login_shell(
    mode: crate::config::ShellModeConfig,
    target: ShellLaunchTarget,
) -> bool {
    match mode {
        crate::config::ShellModeConfig::Auto => target == ShellLaunchTarget::Macos,
        crate::config::ShellModeConfig::Login => true,
        crate::config::ShellModeConfig::NonLogin => false,
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_shell_for_login_mode(shell: &str) -> io::Result<String> {
    if shell.contains(std::path::MAIN_SEPARATOR) {
        let path = Path::new(shell);
        return is_executable_file(path)
            .then(|| shell.to_string())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("login shell {shell:?} is not executable"),
                )
            });
    }

    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(shell))
                .find(|candidate| is_executable_file(candidate))
        })
        .and_then(|path| path.into_os_string().into_string().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("login shell {shell:?} was not found on PATH"),
            )
        })
}

/// Sourced via `-NoExit -Command` when launching PowerShell on Windows. It
/// wraps whatever `prompt` function the user's profile left behind so each
/// prompt render appends the cwd as OSC 9;9 — the sequence Windows Terminal
/// and ConEmu standardized for shell integration. PowerShell never updates
/// its Win32 process cwd on `Set-Location`, so prompt-time reporting is the
/// only reliable cwd source on Windows.
///
/// The snippet must not contain double quotes: powershell.exe parses its
/// command line with its own rules that disagree with the ArgvQuote escaping
/// portable-pty applies, and embedded `\"` sequences get corrupted in
/// transit. Single-quoted strings and `[char]` codes keep the round-trip
/// byte-exact, and the OSC 9;9 payload is emitted unquoted (the original
/// ConEmu form, which the cwd tracker accepts).
///
/// The original prompt must be invoked before any other statement in the
/// wrapper: anything that runs first resets `$?`, so a status-aware user
/// prompt would show success after a failed command (verified on 5.1).
pub(crate) const WINDOWS_POWERSHELL_SHELL_INTEGRATION_COMMAND: &str = r"if ($null -eq $global:__HerdrOriginalPrompt) { $global:__HerdrOriginalPrompt = $function:prompt; function global:prompt { $out = @(& $global:__HerdrOriginalPrompt) -join ' '; $loc = $ExecutionContext.SessionState.Path.CurrentLocation; if ($loc.Provider.Name -eq 'FileSystem') { $esc = [string][char]27; $out += $esc + ']9;9;' + $loc.ProviderPath + $esc + '\' }; $out } }";

fn pane_shell_command_builder_for_target(
    shell_config: PaneShellConfig<'_>,
    target: ShellLaunchTarget,
) -> io::Result<CommandBuilder> {
    let shell = pane_shell(shell_config.default_shell);
    if shell_mode_uses_login_shell(shell_config.mode, target) {
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env("SHELL", resolve_shell_for_login_mode(&shell)?);
        Ok(cmd)
    } else {
        let mut cmd = CommandBuilder::new(&shell);
        if uses_windows_powershell_pane_shell_for_target(shell_config, target) {
            cmd.args([
                "-NoExit",
                "-Command",
                WINDOWS_POWERSHELL_SHELL_INTEGRATION_COMMAND,
            ]);
        }
        Ok(cmd)
    }
}

fn pane_shell_command_builder(shell_config: PaneShellConfig<'_>) -> io::Result<CommandBuilder> {
    pane_shell_command_builder_for_target(shell_config, ShellLaunchTarget::current())
}

/// True when panes launch an interactive PowerShell directly on Windows.
/// Gates the prompt-based cwd reporting pipeline and the agent-exit shell
/// respawn recovery.
pub(crate) fn uses_windows_powershell_pane_shell(shell_config: PaneShellConfig<'_>) -> bool {
    uses_windows_powershell_pane_shell_for_target(shell_config, ShellLaunchTarget::current())
}

fn uses_windows_powershell_pane_shell_for_target(
    shell_config: PaneShellConfig<'_>,
    target: ShellLaunchTarget,
) -> bool {
    target == ShellLaunchTarget::Windows
        && !shell_mode_uses_login_shell(shell_config.mode, target)
        && is_powershell_shell(&pane_shell(shell_config.default_shell))
}

fn is_powershell_shell(shell: &str) -> bool {
    // Split on both separators by hand: `Path::file_name` only treats `\` as
    // a separator on Windows hosts, and this predicate must evaluate Windows
    // shell paths correctly from tests on any host.
    let name = shell
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    )
}

fn usable_reported_cwd(cwd: std::path::PathBuf) -> Option<std::path::PathBuf> {
    (cwd.is_absolute() && cwd.is_dir()).then_some(cwd)
}

fn publish_reported_cwd(
    pane_id: PaneId,
    cwd: std::path::PathBuf,
    reported_cwd: &Arc<Mutex<Option<std::path::PathBuf>>>,
    events: &mpsc::Sender<AppEvent>,
) {
    let Some(cwd) = usable_reported_cwd(cwd) else {
        return;
    };
    if let Ok(mut current) = reported_cwd.lock() {
        if current.as_ref() == Some(&cwd) {
            return;
        }
        *current = Some(cwd.clone());
    }
    if let Err(err) = events.try_send(AppEvent::TerminalCwdReported { pane_id, cwd }) {
        warn!(
            pane = pane_id.raw(),
            err = %err,
            "failed to send terminal cwd report"
        );
    }
}

impl PaneRuntime {
    pub fn shutdown(mut self) {
        self.detect_handle.abort();
        self.io.shutdown();
        shutdown_pane_processes(
            self.pane_id,
            self.child_pid.load(Ordering::Acquire),
            self.child_wait_completed.as_deref(),
        );
        self.preserve_processes_on_drop = true;
    }

    #[cfg(unix)]
    pub fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        self.io.duplicate_handoff_fd()
    }

    #[cfg(unix)]
    pub fn preserve_for_handoff(mut self) {
        if let Err(err) = self.io.release_after_commit() {
            warn!(
                pane = self.pane_id.raw(),
                err = %err,
                "failed to release PTY actor after handoff commit; dropping runtime will still close the actor handle"
            );
        }
        self.detect_handle.abort();
        self.preserve_processes_on_drop = true;
    }

    #[cfg(unix)]
    pub fn assume_handoff_ownership(&mut self) {
        self.preserve_processes_on_drop = false;
    }

    #[cfg(unix)]
    pub fn set_handoff_reader_paused(&self, paused: bool) {
        if let Err(err) = self.io.set_handoff_paused(paused) {
            warn!(
                pane = self.pane_id.raw(),
                err = %err,
                paused,
                "failed to update PTY actor handoff pause state"
            );
        }
    }

    #[cfg(unix)]
    pub fn pause_handoff_reader(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        self.io.begin_handoff(timeout)
    }

    #[cfg(unix)]
    pub fn handoff_runtime_state(
        &self,
        pane_id: u32,
    ) -> crate::handoff_runtime::HandoffRuntimeState {
        let child_pid = self.child_pid.load(Ordering::Acquire);
        let (rows, cols, cell_width_px, cell_height_px) = self.current_size.get();
        crate::handoff_runtime::HandoffRuntimeState {
            pane_id,
            child_pid,
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            keyboard_protocol_flags: match self.keyboard_protocol() {
                crate::input::KeyboardProtocol::Legacy => 0,
                crate::input::KeyboardProtocol::Kitty { flags } => flags,
            },
            keyboard_protocol_ansi: self.terminal.kitty_keyboard_state_ansi(),
            input_state: self.input_state(),
            terminal_title: self.terminal_title(),
            initial_history_ansi: None,
        }
    }

    #[cfg(unix)]
    pub fn handoff_history_ansi(&self) -> Option<String> {
        if self
            .terminal
            .input_state()
            .is_some_and(|input_state| input_state.alternate_screen)
        {
            return None;
        }
        self.snapshot_history().map(|history| {
            truncate_handoff_history(history, crate::server::handoff::MAX_REPLAY_BYTES_PER_PANE)
        })
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        self.terminal.apply_host_terminal_theme(theme);
    }

    pub fn spawn(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        Self::spawn_with_initial_history(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            launch_env,
            None,
            events,
            render_notify,
            render_dirty,
        )
    }

    // Runtime construction needs to thread PTY size, environment, theme, and render hooks together.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_with_initial_history(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        initial_history_ansi: Option<&str>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let windows_powershell_prompt_cwd_reporting =
            uses_windows_powershell_pane_shell(shell_config);
        let mut cmd = pane_shell_command_builder(shell_config)?;
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, launch_env);
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
            cmd,
            "failed to spawn shell",
            SpawnInitialState {
                detected_agent: None,
                history_ansi: initial_history_ansi,
                windows_powershell_prompt_cwd_reporting,
            },
        )
    }

    /// P5: a remote-backed pane whose bytes arrive over the federation raw
    /// terminal channel instead of a local PTY. Builds a real ghostty
    /// terminal (`Pane::render` is source-agnostic — same grid, same
    /// renderer) fed by the SAME `on_read` -> `process_pty_bytes` pathway
    /// every local pane uses; never spawns or waits on a local child
    /// process. `PaneRuntimeIo::Remote`, `preserve_processes_on_drop: true`,
    /// and `child_pid` pinned at 0 for the runtime's whole lifetime (see
    /// `shutdown_pane_processes`'s `child_pid == 0` early return) all
    /// enforce this together.
    ///
    /// `initial_history_ansi` seeds scrollback exactly like
    /// `spawn_with_initial_history` (RT-F6): the caller's `output_rx` is
    /// expected to carry the channel's replayed history bytes before any
    /// live bytes (see `remote::federation::client::TerminalChannelRouter`),
    /// so hydrate order falls out of wire order rather than needing a
    /// second seeding path here.
    ///
    /// Dormant outside tests until P8 wires a real call site (no CLI switch
    /// yet, per the phase's shippability note).
    #[allow(clippy::too_many_arguments, dead_code)]
    pub(crate) fn spawn_remote(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        initial_history_ansi: Option<&str>,
        terminal_id: String,
        mount_generation: u64,
        out_tx: mpsc::UnboundedSender<FederationMessage>,
        output_rx: mpsc::Receiver<Bytes>,
        clipboard_tx: mpsc::UnboundedSender<ClipboardMessage>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let (response_tx, _response_rx) = mpsc::channel::<Bytes>(1);
        let mut terminal = crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        terminal
            .enable_grapheme_cluster_mode()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if crate::kitty_graphics::is_enabled() {
            terminal
                .enable_kitty_graphics()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        let pane_terminal = GhosttyPaneTerminal::new(terminal, response_tx.clone())?;
        pane_terminal.apply_host_terminal_theme(host_terminal_theme);
        if let Some(ansi) = initial_history_ansi {
            pane_terminal.seed_history_ansi(ansi);
        }
        let terminal = Arc::new(PaneTerminal::new(pane_terminal));
        // No local child ever exists for a remote-backed pane: `child_pid`
        // stays 0 for this runtime's whole lifetime. `spawn_basic_detection_task`
        // already treats `pid == 0` as "no process to probe"
        // (`should_check_process = pid > 0 && ...`), degrading gracefully to
        // source-agnostic screen-text detection — the P6 behavior this
        // phase's context calls out ("suppress the local process-table
        // probe for remote panes") is already today's behavior for `pid ==
        // 0`, not new code added here.
        let child_pid = Arc::new(AtomicU32::new(0));
        let reported_cwd = Arc::new(Mutex::new(None));
        let detection_content_seq = Arc::new(AtomicU64::new(0));
        let (output_tee, _) = broadcast::channel::<Bytes>(OUTPUT_TEE_CAPACITY);

        let io = {
            let terminal = terminal.clone();
            let response_writer = response_tx.clone();
            let output_tee = output_tee.clone();
            let render_notify = render_notify.clone();
            let render_dirty = render_dirty.clone();
            let detection_content_seq = detection_content_seq.clone();
            let read_events = events.clone();
            let reported_cwd = reported_cwd.clone();
            let rt = tokio::runtime::Handle::current();
            let delay_rt = rt.clone();
            let on_read = Box::new(move |bytes: &[u8]| {
                // Remote panes never have a local shell pid to report.
                let result = terminal.process_pty_bytes(pane_id, 0, bytes, &response_writer);
                observe_detection_content_change(bytes, &detection_content_seq);
                // Fork the exact bytes just consumed to any federation
                // subscribers (dormant unless one subscribes; see
                // `remote::federation::tee`), same as every local `on_read`.
                let _ = output_tee.send(Bytes::copy_from_slice(bytes));
                if result.request_render && !render_dirty.swap(true, Ordering::AcqRel) {
                    render_notify.notify_one();
                }
                if let Some(delay) = result.render_delay {
                    let render_notify = render_notify.clone();
                    let render_dirty = render_dirty.clone();
                    delay_rt.spawn(async move {
                        tokio::time::sleep(delay).await;
                        if !render_dirty.swap(true, Ordering::AcqRel) {
                            render_notify.notify_one();
                        }
                    });
                }
                if let Some(cwd) = result.reported_cwd.clone() {
                    publish_reported_cwd(pane_id, cwd, &reported_cwd, &read_events);
                }
                // RT-F7: remote-origin OSC 52 clipboard writes are carried
                // on the origin-tagged federation Clipboard channel rather
                // than the local `AppEvent::ClipboardWrite` path every local
                // pane uses — this guarantees the write is never silently
                // dropped; routing it into local clipboard policy with
                // origin propagation is P7 scope (deviation logged in
                // implementation-notes.md: P5 does not own `events.rs`).
                for content in result.clipboard_writes {
                    let _ = clipboard_tx.send(ClipboardMessage {
                        origin_tag: "remote".to_string(),
                        payload: content,
                    });
                }
                // Any `terminal_responses` this mirror emulator computed
                // (e.g. a synthesized cursor-position report) have no
                // destination here — see `RemoteTerminalSourceHandle::resize`
                // for the same disposition applied to resize-triggered
                // responses. The remote host's own real terminal already
                // owns the responder role for its shell.
            });
            PaneRuntimeIo::Remote(RemoteTerminalSourceHandle::spawn(
                RemoteTerminalSourceConfig {
                    terminal_id,
                    mount_generation,
                    out_tx,
                    output_rx,
                    on_read,
                },
            ))
        };

        let full_lifecycle_authority_active = Arc::new(AtomicBool::new(false));
        // P6: relayed-agent-status channel — `None` for every
        // locally-spawned runtime, `Some` only here. Actively driven in
        // production by `drive_mount_channel` -> `router.route_agent_status`
        // feeding this pane's `RemoteMirror::apply_agent_status`, whose
        // output lands on `relayed_agent_status_tx` and is consumed by the
        // detection loop's `relayed_status_recv` branch above.
        let (relayed_agent_status_tx, relayed_agent_status_rx) =
            mpsc::channel::<RelayedAgentStatus>(RELAYED_AGENT_STATUS_CHANNEL_CAPACITY);
        let (detect_handle, detect_reset_notify, pending_release) = spawn_basic_detection_task(
            pane_id,
            child_pid.clone(),
            terminal.clone(),
            detection_content_seq.clone(),
            full_lifecycle_authority_active.clone(),
            events,
            Some(relayed_agent_status_rx),
        );

        Ok(Self {
            pane_id,
            terminal,
            io,
            current_size: Cell::new((rows, cols, 0, 0)),
            child_pid,
            reported_cwd,
            child_wait_completed: None,
            kitty_keyboard_flags: Arc::new(AtomicU16::new(0)),
            detection_content_seq,
            full_lifecycle_authority_active,
            detect_reset_notify,
            pending_release,
            preserve_processes_on_drop: true,
            detect_handle,
            output_tee,
            relayed_agent_status_tx: Some(relayed_agent_status_tx),
        })
    }

    // Runtime construction needs to thread PTY size, environment, theme, and render hooks together.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_shell_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        command: &str,
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let mut cmd = crate::platform::pane_custom_command_pty_builder(command);
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, launch_env);
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
            cmd,
            "failed to spawn command pane",
            SpawnInitialState::default(),
        )
    }

    pub fn spawn_argv_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        argv: &[String],
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let Some((program, args)) = argv.split_first() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "argv must not be empty",
            ));
        };
        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, launch_env);
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
            cmd,
            "failed to spawn argv command pane",
            SpawnInitialState::default(),
        )
    }

    #[cfg(unix)]
    pub fn from_handoff_fd(
        import: crate::handoff_runtime::ImportedHandoffRuntime,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let crate::handoff_runtime::ImportedHandoffRuntime { master_fd, state } = import;
        let crate::handoff_runtime::HandoffRuntimeState {
            pane_id,
            child_pid,
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            keyboard_protocol_flags,
            keyboard_protocol_ansi,
            input_state,
            terminal_title,
            initial_history_ansi,
        } = state;
        let pane_id = PaneId::from_raw(pane_id);
        use std::os::fd::FromRawFd;

        let master_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(master_fd) };

        let (response_tx, _response_rx) = mpsc::channel::<Bytes>(1);
        let mut terminal = crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        terminal
            .enable_grapheme_cluster_mode()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if crate::kitty_graphics::is_enabled() {
            terminal
                .enable_kitty_graphics()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        let pane_terminal = GhosttyPaneTerminal::new(terminal, response_tx.clone())?;
        pane_terminal.apply_host_terminal_theme(host_terminal_theme);
        pane_terminal.seed_terminal_title(terminal_title);
        if let Some(input_state) = input_state {
            pane_terminal.seed_handoff_input_state(input_state);
        }
        if let Some(ansi) = keyboard_protocol_ansi.as_deref() {
            pane_terminal.seed_keyboard_protocol_ansi(ansi);
        } else {
            pane_terminal.seed_keyboard_protocol_flags(keyboard_protocol_flags);
        }
        if let Some(ansi) = initial_history_ansi.as_deref() {
            pane_terminal.seed_history_ansi(ansi);
        }
        let terminal = Arc::new(PaneTerminal::new(pane_terminal));
        let child_pid = Arc::new(AtomicU32::new(child_pid));
        let reported_cwd = Arc::new(Mutex::new(None));
        let kitty_keyboard_flags = Arc::new(AtomicU16::new(keyboard_protocol_flags));
        let detection_content_seq = Arc::new(AtomicU64::new(0));
        let (output_tee, _) = broadcast::channel::<Bytes>(OUTPUT_TEE_CAPACITY);

        let io = {
            let terminal = terminal.clone();
            let response_writer = response_tx.clone();
            let output_tee = output_tee.clone();
            let render_notify = render_notify.clone();
            let render_dirty = render_dirty.clone();
            let detection_content_seq = detection_content_seq.clone();
            let child_pid = child_pid.clone();
            let read_events = events.clone();
            let reported_cwd = reported_cwd.clone();
            let rt = tokio::runtime::Handle::current();
            let delay_rt = rt.clone();
            let on_read = Box::new(move |bytes: &[u8]| {
                let shell_pid = child_pid.load(Ordering::Acquire);
                let result =
                    terminal.process_pty_bytes(pane_id, shell_pid, bytes, &response_writer);
                observe_detection_content_change(bytes, &detection_content_seq);
                // Fork the exact bytes just consumed to any federation
                // subscribers. `send` errors when there are zero
                // subscribers (the common case) — ignored, no perturbation
                // of the local render path either way.
                let _ = output_tee.send(Bytes::copy_from_slice(bytes));
                if result.request_render && !render_dirty.swap(true, Ordering::AcqRel) {
                    render_notify.notify_one();
                }
                if let Some(delay) = result.render_delay {
                    let render_notify = render_notify.clone();
                    let render_dirty = render_dirty.clone();
                    delay_rt.spawn(async move {
                        tokio::time::sleep(delay).await;
                        if !render_dirty.swap(true, Ordering::AcqRel) {
                            render_notify.notify_one();
                        }
                    });
                }
                if let Some(cwd) = result.reported_cwd.clone() {
                    publish_reported_cwd(pane_id, cwd, &reported_cwd, &read_events);
                }
                for content in result.clipboard_writes {
                    if let Err(err) = read_events.try_send(AppEvent::ClipboardWrite { content }) {
                        warn!(
                            pane = pane_id.raw(),
                            err = %err,
                            "failed to queue OSC 52 clipboard write"
                        );
                    }
                }
                PtyReadResult {
                    terminal_responses: result.terminal_responses,
                }
            });
            let exit_events = events.clone();
            let on_reader_exit = Box::new(move || {
                let _ = rt.block_on(exit_events.send(AppEvent::PaneDied { pane_id }));
                debug!(pane = pane_id.raw(), "handoff PTY actor exiting");
            });
            PaneRuntimeIo::Actor(LocalChild::spawn(PtyIoActorConfig {
                pane_id: pane_id.raw(),
                master_fd,
                initially_quiesced: true,
                on_read,
                on_reader_exit: Some(on_reader_exit),
            })?)
        };

        let full_lifecycle_authority_active = Arc::new(AtomicBool::new(false));
        let (detect_handle, detect_reset_notify, pending_release) = spawn_basic_detection_task(
            pane_id,
            child_pid.clone(),
            terminal.clone(),
            detection_content_seq.clone(),
            full_lifecycle_authority_active.clone(),
            events,
            // Local, `LocalChild`-backed runtime: a real process exists to
            // probe; no relay input.
            None,
        );

        Ok(Self {
            pane_id,
            terminal,
            io,
            current_size: Cell::new((rows, cols, cell_width_px, cell_height_px)),
            child_pid,
            reported_cwd,
            child_wait_completed: None,
            kitty_keyboard_flags,
            detection_content_seq,
            full_lifecycle_authority_active,
            detect_reset_notify,
            pending_release,
            preserve_processes_on_drop: true,
            detect_handle,
            output_tee,
            relayed_agent_status_tx: None,
        })
    }

    fn spawn_command_builder(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        cmd: CommandBuilder,
        spawn_error_message: &'static str,
        initial_state: SpawnInitialState<'_>,
    ) -> std::io::Result<Self> {
        crate::logging::pane_spawn_started(pane_id.raw(), rows, cols, scrollback_limit_bytes);

        let (response_tx, _response_rx) = mpsc::channel::<Bytes>(1);
        let mut terminal = crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        terminal
            .enable_grapheme_cluster_mode()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if crate::kitty_graphics::is_enabled() {
            terminal
                .enable_kitty_graphics()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        let pane_terminal = GhosttyPaneTerminal::new(terminal, response_tx.clone())?;
        pane_terminal.apply_host_terminal_theme(host_terminal_theme);
        pane_terminal.set_windows_powershell_prompt_cwd_reporting(
            initial_state.windows_powershell_prompt_cwd_reporting,
        );
        if let Some(ansi) = initial_state.history_ansi {
            pane_terminal.seed_history_ansi(ansi);
        }
        let terminal = Arc::new(PaneTerminal::new(pane_terminal));
        let kitty_keyboard_flags = Arc::new(AtomicU16::new(0));

        let spawned = crate::pty::backend::spawn_with_portable_pty(rows, cols, cmd)
            .inspect_err(|err| error!(pane = pane_id.raw(), err = %err, "{spawn_error_message}"))?;

        // --- Child watcher task ---
        let child_pid = Arc::new(AtomicU32::new(0));
        let reported_cwd = Arc::new(Mutex::new(None));
        let child_wait_completed = Arc::new(AtomicBool::new(false));
        let detection_content_seq = Arc::new(AtomicU64::new(0));
        let full_lifecycle_authority_active = Arc::new(AtomicBool::new(false));
        let (output_tee, _) = broadcast::channel::<Bytes>(OUTPUT_TEE_CAPACITY);
        {
            let child_pid = child_pid.clone();
            let child_wait_completed = child_wait_completed.clone();
            let events = events.clone();
            let rt = tokio::runtime::Handle::current();
            let mut child = spawned.child;
            if let Some(pid) = child.process_id() {
                child_pid.store(pid, Ordering::Release);
                crate::logging::pane_spawned(pane_id.raw(), pid);
            }
            tokio::task::spawn_blocking(move || {
                match child.wait() {
                    Ok(status) => {
                        let status_text = format!("{status:?}");
                        crate::logging::pane_exited(pane_id.raw(), &status_text);
                    }
                    Err(e) => crate::logging::pane_exit_failed(pane_id.raw(), &e.to_string()),
                }
                child_wait_completed.store(true, Ordering::Release);
                // Use blocking send — PaneDied is critical, must not be dropped
                if let Err(e) = rt.block_on(events.send(AppEvent::PaneDied { pane_id })) {
                    error!(pane = pane_id.raw(), err = %e, "failed to send PaneDied event");
                }
            });
        }

        let io = {
            let terminal = terminal.clone();
            let response_writer = response_tx.clone();
            let render_notify = render_notify.clone();
            let render_dirty = render_dirty.clone();
            let detection_content_seq = detection_content_seq.clone();
            let child_pid = child_pid.clone();
            let events = events.clone();
            let reported_cwd = reported_cwd.clone();
            let rt = tokio::runtime::Handle::current();
            let output_tee = output_tee.clone();
            let on_read = Box::new(move |bytes: &[u8]| {
                let shell_pid = child_pid.load(Ordering::Acquire);
                let result =
                    terminal.process_pty_bytes(pane_id, shell_pid, bytes, &response_writer);
                observe_detection_content_change(bytes, &detection_content_seq);
                // See the sibling `from_handoff_fd` on_read: forks raw bytes
                // to any federation subscribers, no-op when none exist.
                let _ = output_tee.send(Bytes::copy_from_slice(bytes));
                if result.request_render && !render_dirty.swap(true, Ordering::AcqRel) {
                    render_notify.notify_one();
                }
                if let Some(delay) = result.render_delay {
                    let render_notify = render_notify.clone();
                    let render_dirty = render_dirty.clone();
                    rt.spawn(async move {
                        tokio::time::sleep(delay).await;
                        if !render_dirty.swap(true, Ordering::AcqRel) {
                            render_notify.notify_one();
                        }
                    });
                }
                if let Some(cwd) = result.reported_cwd.clone() {
                    publish_reported_cwd(pane_id, cwd, &reported_cwd, &events);
                }
                for content in result.clipboard_writes {
                    if let Err(err) = events.try_send(AppEvent::ClipboardWrite { content }) {
                        warn!(
                            pane = pane_id.raw(),
                            err = %err,
                            "failed to send OSC 52 clipboard write"
                        );
                    }
                }
                PtyReadResult {
                    terminal_responses: result.terminal_responses,
                }
            });
            PaneRuntimeIo::Actor(LocalChild::spawn(PtyIoActorConfig {
                pane_id: pane_id.raw(),
                #[cfg(unix)]
                master_fd: spawned.master_fd,
                #[cfg(windows)]
                master: spawned.master,
                initially_quiesced: false,
                on_read,
                on_reader_exit: None,
            })?)
        };

        // --- Detection task ---
        let (detect_handle, detect_reset_notify, pending_release) = {
            use crate::detect;
            use std::time::{Duration, Instant};

            const TICK_UNIDENTIFIED: Duration = Duration::from_millis(500);
            const TICK_IDENTIFIED: Duration = Duration::from_millis(300);
            const TICK_PENDING_RELEASE: Duration = Duration::from_millis(50);

            let child_pid = child_pid.clone();
            let terminal = terminal.clone();
            let state_events = events.clone();
            let detection_content_seq = detection_content_seq.clone();
            let full_lifecycle_authority_active_for_task = full_lifecycle_authority_active.clone();
            let render_notify = render_notify.clone();
            let render_dirty = render_dirty.clone();
            let detect_reset_notify = Arc::new(Notify::new());
            let detect_reset = detect_reset_notify.clone();
            let pending_release = Arc::new(Mutex::new(None));
            let pending_release_for_task = pending_release.clone();

            let handle = tokio::spawn(async move {
                let mut agent_presence =
                    AgentDetectionPresence::from_agent(initial_state.detected_agent);
                let mut state = AgentState::Idle;
                let mut last_visible_idle = initial_state.detected_agent.is_some();
                let mut last_process_check = Instant::now();
                let mut last_foreground_pgid = None;
                let mut has_process_probe = false;
                let mut acquisition_started_at = None;
                let mut last_content_change_at = None;
                let mut pending_foreground_shell_clear = false;
                let mut foreground_shell_exit_reported = false;
                let mut release_was_active = false;
                let mut pending_restore_probe = initial_state.detected_agent.is_some();
                let mut last_visible_blocker = false;
                let mut last_visible_working = false;
                let mut last_visible_signal_refresh = None;
                let mut last_detection_text = String::new();
                let mut last_screen_scan_detection_content_seq = None;
                let mut agent_startup_grace_until = None;
                let mut pending_idle = PendingIdleConfirmation::default();

                tokio::time::sleep(Duration::from_millis(50)).await;

                loop {
                    let now_for_tick = Instant::now();
                    let tick = if active_pending_release(&pending_release_for_task, now_for_tick)
                        .is_some()
                        || terminal.has_transient_default_color_override()
                    {
                        TICK_PENDING_RELEASE
                    } else if pending_idle.active() {
                        AGENT_PENDING_IDLE_RECHECK
                    } else if agent_presence.current_agent().is_none() {
                        TICK_UNIDENTIFIED
                    } else {
                        TICK_IDENTIFIED
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(tick) => {}
                        _ = detect_reset.notified() => {
                            agent_presence = AgentDetectionPresence::from_agent(None);
                            state = AgentState::Unknown;
                            last_visible_idle = false;
                            last_foreground_pgid = None;
                            has_process_probe = false;
                            acquisition_started_at = None;
                            last_content_change_at = None;
                            pending_foreground_shell_clear = false;
                            foreground_shell_exit_reported = false;
                            release_was_active = false;
                            pending_restore_probe = false;
                            last_visible_blocker = false;
                            last_visible_working = false;
                            last_visible_signal_refresh = None;
                            last_detection_text.clear();
                            last_screen_scan_detection_content_seq = None;
                            agent_startup_grace_until = None;
                            pending_idle.clear();
                        }
                    }

                    let now = Instant::now();
                    let suppressed_agent = active_pending_release(&pending_release_for_task, now);
                    if suppressed_agent.is_none() && release_was_active {
                        has_process_probe = false;
                        acquisition_started_at = None;
                        last_content_change_at = None;
                    }
                    release_was_active = suppressed_agent.is_some();
                    let pid = child_pid.load(Ordering::Acquire);
                    let mut agent = agent_presence.current_agent();
                    let lifecycle_authority_active =
                        full_lifecycle_authority_active_for_task.load(Ordering::Acquire);
                    let foreground_pgid = (pid > 0)
                        .then(|| detect::foreground_process_group_id(pid))
                        .flatten();
                    let process_group_changed =
                        foreground_group_changed(foreground_pgid, last_foreground_pgid);
                    let should_check_process = pid > 0 && {
                        let process_probe_input = ProcessProbeInput {
                            current_agent: agent,
                            suppressed_agent,
                            foreground_pgid,
                            last_foreground_pgid,
                            has_process_probe,
                            acquisition_age: acquisition_started_at
                                .map(|started| now.duration_since(started)),
                            pending_foreground_shell_clear,
                            pending_restore_probe,
                            elapsed_since_process_check: now.duration_since(last_process_check),
                        };
                        !should_skip_process_probe_for_lifecycle_authority(
                            lifecycle_authority_active,
                            process_probe_input,
                        ) && should_probe_foreground_job(process_probe_input)
                    };

                    let mut agent_changed = false;
                    if should_check_process {
                        last_process_check = now;
                        let had_process_probe = has_process_probe;
                        has_process_probe = true;
                        if pid > 0 {
                            let probe = probe_foreground_process(pid, foreground_pgid);
                            let process_name = probe.process_name;
                            let process_group_id = probe.process_group_id;
                            let foreground_is_pane_shell = probe.foreground_is_pane_shell;
                            let mut new_agent = probe.agent;

                            if let Some(suppressed_agent) = suppressed_agent {
                                if new_agent == Some(suppressed_agent) {
                                    new_agent = None;
                                } else if let Ok(mut pending_release) =
                                    pending_release_for_task.lock()
                                {
                                    *pending_release = None;
                                }
                            }

                            let previous_agent = agent_presence.current_agent();
                            let changed = match foreground_shell_agent_action(
                                previous_agent,
                                new_agent,
                                foreground_is_pane_shell,
                                foreground_shell_exit_reported,
                            ) {
                                ForegroundShellAgentAction::ReportProcessExit => {
                                    pending_foreground_shell_clear = true;
                                    false
                                }
                                ForegroundShellAgentAction::ClearAgent => {
                                    pending_foreground_shell_clear = false;
                                    foreground_shell_exit_reported = false;
                                    agent_presence.clear_current_agent()
                                }
                                ForegroundShellAgentAction::ObserveProbe => {
                                    pending_foreground_shell_clear = false;
                                    foreground_shell_exit_reported = false;
                                    agent_presence.observe_process_probe(new_agent)
                                }
                            };
                            if new_agent.is_some() {
                                last_foreground_pgid = process_group_id;
                                acquisition_started_at = None;
                                last_content_change_at = None;
                                pending_restore_probe = false;
                            } else if agent_presence.current_agent().is_none() {
                                last_foreground_pgid = process_group_id.or(foreground_pgid);
                                if had_process_probe && process_group_changed {
                                    acquisition_started_at = Some(now);
                                }
                                pending_restore_probe = false;
                            } else {
                                last_foreground_pgid = process_group_id.or(foreground_pgid);
                            }
                            if changed {
                                agent = agent_presence.current_agent();
                                if agent != previous_agent {
                                    pending_idle.clear();
                                    last_screen_scan_detection_content_seq = None;
                                    // A new foreground agent must not inherit OSC
                                    // title/progress evidence from the previous process.
                                    terminal.clear_agent_osc_state();
                                    if agent.is_some() {
                                        agent_startup_grace_until =
                                            Some(now + AGENT_STARTUP_GRACE_WINDOW);
                                        state = AgentState::Idle;
                                        last_visible_idle = true;
                                        last_visible_blocker = false;
                                        last_visible_working = false;
                                        last_visible_signal_refresh = None;
                                        publish_state_changed_event(
                                            state_events.clone(),
                                            pane_id,
                                            agent,
                                            AgentState::Idle,
                                            false,
                                            false,
                                            false,
                                            now,
                                        )
                                        .await;
                                    } else {
                                        agent_startup_grace_until = None;
                                    }
                                }
                                if let Some(process_name) = process_name {
                                    info!(
                                        pane = pane_id.raw(),
                                        previous_agent = ?previous_agent,
                                        ?agent,
                                        process = %process_name,
                                        pgid = ?process_group_id,
                                        "agent changed"
                                    );
                                } else {
                                    info!(
                                        pane = pane_id.raw(),
                                        previous_agent = ?previous_agent,
                                        ?agent,
                                        pgid = ?process_group_id,
                                        "agent changed"
                                    );
                                }
                                agent_changed = true;
                            }
                        }
                    }

                    let pid = child_pid.load(Ordering::Acquire);
                    // Keep the terminal restore side effect separate from render notification state.
                    #[allow(clippy::collapsible_if)]
                    if pid > 0 && terminal.maybe_restore_host_terminal_theme(pane_id, pid) {
                        if !render_dirty.swap(true, Ordering::AcqRel) {
                            render_notify.notify_one();
                        }
                    }

                    let process_exited = pending_foreground_shell_clear
                        && agent.is_some()
                        && !foreground_shell_exit_reported;

                    if lifecycle_authority_active && !process_exited {
                        pending_idle.clear();
                        continue;
                    }

                    if let Some(until) = agent_startup_grace_until {
                        if process_exited {
                            agent_startup_grace_until = None;
                            last_screen_scan_detection_content_seq = None;
                            pending_idle.clear();
                        } else {
                            if now < until {
                                pending_idle.clear();
                                continue;
                            }
                            agent_startup_grace_until = None;
                            pending_idle.clear();
                            continue;
                        }
                    }

                    let current_detection_content_seq = if agent.is_some() {
                        Some(detection_content_seq.load(Ordering::Relaxed))
                    } else {
                        None
                    };
                    match decide_detection_screen_read(DetectionScreenReadInput {
                        state,
                        agent,
                        pending_idle_active: pending_idle.active(),
                        agent_changed,
                        process_exited,
                        current_detection_content_seq,
                        last_screen_scan_detection_content_seq,
                    }) {
                        DetectionScreenReadDecision::Read => {}
                        DetectionScreenReadDecision::Skip => continue,
                    }

                    let content = terminal.detection_text();
                    last_screen_scan_detection_content_seq = current_detection_content_seq;
                    let content_changed = content != last_detection_text;
                    last_detection_text.clone_from(&content);
                    if detect::should_skip_state_update(agent, &content) {
                        pending_idle.clear();
                        continue;
                    }
                    sync_content_change_acquisition(
                        agent_presence.current_agent(),
                        suppressed_agent,
                        process_group_changed,
                        content_changed,
                        now,
                        &mut acquisition_started_at,
                        &mut last_content_change_at,
                    );

                    let osc_title = terminal.agent_osc_title();
                    let osc_progress = terminal.agent_osc_progress();
                    let Some(screen_detection) = detection_update_for_publish_with_osc(
                        agent,
                        &content,
                        &osc_title,
                        &osc_progress,
                        process_exited,
                    ) else {
                        pending_idle.clear();
                        continue;
                    };
                    match decide_screen_detection_publish(
                        ScreenDetectionPublishInput {
                            screen_detection,
                            current_state: state,
                            last_visible_idle,
                            last_visible_blocker,
                            last_visible_working,
                            last_visible_signal_refresh,
                            process_exited,
                            agent_changed,
                            now,
                        },
                        &mut pending_idle,
                    ) {
                        DetectionPublishDecision::NoPublish => {}
                        DetectionPublishDecision::Publish {
                            state: new_state,
                            visible_idle,
                            visible_blocker,
                            visible_working,
                            process_exited: publish_process_exited,
                        } => {
                            apply_agent_detection_publish_update(
                                state_events.clone(),
                                pane_id,
                                agent,
                                AgentDetectionPublishUpdate {
                                    state: new_state,
                                    visible_idle,
                                    visible_blocker,
                                    visible_working,
                                    process_exited: publish_process_exited,
                                },
                                now,
                                &mut state,
                                &mut last_visible_idle,
                                &mut last_visible_blocker,
                                &mut last_visible_working,
                                &mut last_visible_signal_refresh,
                                &mut foreground_shell_exit_reported,
                            )
                            .await;
                        }
                    }
                }
            });
            (handle.abort_handle(), detect_reset_notify, pending_release)
        };

        Ok(Self {
            pane_id,
            terminal,
            io,
            current_size: Cell::new((rows, cols, 0, 0)),
            child_pid,
            reported_cwd,
            child_wait_completed: Some(child_wait_completed),
            kitty_keyboard_flags,
            detection_content_seq,
            full_lifecycle_authority_active,
            detect_reset_notify,
            pending_release,
            preserve_processes_on_drop: false,
            detect_handle,
            output_tee,
            // Handoff-resume: the resumed runtime is always a
            // `LocalChild`-backed pane, never remote (RT-F: federated panes
            // are excluded from warm handoff, P9 scope).
            relayed_agent_status_tx: None,
        })
    }

    pub fn begin_graceful_release(&self, agent: Agent) {
        if let Ok(mut pending_release) = self.pending_release.lock() {
            *pending_release = Some(PendingAgentRelease {
                agent,
                until: std::time::Instant::now() + RELEASE_REACQUIRE_SUPPRESSION,
            });
        }
        self.detect_reset_notify.notify_one();
    }

    pub fn reset_agent_detection(&self) {
        self.detect_reset_notify.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_reset_notify_for_test(&self) -> Arc<Notify> {
        self.detect_reset_notify.clone()
    }

    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        let previous = self
            .full_lifecycle_authority_active
            .swap(active, Ordering::AcqRel);
        if active && !previous {
            self.detect_reset_notify.notify_one();
        }
    }

    pub(crate) fn current_size(&self) -> (u16, u16) {
        let (rows, cols, _, _) = self.current_size.get();
        (rows, cols)
    }

    /// Subscribe to the raw-byte tee attached at the `on_read` source: the
    /// exact bytes `process_pty_bytes` consumes, not rendered frames. Used
    /// by federation (`herdr federation-serve`) to fork PTY output without
    /// taking over the single-writer render/attach path. Multiple
    /// subscribers may coexist with the local render path.
    pub(crate) fn subscribe_output_bytes(&self) -> broadcast::Receiver<Bytes> {
        self.output_tee.subscribe()
    }

    /// Resize if the dimensions actually changed.
    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        let rows = rows.max(2);
        let cols = cols.max(4);
        let size = (rows, cols, cell_width_px, cell_height_px);
        if self.current_size.get() == size {
            return;
        }
        self.current_size.set(size);
        // Remote-backed panes: the authoritative repaint is an async round
        // trip over the federation link (pane_source.rs RT-F10), not a
        // same-machine SIGWINCH repaint, so the local replay-recovery
        // heuristic below must not run for them (see `PaneTerminal::resize`).
        let terminal_responses = self.terminal.resize(
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            self.io.is_remote(),
        );
        mark_detection_content_changed(&self.detection_content_seq);
        self.io.resize(
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            terminal_responses,
        );
    }

    #[cfg(unix)]
    pub fn nudge_child_redraw_after_handoff(&self) {
        let (rows, cols, cell_width_px, cell_height_px) = self.current_size.get();
        self.io
            .nudge_child_redraw_after_handoff(rows, cols, cell_width_px, cell_height_px);
    }

    /// Scroll up by N lines (into scrollback history).
    pub fn scroll_up(&self, lines: usize) {
        self.terminal.scroll_up(lines);
    }

    /// Scroll down by N lines (toward live output).
    pub fn scroll_down(&self, lines: usize) {
        self.terminal.scroll_down(lines);
    }

    /// Reset scroll to live view (offset = 0).
    pub fn scroll_reset(&self) {
        self.terminal.scroll_reset();
    }

    /// Set scrollback offset measured from the live bottom of the terminal.
    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.terminal.set_scroll_offset_from_bottom(lines);
    }

    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        self.terminal.scroll_metrics()
    }

    pub(crate) fn search_text_matches(
        &self,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<crate::pane::TerminalTextMatch> {
        self.terminal.search_text_matches(query, case_sensitive)
    }

    pub(crate) fn text_match_is_current(&self, text_match: crate::pane::TerminalTextMatch) -> bool {
        self.terminal.text_match_is_current(text_match)
    }

    pub(crate) fn text_matches_are_current(
        &self,
        text_matches: &[crate::pane::TerminalTextMatch],
    ) -> Vec<bool> {
        self.terminal.text_matches_are_current(text_matches)
    }

    pub(crate) fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.terminal.word_motion_target(row, col, motion)
    }

    pub fn input_state(&self) -> Option<InputState> {
        self.terminal.input_state()
    }

    pub fn cursor_state(&self, area: Rect, show_cursor: bool) -> Option<TerminalCursorState> {
        if !show_cursor {
            return None;
        }
        let cursor = self.terminal.cursor_state()?;
        if cursor.x >= area.width || cursor.y >= area.height {
            return None;
        }
        Some(TerminalCursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    pub fn visible_text(&self) -> String {
        self.terminal.visible_text()
    }

    pub fn visible_ansi(&self) -> String {
        self.terminal.visible_ansi()
    }

    pub fn detection_text(&self) -> String {
        self.terminal.detection_text()
    }

    /// Sender for relaying a remote's foreground-process-equivalent
    /// `AgentStatus` into this pane's detection loop (P6). `None` for
    /// locally-spawned runtimes (which already have a real process to
    /// probe); `Some` only for `spawn_remote`-constructed runtimes. Wired
    /// by `App::build_remote_pane` into
    /// `TerminalChannelRouter::register_agent_status_sender`, driven by
    /// `remote::federation::client::drive_mount_channel`'s `AgentStatus`
    /// handling (which reads `remote::federation::reducer::RemoteMirror::
    /// apply_agent_status`'s companion status).
    pub(crate) fn relayed_agent_status_sender(&self) -> Option<mpsc::Sender<RelayedAgentStatus>> {
        self.relayed_agent_status_tx.clone()
    }

    /// This pane's raw (un-namespaced) remote terminal id, if it is
    /// `spawn_remote`-constructed (`PaneRuntimeIo::Remote`). `None` for a
    /// locally-spawned pane. Used by `handle_pane_split` (P9 follow-up) to
    /// address a `SplitPaneRequest` at the correct remote pane without a
    /// separate id-mapping registry.
    pub(crate) fn remote_terminal_id(&self) -> Option<&str> {
        match &self.io {
            PaneRuntimeIo::Remote(remote) => Some(remote.raw_terminal_id()),
            _ => None,
        }
    }

    /// A clone of this pane's shared mount out-tx, if it is
    /// `spawn_remote`-constructed. `None` for a locally-spawned pane. Lets a
    /// caller enqueue a control-channel message (e.g. `SplitPaneRequest`) on
    /// the exact mount tunnel this pane already rides.
    pub(crate) fn remote_out_tx(&self) -> Option<mpsc::UnboundedSender<FederationMessage>> {
        match &self.io {
            PaneRuntimeIo::Remote(remote) => Some(remote.out_tx()),
            _ => None,
        }
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.terminal.terminal_title()
    }

    pub fn agent_osc_title(&self) -> String {
        self.terminal.agent_osc_title()
    }

    pub fn agent_osc_progress(&self) -> String {
        self.terminal.agent_osc_progress()
    }

    pub fn recent_text(&self, lines: usize) -> String {
        self.terminal.recent_text(lines)
    }

    pub fn recent_ansi(&self, lines: usize) -> String {
        self.terminal.recent_ansi(lines)
    }

    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.terminal.recent_unwrapped_text(lines)
    }

    pub fn recent_unwrapped_ansi(&self, lines: usize) -> String {
        self.terminal.recent_unwrapped_ansi(lines)
    }

    pub fn snapshot_history(&self) -> Option<String> {
        let ansi = self.recent_unwrapped_ansi(usize::MAX);
        (!ansi.trim().is_empty()).then_some(ansi)
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        self.terminal.extract_selection(selection)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        self.terminal.render(frame, area, show_cursor);
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        self.terminal.collect_dirty_patch(area_width, area_height)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.terminal.visible_hyperlinks(area)
    }

    pub fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::ghostty::KittyImagePlacement>
    where
        F: FnMut(crate::ghostty::KittyImageDescriptor) -> bool,
    {
        self.terminal
            .kitty_image_placements_with_data_filter(needs_data)
    }

    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        let fallback = crate::input::KeyboardProtocol::from_kitty_flags(
            self.kitty_keyboard_flags.load(Ordering::Relaxed),
        );
        self.terminal.keyboard_protocol(fallback)
    }

    pub fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        self.terminal
            .encode_terminal_key(key, self.keyboard_protocol())
    }

    pub async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        self.io.send_bytes(bytes).await
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.io.try_send_bytes(bytes)
    }

    pub async fn send_paste(&self, text: String) -> Result<(), mpsc::error::SendError<Bytes>> {
        let bracketed = self
            .input_state()
            .map(|state| state.bracketed_paste)
            .unwrap_or(false);
        let payload = if bracketed {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        self.send_bytes(Bytes::from(payload)).await
    }

    pub fn try_send_focus_event(&self, event: crate::ghostty::FocusEvent) -> bool {
        if !self
            .input_state()
            .map(|state| state.focus_reporting)
            .unwrap_or(false)
        {
            return false;
        }

        let Ok(bytes) = crate::ghostty::encode_focus(event) else {
            return false;
        };
        if let Err(err) = self.try_send_bytes(Bytes::from(bytes)) {
            warn!(err = %err, ?event, "failed to forward pane focus event");
        }
        true
    }

    pub fn wheel_routing(&self) -> Option<WheelRouting> {
        self.terminal.wheel_routing()
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if !self.input_state()?.mouse_protocol_mode.reporting_enabled() {
            return None;
        }
        self.terminal
            .encode_mouse_button(kind, column, row, modifiers)
    }

    pub fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal
            .encode_mouse_motion(kind, column, row, modifiers)
    }

    pub fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing()? != WheelRouting::MouseReport {
            return None;
        }
        self.terminal
            .encode_mouse_wheel(kind, column, row, modifiers)
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        self.input_state()?;
        if self.wheel_routing()? != WheelRouting::AlternateScroll {
            return None;
        }
        let key = match kind {
            crossterm::event::MouseEventKind::ScrollUp => crossterm::event::KeyCode::Up,
            crossterm::event::MouseEventKind::ScrollDown => crossterm::event::KeyCode::Down,
            _ => return None,
        };
        Some(self.encode_terminal_key(crate::input::TerminalKey::new(
            key,
            crossterm::event::KeyModifiers::empty(),
        )))
    }

    /// Get the current working directory of the child shell process.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        if let Some(cwd) = self
            .reported_cwd
            .lock()
            .ok()
            .and_then(|reported_cwd| reported_cwd.clone())
            .and_then(usable_reported_cwd)
        {
            return Some(cwd);
        }

        let pid = self.child_pid.load(Ordering::Relaxed);
        crate::platform::process_cwd(pid)
    }

    pub fn child_pid(&self) -> Option<u32> {
        let pid = self.child_pid.load(Ordering::Acquire);
        (pid > 0).then_some(pid)
    }

    /// Get the current working directory of the process group controlling the pane PTY.
    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        #[cfg(unix)]
        {
            let pid = self.child_pid.load(Ordering::Acquire);
            let shell_cwd = usable_process_cwd(pid);
            let foreground_pgid = self
                .io
                .foreground_process_group_id()
                .or_else(|| crate::platform::foreground_process_group_id(pid));
            let leader_cwd = foreground_pgid.and_then(usable_process_cwd);

            if leader_cwd.as_ref() == shell_cwd.as_ref() {
                foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref()).or(leader_cwd)
            } else {
                leader_cwd
                    .or_else(|| foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref()))
            }
        }

        #[cfg(not(unix))]
        {
            None
        }
    }
}

#[cfg(test)]
impl PaneRuntime {
    pub(crate) fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, 0, &[], 4)
    }

    pub(crate) fn test_with_channel_capacity(
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, 0, &[], capacity)
    }

    pub(crate) fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::test_with_scrollback_bytes(cols, rows, 0, bytes)
    }

    pub(crate) fn test_process_pty_bytes(&self, bytes: &[u8]) {
        let (tx, _rx) = mpsc::channel(1);
        let _ = self.terminal.process_pty_bytes(self.pane_id, 0, bytes, &tx);
    }

    /// Test-only stand-in for the `on_read` seam: drives both the terminal
    /// parser (as `test_process_pty_bytes` does) and the raw-byte tee, so
    /// federation loopback tests can assert the tee delivers the exact bytes
    /// the local grid consumed (CX-4 raw-vs-rendered fidelity).
    pub(crate) fn test_process_pty_bytes_and_tee(&self, bytes: &[u8]) {
        self.test_process_pty_bytes(bytes);
        let _ = self.output_tee.send(Bytes::copy_from_slice(bytes));
    }

    pub(crate) fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, scrollback_limit_bytes, bytes, 4).0
    }

    pub(crate) fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel(channel_capacity);
        let (resize_tx, _resize_rx) = watch::channel((rows, cols, 0, 0));
        let mut terminal =
            crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes).unwrap();
        terminal.write(bytes);

        (
            Self {
                pane_id: PaneId::from_raw(0),
                terminal: Arc::new(PaneTerminal::new(
                    GhosttyPaneTerminal::new(terminal, tx.clone()).unwrap(),
                )),
                io: PaneRuntimeIo::TestChannel {
                    sender: tx,
                    resize_tx,
                },
                current_size: Cell::new((rows, cols, 0, 0)),
                child_pid: Arc::new(AtomicU32::new(0)),
                reported_cwd: Arc::new(Mutex::new(None)),
                child_wait_completed: None,
                kitty_keyboard_flags: Arc::new(AtomicU16::new(0)),
                detection_content_seq: Arc::new(AtomicU64::new(0)),
                full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
                detect_reset_notify: Arc::new(Notify::new()),
                pending_release: Arc::new(Mutex::new(None)),
                preserve_processes_on_drop: true,
                detect_handle: tokio::spawn(async {}).abort_handle(),
                output_tee: broadcast::channel::<Bytes>(OUTPUT_TEE_CAPACITY).0,
                relayed_agent_status_tx: None,
            },
            rx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_liveness_treats_reaped_direct_child_as_gone() {
        assert!(!process_alive_for_shutdown(42, 42, true, |_| true));
    }

    #[test]
    fn shutdown_liveness_keeps_unreaped_direct_child_alive() {
        assert!(process_alive_for_shutdown(42, 42, false, |_| true));
    }

    #[test]
    fn shutdown_liveness_keeps_other_session_processes_alive() {
        assert!(process_alive_for_shutdown(43, 42, true, |_| true));
    }

    #[test]
    fn shutdown_liveness_treats_missing_process_as_gone() {
        assert!(!process_alive_for_shutdown(43, 42, false, |_| false));
    }

    #[cfg(unix)]
    fn capture_shell_output(command: &str, extra_env: &[(&str, &str)]) -> String {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let output_path = std::env::temp_dir().join(format!(
            "herdr-pane-term-test-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(format!("{command} > '{}'", output_path.display()));
        cmd.cwd(std::env::current_dir().unwrap());
        cmd.env("TERM", "xterm-ghostty");
        cmd.env("COLORTERM", "falsecolor");
        apply_pane_terminal_env(&mut cmd);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let mut child = pair.slave.spawn_command(cmd).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "shell command failed: {status:?}");

        let output = std::fs::read_to_string(&output_path).unwrap();
        let _ = std::fs::remove_file(output_path);
        output
    }

    #[test]
    fn pane_shell_prefers_configured_shell() {
        assert_eq!(
            pane_shell_from("/usr/bin/nu", Some("/bin/bash".to_string())),
            "/usr/bin/nu"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn pane_shell_falls_back_to_shell_env() {
        assert_eq!(
            pane_shell_from("", Some("/bin/bash".to_string())),
            "/bin/bash"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pane_shell_ignores_shell_env_on_windows() {
        assert_eq!(
            pane_shell_from("", Some("c:\\windows\\system32\\cmd.exe".to_string())),
            default_pane_shell()
        );
    }

    #[test]
    fn pane_shell_ignores_empty_values() {
        assert_eq!(
            pane_shell_from("   ", Some("  ".to_string())),
            default_pane_shell()
        );
        assert_eq!(pane_shell_from("", None), default_pane_shell());
    }

    #[test]
    fn shell_mode_auto_uses_login_shell_only_on_macos() {
        assert!(shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::Auto,
            ShellLaunchTarget::Macos
        ));
        assert!(!shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::Auto,
            ShellLaunchTarget::OtherUnix
        ));
        assert!(!shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::Auto,
            ShellLaunchTarget::Windows
        ));
        assert!(shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::Login,
            ShellLaunchTarget::OtherUnix
        ));
        assert!(!shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::NonLogin,
            ShellLaunchTarget::Macos
        ));
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_builder_uses_default_prog_with_resolved_shell_env() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::Login),
            ShellLaunchTarget::OtherUnix,
        )
        .unwrap();
        assert!(cmd.is_default_prog());
        assert_eq!(
            cmd.get_env("SHELL").and_then(std::ffi::OsStr::to_str),
            Some("/bin/sh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn auto_shell_builder_uses_login_shell_on_macos_target() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::Auto),
            ShellLaunchTarget::Macos,
        )
        .unwrap();
        assert!(cmd.is_default_prog());
        assert_eq!(
            cmd.get_env("SHELL").and_then(std::ffi::OsStr::to_str),
            Some("/bin/sh")
        );
    }

    #[test]
    fn auto_shell_builder_keeps_direct_shell_on_non_macos_target() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::Auto),
            ShellLaunchTarget::OtherUnix,
        )
        .unwrap();
        assert!(!cmd.is_default_prog());
        assert_eq!(cmd.get_argv(), &[std::ffi::OsString::from("/bin/sh")]);
    }

    #[test]
    fn windows_powershell_builder_injects_prompt_cwd_shell_integration() {
        for shell in [
            "powershell.exe",
            "pwsh.exe",
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
        ] {
            let cmd = pane_shell_command_builder_for_target(
                PaneShellConfig::new(shell, crate::config::ShellModeConfig::NonLogin),
                ShellLaunchTarget::Windows,
            )
            .unwrap();

            assert_eq!(
                cmd.get_argv(),
                &[
                    std::ffi::OsString::from(shell),
                    std::ffi::OsString::from("-NoExit"),
                    std::ffi::OsString::from("-Command"),
                    std::ffi::OsString::from(WINDOWS_POWERSHELL_SHELL_INTEGRATION_COMMAND),
                ]
            );
        }

        let script = WINDOWS_POWERSHELL_SHELL_INTEGRATION_COMMAND;
        assert!(script.contains("]9;9;"), "missing OSC 9;9 emit: {script}");
        assert!(
            script.contains("$global:__HerdrOriginalPrompt = $function:prompt"),
            "must wrap the profile-defined prompt: {script}"
        );
        assert!(
            script.contains("$null -eq $global:__HerdrOriginalPrompt"),
            "wrap must be idempotent for nested sessions: {script}"
        );
        assert!(
            script.contains("'FileSystem'"),
            "must not report non-filesystem provider paths: {script}"
        );
        assert!(
            !script.contains('"'),
            "double quotes corrupt the powershell.exe command-line round-trip: {script}"
        );
        let invoke_original = script
            .find("@(& $global:__HerdrOriginalPrompt)")
            .expect("wrapper must invoke the original prompt");
        let cwd_lookup = script
            .find("$loc =")
            .expect("wrapper must look up the current location");
        assert!(
            invoke_original < cwd_lookup,
            "original prompt must run first or $? is reset before a status-aware prompt reads it: {script}"
        );
    }

    #[test]
    fn windows_non_powershell_builder_launches_plain_shell() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("cmd.exe", crate::config::ShellModeConfig::NonLogin),
            ShellLaunchTarget::Windows,
        )
        .unwrap();

        assert_eq!(cmd.get_argv(), &[std::ffi::OsString::from("cmd.exe")]);
    }

    #[test]
    fn unix_powershell_builder_launches_plain_shell() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("pwsh", crate::config::ShellModeConfig::NonLogin),
            ShellLaunchTarget::OtherUnix,
        )
        .unwrap();

        assert_eq!(cmd.get_argv(), &[std::ffi::OsString::from("pwsh")]);
    }

    #[test]
    fn windows_powershell_pane_shell_predicate_requires_windows_and_non_login() {
        let pwsh = PaneShellConfig::new("pwsh.exe", crate::config::ShellModeConfig::NonLogin);
        assert!(uses_windows_powershell_pane_shell_for_target(
            pwsh,
            ShellLaunchTarget::Windows
        ));
        assert!(!uses_windows_powershell_pane_shell_for_target(
            pwsh,
            ShellLaunchTarget::OtherUnix
        ));
        assert!(!uses_windows_powershell_pane_shell_for_target(
            pwsh,
            ShellLaunchTarget::Macos
        ));
        assert!(!uses_windows_powershell_pane_shell_for_target(
            PaneShellConfig::new("pwsh.exe", crate::config::ShellModeConfig::Login),
            ShellLaunchTarget::Windows
        ));
        assert!(!uses_windows_powershell_pane_shell_for_target(
            PaneShellConfig::new("cmd.exe", crate::config::ShellModeConfig::NonLogin),
            ShellLaunchTarget::Windows
        ));
    }

    #[test]
    fn login_shell_builder_rejects_missing_shell_instead_of_falling_back() {
        let err = pane_shell_command_builder_for_target(
            PaneShellConfig::new(
                "/__herdr_missing_shell__",
                crate::config::ShellModeConfig::Login,
            ),
            ShellLaunchTarget::OtherUnix,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_builder_resolves_bare_shell_names_from_path() {
        let _lock = crate::integration::integration_env_lock();
        let base = std::env::temp_dir().join(format!(
            "herdr-login-shell-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let shell = bin.join("fake-shell");
        std::fs::write(&shell, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let original_path = std::env::var_os("PATH");
        std::env::set_var("PATH", &bin);

        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("fake-shell", crate::config::ShellModeConfig::Login),
            ShellLaunchTarget::OtherUnix,
        )
        .unwrap();

        assert!(cmd.is_default_prog());
        assert_eq!(
            cmd.get_env("SHELL").and_then(std::ffi::OsStr::to_str),
            shell.to_str()
        );
        match original_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_resolution_preserves_shell_paths() {
        assert_eq!(resolve_shell_for_login_mode("/bin/sh").unwrap(), "/bin/sh");
    }

    #[test]
    fn non_login_shell_builder_execs_resolved_shell_directly() {
        let cmd = pane_shell_command_builder(PaneShellConfig::new(
            "/bin/sh",
            crate::config::ShellModeConfig::NonLogin,
        ))
        .unwrap();
        assert!(!cmd.is_default_prog());
        assert_eq!(cmd.get_argv(), &[std::ffi::OsString::from("/bin/sh")]);
    }

    #[cfg(unix)]
    #[test]
    fn pane_terminal_identity_overrides_outer_terminal_env() {
        let output = capture_shell_output("printf '%s\\n%s\\n' \"$TERM\" \"$COLORTERM\"", &[]);
        assert_eq!(output, "xterm-256color\ntruecolor\n");
    }

    #[cfg(unix)]
    #[test]
    fn pane_terminal_identity_allows_explicit_override() {
        let output = capture_shell_output(
            "printf '%s\\n%s\\n' \"$TERM\" \"$COLORTERM\"",
            &[("TERM", "vt100"), ("COLORTERM", "24bit")],
        );
        assert_eq!(output, "vt100\n24bit\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handoff_history_ansi_captures_primary_screen() {
        let runtime =
            PaneRuntime::test_with_scrollback_bytes(40, 5, 4096, b"handoff-primary-history\r\n");

        let history = runtime.handoff_history_ansi().unwrap();

        assert!(history.contains("handoff-primary-history"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handoff_history_ansi_skips_alternate_screen() {
        let runtime = PaneRuntime::test_with_scrollback_bytes(
            40,
            5,
            4096,
            b"primary\r\n\x1b[?1049halt-screen",
        );

        assert!(runtime.handoff_history_ansi().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handoff_runtime_state_captures_terminal_input_and_title_state() {
        let runtime = PaneRuntime::test_with_screen_bytes(
            80,
            24,
            b"\x1b[>5u\x1b[>4;2m\x1b[?1h\x1b[?2004h\x1b[?1004h\x1b[?1002h\x1b[?1006h",
        );

        runtime.test_process_pty_bytes("\x1b]2;✳ 修复🙂标题\x1b\\".as_bytes());
        runtime.terminal.clear_agent_osc_state();
        assert_eq!(runtime.agent_osc_title(), "");
        let pane = runtime.handoff_runtime_state(12);

        assert_eq!(pane.keyboard_protocol_flags, 5);
        assert_eq!(pane.terminal_title.as_deref(), Some("✳ 修复🙂标题"));
        assert_eq!(
            pane.input_state,
            Some(InputState {
                alternate_screen: false,
                application_cursor: true,
                bracketed_paste: true,
                focus_reporting: true,
                mouse_protocol_mode: crate::input::MouseProtocolMode::ButtonMotion,
                mouse_protocol_encoding: crate::input::MouseProtocolEncoding::Sgr,
                mouse_alternate_scroll: true,
                modify_other_keys: true,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn truncate_handoff_history_keeps_recent_utf8_boundary() {
        let history = format!("old\n{}\nrecent\n", "é".repeat(8));

        let truncated = truncate_handoff_history(history, 20);

        assert_eq!(truncated, "recent\n");
        assert!(truncated.is_char_boundary(0));
    }

    #[cfg(unix)]
    #[test]
    fn truncate_handoff_history_drops_partial_long_line() {
        let history = format!("old\n{}", "x".repeat(64));

        let truncated = truncate_handoff_history(history, 12);

        assert!(truncated.is_empty());
    }

    #[tokio::test]
    async fn focus_events_are_forwarded_when_enabled() {
        let (tx, mut rx) = mpsc::channel(4);
        let (resize_tx, _resize_rx) = watch::channel((80, 24, 0, 0));
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0).unwrap();
        terminal
            .mode_set(crate::ghostty::MODE_FOCUS_EVENT, true)
            .unwrap();
        let runtime = PaneRuntime {
            pane_id: PaneId::from_raw(0),
            terminal: Arc::new(PaneTerminal::new(
                GhosttyPaneTerminal::new(terminal, tx.clone()).unwrap(),
            )),
            io: PaneRuntimeIo::TestChannel {
                sender: tx,
                resize_tx,
            },
            current_size: Cell::new((80, 24, 0, 0)),
            child_pid: Arc::new(AtomicU32::new(0)),
            reported_cwd: Arc::new(Mutex::new(None)),
            child_wait_completed: None,
            kitty_keyboard_flags: Arc::new(AtomicU16::new(0)),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            pending_release: Arc::new(Mutex::new(None)),
            preserve_processes_on_drop: true,
            detect_handle: tokio::spawn(async {}).abort_handle(),
            output_tee: broadcast::channel::<Bytes>(OUTPUT_TEE_CAPACITY).0,
            relayed_agent_status_tx: None,
        };

        assert!(runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained));
        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"\x1b[I"));
    }

    #[tokio::test]
    async fn focus_events_are_suppressed_when_disabled() {
        let (tx, mut rx) = mpsc::channel(4);
        let (resize_tx, _resize_rx) = watch::channel((80, 24, 0, 0));
        let terminal = crate::ghostty::Terminal::new(80, 24, 0).unwrap();
        let runtime = PaneRuntime {
            pane_id: PaneId::from_raw(0),
            terminal: Arc::new(PaneTerminal::new(
                GhosttyPaneTerminal::new(terminal, tx.clone()).unwrap(),
            )),
            io: PaneRuntimeIo::TestChannel {
                sender: tx,
                resize_tx,
            },
            current_size: Cell::new((80, 24, 0, 0)),
            child_pid: Arc::new(AtomicU32::new(0)),
            reported_cwd: Arc::new(Mutex::new(None)),
            child_wait_completed: None,
            kitty_keyboard_flags: Arc::new(AtomicU16::new(0)),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            pending_release: Arc::new(Mutex::new(None)),
            preserve_processes_on_drop: true,
            detect_handle: tokio::spawn(async {}).abort_handle(),
            output_tee: broadcast::channel::<Bytes>(OUTPUT_TEE_CAPACITY).0,
            relayed_agent_status_tx: None,
        };

        assert!(!runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    #[test]
    fn foreground_shell_reports_process_exit_before_clearing_agent() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Codex), None, true, false),
            ForegroundShellAgentAction::ReportProcessExit
        );
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Codex), None, true, true),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn unknown_non_shell_foreground_job_is_not_immediate_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Claude), None, false, false),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    #[test]
    fn reported_process_exit_clears_before_unknown_foreground_probe() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Claude), None, false, true),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn foreground_agent_job_is_not_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(Some(Agent::Claude), Some(Agent::OpenCode), true, false),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    fn foreground_process(pid: u32, name: &str) -> crate::platform::ForegroundProcess {
        crate::platform::ForegroundProcess {
            pid,
            name: name.to_string(),
            argv0: None,
            argv: None,
            cmdline: None,
        }
    }

    #[test]
    fn foreground_agent_hint_accepts_pane_shell_environment() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 42,
            processes: vec![foreground_process(42, "bash")],
        };

        assert_eq!(
            agent_hint_for_foreground_job_members(&job, |pid| {
                (pid == 42).then_some(Agent::Claude)
            }),
            Some(Agent::Claude)
        );
    }

    #[test]
    fn foreground_agent_hint_accepts_non_leader_foreground_process_environment() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "fence"),
                foreground_process(100, "pi"),
            ],
        };

        assert_eq!(
            agent_hint_for_foreground_job_members(&job, |pid| {
                (pid == 100).then_some(Agent::Codex)
            }),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn foreground_agent_hint_wins_over_process_name_detection() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![foreground_process(99, "codex")],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            Some(job),
            || None,
            |pid| (pid == 99).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Claude));
        assert_eq!(result.process_name.as_deref(), Some("claude"));
    }

    #[test]
    fn foreground_agent_hint_on_inherited_child_environment_is_authoritative() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![foreground_process(99, "vim")],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            None,
            || Some(job),
            |pid| (pid == 99).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Claude));
        assert_eq!(result.process_name.as_deref(), Some("claude"));
    }

    #[test]
    fn non_leader_agent_hint_does_not_override_identifiable_leader() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "codex"),
                foreground_process(100, "vim"),
            ],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            None,
            || Some(job),
            |pid| (pid == 100).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Codex));
        assert_eq!(result.process_name.as_deref(), Some("codex"));
    }

    #[test]
    fn non_leader_agent_hint_wins_when_leader_is_unidentified() {
        let job = crate::platform::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "some_vm"),
                foreground_process(100, "vim"),
            ],
        };

        let result = probe_foreground_process_from_jobs(
            42,
            Some(99),
            None,
            || Some(job),
            |pid| (pid == 100).then_some(Agent::Claude),
        );

        assert_eq!(result.agent, Some(Agent::Claude));
        assert_eq!(result.process_name.as_deref(), Some("claude"));
    }

    fn process_probe_input() -> ProcessProbeInput {
        ProcessProbeInput {
            current_agent: None,
            suppressed_agent: None,
            foreground_pgid: Some(42),
            last_foreground_pgid: Some(42),
            has_process_probe: true,
            acquisition_age: None,
            pending_foreground_shell_clear: false,
            pending_restore_probe: false,
            elapsed_since_process_check: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn unchanged_unidentified_foreground_group_skips_full_process_probe() {
        assert!(!should_probe_foreground_job(process_probe_input()));
    }

    #[test]
    fn unidentified_foreground_group_change_runs_full_process_probe() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: Some(43),
            ..process_probe_input()
        }));
    }

    #[test]
    fn unidentified_pane_gets_initial_process_probe() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn stable_unidentified_foreground_group_has_no_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            elapsed_since_process_check: PROCESS_RECHECK_MISSING_FOREGROUND_GROUP,
            ..process_probe_input()
        }));
    }

    #[test]
    fn unidentified_pane_without_foreground_group_uses_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: None,
            last_foreground_pgid: None,
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: None,
            last_foreground_pgid: None,
            elapsed_since_process_check: PROCESS_RECHECK_MISSING_FOREGROUND_GROUP,
            ..process_probe_input()
        }));
    }

    #[test]
    fn unidentified_pane_probes_when_foreground_group_disappears() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            foreground_pgid: None,
            last_foreground_pgid: Some(42),
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_shell_clear_and_restore_force_process_probes() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            pending_foreground_shell_clear: true,
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            pending_restore_probe: true,
            ..process_probe_input()
        }));
    }

    #[test]
    fn lifecycle_authority_skips_stable_routine_process_probe() {
        assert!(should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
                ..process_probe_input()
            }
        ));
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            false,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
                ..process_probe_input()
            }
        ));
    }

    #[test]
    fn lifecycle_authority_preserves_process_exit_and_release_probes() {
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                pending_foreground_shell_clear: true,
                ..process_probe_input()
            }
        ));
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                suppressed_agent: Some(Agent::Pi),
                ..process_probe_input()
            }
        ));
    }

    #[test]
    fn lifecycle_authority_preserves_initial_and_foreground_group_change_probes() {
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: None,
                has_process_probe: false,
                ..process_probe_input()
            }
        ));
        assert!(!should_skip_process_probe_for_lifecycle_authority(
            true,
            ProcessProbeInput {
                current_agent: Some(Agent::Pi),
                foreground_pgid: Some(43),
                ..process_probe_input()
            }
        ));
    }

    #[test]
    fn pending_release_forces_initial_process_probe() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            suppressed_agent: Some(Agent::Codex),
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_release_forces_process_probe_after_runtime_identity_clears() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            suppressed_agent: Some(Agent::Codex),
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_release_skips_repeated_probe_when_foreground_group_is_stable() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            suppressed_agent: Some(Agent::Codex),
            ..process_probe_input()
        }));
    }

    #[test]
    fn pending_release_probes_when_foreground_group_changes() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            suppressed_agent: Some(Agent::Codex),
            foreground_pgid: Some(43),
            ..process_probe_input()
        }));
    }

    #[test]
    fn acquisition_window_catches_delayed_same_group_wrapper_startup() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(std::time::Duration::from_millis(1250)),
            elapsed_since_process_check: PROCESS_ACQUISITION_FAST_RECHECK
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(std::time::Duration::from_millis(1250)),
            elapsed_since_process_check: PROCESS_ACQUISITION_FAST_RECHECK,
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(std::time::Duration::from_secs(5)),
            elapsed_since_process_check: PROCESS_ACQUISITION_SLOW_RECHECK,
            ..process_probe_input()
        }));
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            acquisition_age: Some(PROCESS_ACQUISITION_WINDOW + std::time::Duration::from_millis(1),),
            elapsed_since_process_check: PROCESS_ACQUISITION_SLOW_RECHECK,
            ..process_probe_input()
        }));
    }

    #[test]
    fn content_change_starts_bounded_unidentified_acquisition_window() {
        let now = std::time::Instant::now();
        let mut acquisition_started_at = None;
        let mut last_content_change_at = None;

        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, Some(now));
        assert_eq!(last_content_change_at, Some(now));

        let later = now + std::time::Duration::from_secs(1);
        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            later,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(
            acquisition_started_at,
            Some(now),
            "changed frames should not refresh the acquisition window"
        );
        assert_eq!(last_content_change_at, Some(later));

        let quiet_after_window =
            later + PROCESS_ACQUISITION_WINDOW + PROCESS_ACQUISITION_IDLE_RESET;
        sync_content_change_acquisition(
            None,
            None,
            false,
            false,
            quiet_after_window,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);

        let next_burst = quiet_after_window + std::time::Duration::from_secs(1);
        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            next_burst,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, Some(next_burst));
        assert_eq!(last_content_change_at, Some(next_burst));
    }

    #[test]
    fn content_change_does_not_start_acquisition_when_process_probe_has_other_signal() {
        let now = std::time::Instant::now();
        let mut acquisition_started_at = None;
        let mut last_content_change_at = None;

        sync_content_change_acquisition(
            Some(Agent::Codex),
            None,
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);

        sync_content_change_acquisition(
            None,
            Some(Agent::Codex),
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);

        sync_content_change_acquisition(
            None,
            None,
            true,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );
        assert_eq!(acquisition_started_at, None);
        assert_eq!(last_content_change_at, None);
    }

    #[test]
    fn content_change_restarts_stale_process_group_acquisition_window() {
        let now = std::time::Instant::now();
        let stale_start = now - PROCESS_ACQUISITION_WINDOW - std::time::Duration::from_millis(1);
        let mut acquisition_started_at = Some(stale_start);
        let mut last_content_change_at = None;

        sync_content_change_acquisition(
            None,
            None,
            false,
            true,
            now,
            &mut acquisition_started_at,
            &mut last_content_change_at,
        );

        assert_eq!(acquisition_started_at, Some(now));
        assert_eq!(last_content_change_at, Some(now));
    }

    #[test]
    fn release_expiry_can_force_reacquire_probe_by_resetting_probe_state() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: None,
            has_process_probe: false,
            ..process_probe_input()
        }));
    }

    #[test]
    fn identified_agent_uses_shorter_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
            ..process_probe_input()
        }));
    }

    #[test]
    fn identified_agent_probes_when_foreground_group_disappears() {
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            foreground_pgid: None,
            last_foreground_pgid: Some(42),
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
    }

    #[test]
    fn stable_missing_foreground_group_uses_safety_process_probe() {
        assert!(!should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            foreground_pgid: None,
            last_foreground_pgid: None,
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED
                - std::time::Duration::from_millis(1),
            ..process_probe_input()
        }));
        assert!(should_probe_foreground_job(ProcessProbeInput {
            current_agent: Some(Agent::Codex),
            foreground_pgid: None,
            last_foreground_pgid: None,
            elapsed_since_process_check: PROCESS_RECHECK_IDENTIFIED,
            ..process_probe_input()
        }));
    }

    #[test]
    fn transient_process_miss_keeps_current_agent_detected() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        let changed = presence.observe_process_probe(None);

        assert!(!changed, "one miss should not clear the detected agent");
        assert_eq!(presence.current_agent(), Some(Agent::Pi));
    }

    #[test]
    fn agent_only_clears_after_confirmation_misses() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        for attempt in 1..AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let changed = presence.observe_process_probe(None);
            assert!(
                !changed,
                "miss {attempt} should stay in the confirmation window"
            );
            assert_eq!(presence.current_agent(), Some(Agent::Pi));
        }

        let changed = presence.observe_process_probe(None);
        assert!(changed, "last confirmation miss should clear the agent");
        assert_eq!(presence.current_agent(), None);
    }

    #[tokio::test]
    async fn set_full_lifecycle_authority_active_notifies_only_on_activation_transitions() {
        let runtime = PaneRuntime::test_with_screen_bytes(80, 24, b"");
        let reset_notify = runtime.agent_detection_reset_notify_for_test();

        runtime.set_full_lifecycle_authority_active(true);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("false-to-true transition should notify detection reset");

        runtime.set_full_lifecycle_authority_active(true);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reset_notify.notified()
            )
            .await
            .is_err(),
            "repeated true-to-true sync should not notify detection reset"
        );

        runtime.set_full_lifecycle_authority_active(false);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reset_notify.notified()
            )
            .await
            .is_err(),
            "true-to-false transition should not notify detection reset"
        );

        runtime.set_full_lifecycle_authority_active(true);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("re-entering active authority should notify detection reset");
    }

    #[tokio::test]
    async fn state_changed_event_waits_for_queue_space_instead_of_dropping() {
        let (tx, mut rx) = mpsc::channel(1);
        let pane_id = PaneId::from_raw(42);

        tx.try_send(AppEvent::UpdateReady {
            version: "9.9.9".into(),
            install_command: "herdr update".into(),
        })
        .unwrap();

        let publish = publish_state_changed_event(
            tx.clone(),
            pane_id,
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            false,
            false,
            std::time::Instant::now(),
        );
        tokio::pin!(publish);

        let blocked = tokio::time::timeout(std::time::Duration::from_millis(20), async {
            (&mut publish).await;
        })
        .await;
        assert!(
            blocked.is_err(),
            "publisher should wait for queue space instead of dropping StateChanged"
        );

        let first = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("queue should yield first event")
            .expect("sender still alive");
        assert!(matches!(first, AppEvent::UpdateReady { .. }));

        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            (&mut publish).await;
        })
        .await
        .expect("publisher should complete once queue space is available");

        let second = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("queue should yield second event")
            .expect("sender still alive");
        assert!(matches!(
            second,
            AppEvent::StateChanged {
                pane_id: delivered_pane,
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: _,
            } if delivered_pane == pane_id
        ));
    }
}

/// P5: `PaneRuntime::spawn_remote` — remote-backed panes (raw byte channel
/// -> the same `process_pty_bytes` pathway a local pane uses). Loopback-only
/// (no real SSH), per the phase's TDD plan.
#[cfg(test)]
mod remote_spawn_tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn spawn_test_remote_pane(
        terminal_id: &str,
        mount_generation: u64,
    ) -> (
        PaneRuntime,
        mpsc::Sender<Bytes>,
        mpsc::UnboundedReceiver<FederationMessage>,
        mpsc::Receiver<AppEvent>,
    ) {
        let (events_tx, events_rx) = mpsc::channel::<AppEvent>(16);
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (output_tx, output_rx) = mpsc::channel::<Bytes>(64);
        let (clipboard_tx, _clipboard_rx) = mpsc::unbounded_channel();
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let runtime = PaneRuntime::spawn_remote(
            PaneId::from_raw(1),
            24,
            80,
            1 << 20,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            terminal_id.to_string(),
            mount_generation,
            out_tx,
            output_rx,
            clipboard_tx,
            events_tx,
            render_notify,
            render_dirty,
        )
        .expect("spawn_remote should succeed with no local PTY/child involved");
        (runtime, output_tx, out_rx, events_rx)
    }

    // Test 1 (CX-4, full fidelity): bytes pushed onto the demuxed output
    // channel drive the SAME `process_pty_bytes` pathway a local pane uses,
    // producing the exact same rendered grid a local PTY source would for
    // identical bytes.
    #[tokio::test]
    async fn remote_byte_in_renders_the_same_grid_as_a_local_pty_source() {
        let known_bytes = b"hello federation\r\nsecond line";

        let local_reference = PaneRuntime::test_with_channel(80, 24).0;
        local_reference.test_process_pty_bytes(known_bytes);

        let (remote_runtime, output_tx, _out_rx, _events_rx) = spawn_test_remote_pane("term_1", 1);
        output_tx
            .send(Bytes::copy_from_slice(known_bytes))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        assert_eq!(
            remote_runtime.visible_text(),
            local_reference.visible_text()
        );
    }

    // `remote_terminal_id`/`remote_out_tx` (P9 remote-split follow-up):
    // `spawn_remote`-constructed runtimes expose their raw wire id and
    // shared mount out-tx so `handle_pane_split` can address a
    // `SplitPaneRequest` without a separate id-mapping registry; a local
    // (non-remote) runtime must expose neither.
    #[tokio::test]
    async fn remote_runtime_exposes_its_raw_terminal_id_and_out_tx() {
        let (remote_runtime, _output_tx, _out_rx, _events_rx) = spawn_test_remote_pane("term_1", 1);
        assert_eq!(remote_runtime.remote_terminal_id(), Some("term_1"));
        assert!(remote_runtime.remote_out_tx().is_some());

        let local_runtime = PaneRuntime::test_with_channel(80, 24).0;
        assert_eq!(local_runtime.remote_terminal_id(), None);
        assert!(local_runtime.remote_out_tx().is_none());
    }

    // Test 6 (RT-F6 scrollback-on-hydrate): the caller is expected to push
    // replayed history bytes onto `output_rx` before any live bytes — since
    // hydrate order falls out of wire order (no separate seeding path in
    // `spawn_remote`), pushing history first and live bytes after must
    // produce a grid containing both, in that order.
    #[tokio::test]
    async fn scrollback_replay_bytes_pushed_before_live_bytes_both_land_on_the_grid() {
        let (remote_runtime, output_tx, _out_rx, _events_rx) = spawn_test_remote_pane("term_1", 1);

        output_tx
            .send(Bytes::from_static(b"earlier history\r\n"))
            .await
            .unwrap();
        output_tx
            .send(Bytes::from_static(b"live bytes after replay"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let visible = remote_runtime.visible_text();
        assert!(visible.contains("earlier history"));
        assert!(visible.contains("live bytes after replay"));
    }

    // Test 5 (codex #5 lifecycle, E2E): dropping a remote-backed
    // `PaneRuntime` (simulating a remote disconnect/pane teardown) never
    // sends `AppEvent::PaneDied` — the local-only signal every local pane's
    // child-watcher/reader-exit path sends. `child_pid` stays 0 for the
    // runtime's whole life, so `shutdown_pane_processes` (called from
    // `Drop`) is a guaranteed no-op regardless.
    #[tokio::test]
    async fn dropping_a_remote_pane_never_emits_pane_died() {
        let (remote_runtime, output_tx, _out_rx, mut events_rx) =
            spawn_test_remote_pane("term_1", 1);

        drop(output_tx);
        drop(remote_runtime);

        let saw_event =
            tokio::time::timeout(std::time::Duration::from_millis(50), events_rx.recv()).await;
        if let Ok(Some(event)) = saw_event {
            assert!(
                !matches!(event, AppEvent::PaneDied { .. }),
                "a remote-backed pane must never emit PaneDied"
            );
        }
    }

    // Test 9 (P2 oracle re-run, narrow slice): the `Remote` variant does not
    // perturb `PaneRuntimeIo`'s handoff no-op arms — calling every
    // `#[cfg(unix)]` handoff method on a `Remote`-backed runtime must not
    // panic, mirroring the existing `TestChannel` no-op contract.
    #[cfg(unix)]
    #[tokio::test]
    async fn handoff_methods_on_a_remote_pane_no_op_without_panicking() {
        let (remote_runtime, _output_tx, _out_rx, _events_rx) = spawn_test_remote_pane("term_1", 1);

        assert!(remote_runtime.duplicate_handoff_fd().is_err());
        assert!(remote_runtime
            .pause_handoff_reader(std::time::Duration::from_millis(10))
            .is_ok());
        remote_runtime.set_handoff_reader_paused(false);
    }

    // Phase 06 test 2 (requirement 1/2, E2E through pane.rs): feeding a
    // relayed `AgentStatus` into a remote pane's dormant channel updates
    // the pane's published detection state — with `child_pid` staying 0 for
    // the runtime's whole life, so `probe_foreground_process` (which reads
    // that pid) is structurally never invoked for this update.
    #[tokio::test]
    async fn relayed_agent_status_updates_detection_state_without_any_local_probe() {
        let (remote_runtime, _output_tx, _out_rx, mut events_rx) =
            spawn_test_remote_pane("term_1", 1);

        // Structural half of the assertion: a remote-backed runtime never
        // acquires a local pid to probe.
        assert_eq!(remote_runtime.child_pid.load(Ordering::Acquire), 0);

        let sender = remote_runtime
            .relayed_agent_status_sender()
            .expect("spawn_remote runtimes must expose a relay sender");
        sender
            .send(RelayedAgentStatus {
                status: AgentStatus::Working,
                agent: Some("claude".to_string()),
            })
            .await
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .expect("relayed status must publish a StateChanged event")
            .expect("sender still alive");
        assert!(matches!(
            event,
            AppEvent::StateChanged {
                state: AgentState::Working,
                visible_working: true,
                visible_blocker: false,
                agent: Some(Agent::Claude),
                ..
            }
        ));

        // `child_pid` is still 0 after the relayed update — the process
        // probe path was never exercised.
        assert_eq!(remote_runtime.child_pid.load(Ordering::Acquire), 0);
    }

    // Locally-spawned (non-remote) runtimes have no relay sender at all —
    // there is nothing to suppress a probe for; they have a real process.
    #[tokio::test]
    async fn local_runtime_has_no_relayed_agent_status_sender() {
        let local = PaneRuntime::test_with_channel(80, 24).0;
        assert!(local.relayed_agent_status_sender().is_none());
    }

    // MAJOR fix regression: once a relayed identity is established, a
    // subsequent run of idle/no-agent frames (the remote reporting its
    // agent is gone) must clear that identity via the debounced
    // `observe_process_probe(None)` path — mirroring how the local
    // process-probe loop clears a vanished agent — so the pane stops
    // reporting as an agent terminal. A single stray idle/None frame must
    // NOT clear it (debounce), and a Working/Blocked+None frame (the shape
    // an old peer that never populates `agent` would send) must never
    // clear a known identity either.
    #[tokio::test]
    async fn relayed_idle_none_clears_identity_only_after_debounce() {
        let (remote_runtime, _output_tx, _out_rx, mut events_rx) =
            spawn_test_remote_pane("term_1", 1);
        let sender = remote_runtime
            .relayed_agent_status_sender()
            .expect("spawn_remote runtimes must expose a relay sender");

        // Establish identity.
        sender
            .send(RelayedAgentStatus {
                status: AgentStatus::Working,
                agent: Some("claude".to_string()),
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .expect("identity-establishing frame must publish")
            .expect("sender still alive");
        assert!(matches!(
            event,
            AppEvent::StateChanged {
                agent: Some(Agent::Claude),
                state: AgentState::Working,
                ..
            }
        ));

        // A Working+None frame (old-peer shape) must never clear identity,
        // and must not even publish (no status/identity change).
        sender
            .send(RelayedAgentStatus {
                status: AgentStatus::Working,
                agent: None,
            })
            .await
            .unwrap();
        let no_event =
            tokio::time::timeout(std::time::Duration::from_millis(150), events_rx.recv()).await;
        assert!(
            no_event.is_err(),
            "Working+None must not publish or clear identity"
        );

        // First Idle+None frame: publishes the Working->Idle transition,
        // but identity is still debounced, not yet cleared.
        sender
            .send(RelayedAgentStatus {
                status: AgentStatus::Idle,
                agent: None,
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .expect("idle transition must publish")
            .expect("sender still alive");
        assert!(matches!(
            event,
            AppEvent::StateChanged {
                agent: Some(Agent::Claude),
                state: AgentState::Idle,
                ..
            }
        ));

        // Repeat Idle+None frames until the debounce threshold is hit —
        // total idle/None observations must equal
        // `AGENT_MISS_CONFIRMATION_ATTEMPTS` for the clear to fire on the
        // final one. One was already sent above.
        for _ in 1..AGENT_MISS_CONFIRMATION_ATTEMPTS {
            sender
                .send(RelayedAgentStatus {
                    status: AgentStatus::Idle,
                    agent: None,
                })
                .await
                .unwrap();
        }

        let cleared = tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv())
            .await
            .expect("debounced clear must publish once threshold is reached")
            .expect("sender still alive");
        assert!(matches!(
            cleared,
            AppEvent::StateChanged {
                agent: None,
                state: AgentState::Idle,
                ..
            }
        ));
    }
}

/// Phase 06 (agent-status relay): pure mapping + cadence-gate tests, plus
/// the reducer's own coverage in `remote::federation::reducer`.
#[cfg(test)]
mod agent_status_relay_tests {
    use super::*;

    // Phase 06 requirement 1: relayed `AgentStatus` maps onto the local
    // `AgentState` vocabulary; `Done` collapses to `Idle` (no local
    // "finished" state) and `Unknown` never claims a stronger signal than
    // the source gave.
    #[test]
    fn relayed_status_maps_onto_local_agent_state() {
        assert_eq!(
            map_relayed_agent_status(AgentStatus::Idle),
            AgentState::Idle
        );
        assert_eq!(
            map_relayed_agent_status(AgentStatus::Done),
            AgentState::Idle
        );
        assert_eq!(
            map_relayed_agent_status(AgentStatus::Working),
            AgentState::Working
        );
        assert_eq!(
            map_relayed_agent_status(AgentStatus::Blocked),
            AgentState::Blocked
        );
        assert_eq!(
            map_relayed_agent_status(AgentStatus::Unknown),
            AgentState::Unknown
        );
    }

    // Phase 06 test 5 (S12.2 cadence): of N remote panes with only one
    // visible, only that one's relay gate reports active.
    #[test]
    fn only_the_visible_pane_has_an_active_relay_gate() {
        let mut gate = RemoteAgentStatusGate::new();
        let visible = PaneId::from_raw(1);
        let hidden_a = PaneId::from_raw(2);
        let hidden_b = PaneId::from_raw(3);

        // Default: nothing is active until explicitly marked visible.
        assert!(!gate.is_active(visible));
        assert!(!gate.is_active(hidden_a));
        assert!(!gate.is_active(hidden_b));

        gate.set_visible(visible, true);
        assert!(gate.is_active(visible));
        assert!(!gate.is_active(hidden_a));
        assert!(!gate.is_active(hidden_b));

        // Switching visibility away deactivates it again.
        gate.set_visible(visible, false);
        assert!(!gate.is_active(visible));
    }
}
