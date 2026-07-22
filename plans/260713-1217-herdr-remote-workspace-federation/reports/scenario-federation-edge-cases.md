# Scenario/Edge-Case Decomposition — Tier-2 True Federation

Stage: `/ck:scenario`. Grounded in `blindspot-synthesis-feasibility.md` + 4 `scout-*.md` reports
(paths above). Assumes Tier-2 design: local server owns an SSH-tunneled API-client connection to
the remote server, ingests `SessionSnapshot` + `events.subscribe`, id-namespaces everything, and
either (a) nests the remote's whole-screen stream in one pane, or (b) builds genuine remote-backed
`Pane`s server-side. Where behavior differs by sub-approach it's called out.

Legend: **Blocker** = must be designed in before Tier-2 can ship at all. **Major** = must be
handled or the feature is unsafe/unusable in normal operation, but a degraded-mode fallback is
acceptable for v1. **Minor** = deferrable, document as known limitation.

---

## 1. Connection lifecycle

**S1.1 — Initial mount succeeds, remote server was already running (compatible version).**
Trigger: `herdr --remote ssh user@ip --workspace` on a live, compatible remote.
Expected: `ensure_remote_server_ready` short-circuits (no restart), local server opens API-client
connection, pulls `session.snapshot`, mounts workspace(s) with id-namespace prefix, sidebar shows
remote group. Breaks if unhandled: N/A (already scouted as reusable seam). Severity: n/a (baseline).

**S1.2 — Mount requested while a previous mount of the same host is still tearing down.**
Trigger: user aborts a mount mid-connect and immediately retries. Expected: either queue/reject
with a clear "still disconnecting" state, or make teardown idempotent and safe to interleave.
Breaks if unhandled: two `SshStdioBridge`/API-client threads racing on the same local forward
socket path (`local_forward_socket_path` is deterministic per target+session, so a second bind can
collide) → panic or silently-duplicated remote workspace with two id-namespaces. Severity: **Major**.

**S1.3 — Remote install/`prepare_remote_herdr` requires interactive confirmation (binary
overwrite, stale server stop) but is invoked headlessly from inside the server process.**
Trigger: remote already has an incompatible/old herdr binary or a stale server running. Expected:
federation mount must run fully non-interactively (server process has no TTY) — needs an explicit
policy (auto-approve within safe bounds e.g. same major version, or fail closed with an actionable
error surfaced in the sidebar/notification, never silently hang). Breaks if unhandled: today's code
already degenerates prompts to hard errors when non-interactive stdin is detected (per scout-remote-
lifecycle-transport §2) — but that path was designed for a one-shot CLI invocation that can just
exit; inside a long-lived server it must not block the event loop or crash the process. Severity:
**Blocker** (mount path must never assume a TTY).

**S1.4 — User unmounts/closes the remote workspace while panes are mid-detection or mid-input.**
Trigger: close the remote workspace tab from sidebar. Expected: clean teardown — stop the API-
client subscription, close SSH channel(s), release namespaced ids (or mark tombstoned so late
in-flight events referencing them are dropped, not misrouted to a reused id), remove sidebar
entries atomically. Breaks if unhandled: a straggling `events_after` poll or in-flight `Frame`
delivers content for an id that now belongs to nothing (or worse, a new local workspace that reused
the numeric slot). Severity: **Major**.

---

## 2. Latency / throughput

**S2.1 — High-latency link (transcontinental, 150-300ms RTT) with interactive typing.**
Trigger: user types in a remote-backed pane over a slow link. Expected: local echo suppressed (SSH
already doesn't local-echo through a PTY relay) — input is genuinely round-tripped, so users must
perceive latency, not a broken keystroke; no doubled/dropped characters under queueing. Breaks if
unhandled: if the design tries to fake local echo for responsiveness, it desyncs from ghostty grid
state ("torn" cursor/echo) the moment output disagrees. Severity: **Major** (must NOT attempt local
echo prediction; keep it dumb-relay like SSH itself).

**S2.2 — High-throughput remote pane (e.g., `yes`, build log spam, large `cat`) saturates the SSH
tunnel.** Trigger: remote process floods output. Expected: backpressure on the SSH channel/local
forward socket must propagate to the remote PTY (same as SSH's own flow control) so the local
server's event loop / EventHub doesn't get starved or OOM-buffer unbounded bytes; other local
(non-remote) workspaces must stay responsive (no shared-thread head-of-line blocking). Severity:
**Blocker** if a single remote pane can stall the entire local render loop — the architecture must
isolate remote I/O onto its own task/actor (scout confirms `process_pty_bytes` boundary is
byte-agnostic, but the *I/O actor* driving it is not; a bad remote-actor implementation could block
shared state).

**S2.3 — `events.subscribe`/`events.wait` long-poll over SSH during network jitter.**
Trigger: remote link has variable latency causing the long-poll response to arrive late or batched.
Expected: local mirrored `EventHub` entries apply in order even if delivered in bursts; UI doesn't
show stale/flickering state between bursts. Breaks if unhandled: the single global monotonic
`next_sequence` in `EventHub` (scout: 46-line ring buffer) has no per-source ordering guarantee
once a second source is merged — burst delivery could interleave remote events ahead of causally-
earlier local events if sequencing isn't source-aware. Severity: **Blocker** (multi-source EventHub
is explicitly called out as unbuilt).

---

## 3. Version skew (local vs remote herdr protocol v16)

**S3.1 — Remote herdr is an older/newer protocol version that doesn't support federation's new
API-client role (e.g., no `session.snapshot`+`events.subscribe` federation extensions).**
Trigger: user has an old remote binary and declines/can't auto-upgrade. Expected: explicit
capability negotiation — local server checks remote `protocol`/`capabilities` (Pong already carries
`capabilities`, per scout) before attempting federation mount; falls back to a clear error ("remote
herdr vX does not support workspace federation, upgrade or use classic --remote attach") rather than
a confusing partial mount. Severity: **Blocker** (silent skew = corrupted/garbage session state).

**S3.2 — Remote and local support federation but at different sub-versions of the federation
protocol itself (post-ship iteration).** Trigger: local ships federation v2 (adds a field), remote
still on v1. Expected: additive-only wire changes (scout confirms `#[serde(default)]` pattern used
elsewhere) with graceful degradation, not a hard version-lock across the whole app protocol.
Severity: **Major** (design-time constraint, not urgent for v1 but must be planned so v1 doesn't
paint into a corner).

**S3.3 — Mid-session remote server self-updates (warm handoff on the remote host) while federated.**
Trigger: remote host runs its own `herdr self-update`. Expected: local federation link either
survives (remote's own warm handoff is local-to-remote-host and orthogonal to the SSH link) or
detects the disconnect/reconnect cleanly and re-syncs via a fresh `session.snapshot`. Breaks if
unhandled: local server treats the blip as a hard failure and tears down the whole remote workspace
instead of reconciling. Severity: **Major**.

---

## 4. ID collision / namespacing

**S4.1 — Local and remote both have a workspace `w1` (guaranteed, since both start their
`AtomicU64` counter at 1).** Trigger: any two-server mount. Expected: every id crossing the
federation boundary — workspace, tab, pane, terminal_id, in `Method`/`ResponseResult`/
`Subscription`/`EventEnvelope`/`ClientMessage::AttachTerminal`/`ObserveTerminal`/`ControlTerminal`
— is remapped through a stable prefix (e.g., `r:<host-key>:w1`) before it ever reaches local
`AppState`/sidebar/API responses. Breaks if unhandled (confirmed, not hypothetical, per scout): two
distinct panes/workspaces silently collapse into one in `AppState.workspaces: Vec<Workspace>`
indexed by plain `usize` — input gets routed to the wrong pane, potentially typing a user's remote
SSH-session command into a local shell or vice versa. Severity: **Blocker**.

**S4.2 — Same remote host mounted twice (two separate `herdr --remote` invocations to the same
`user@ip`, e.g. user retries after thinking it failed).** Trigger: double-mount. Expected: dedupe
by (host, session-name) — either refuse the second mount with "already mounted" or explicitly
support multiple independent sessions on one host with a session-name discriminator in the
namespace prefix. Breaks if unhandled: id prefix collides on itself (`r:1.2.3.4:w1` twice) — same
class of bug as S4.1 but self-inflicted. Severity: **Major**.

**S4.3 — Namespace prefix must survive a full round trip through JSON API responses that external
tooling (scripts using `herdr` CLI subcommands / the JSON-RPC API) may already parse assuming bare
ids.** Trigger: existing automation calls `workspace.list` and expects ids in the pre-federation
shape. Expected: decide and document whether federated ids are a breaking format change to the
public API contract, or whether federation is opt-in enough that non-federating users see unchanged
ids. Severity: **Major** (public-contract compatibility, not just internal correctness).

---

## 5. Auth / permissions

**S5.1 — SSH auth prompts (password, 2FA, key passphrase) needed but the local server process has
no TTY to prompt through.** Trigger: remote host requires interactive auth and no ssh-agent/key is
pre-loaded. Expected: mount attempt fails fast with a clear, actionable message surfaced in the
local UI (not a hung connection); federation should document/require key-based non-interactive auth
(agent-forwarded or plain keyfile) as a hard prerequisite, same as any headless SSH automation.
Severity: **Blocker** for UX (must fail loud, not hang) — the auth *requirement itself* is
reasonable to punt to "use ssh-agent," not a blocker to design around.

**S5.2 — Local API socket / client socket permission model (0600, owner-only, no app-layer auth)
now also gates access to remote-mirrored data.** Trigger: another local user account on the same
machine. Expected: unchanged — federation inherits the exact same "socket ownership = trust"
boundary already used for local sessions (scout confirms no auth layer exists anywhere; nothing new
to build, but nothing new to rely on either). Severity: **Minor** (no regression vs. today, but
worth stating explicitly since it's now proxying a *second* machine's data through that boundary).

**S5.3 — Remote host's herdr server has broader/narrower filesystem permissions than expected
(e.g., remote panes can read remote-host secrets the local user assumed were sandboxed).**
Trigger: user assumes federation is "read-only preview," but a mounted remote pane has full
read-write remote-shell access. Expected: UI must not visually imply reduced trust/capability for
remote panes vs local — they are exactly as powerful as a normal SSH session to that host, no more,
no less. Severity: **Minor** (documentation/UX clarity, not a code defect) — flag so it's not
mis-sold as a sandbox.

---

## 6. Remote server crash / restart

**S6.1 — Remote herdr server process crashes (panic, OOM-killed) mid-session.**
Trigger: remote server dies. Expected: SSH channel/API-client connection drops; local server
detects it (not a silent hang), marks the remote workspace as "disconnected" in the sidebar (visual
state, not removal), stops delivering stale frames, offers reconnect/relaunch. Breaks if unhandled:
without an explicit disconnected state, panes look "frozen but seemingly fine," and any queued local
input silently vanishes into a dead channel. Severity: **Blocker** (must have an explicit
disconnected UI state — this is the single most common failure mode of any network feature).

**S6.2 — Remote server restarts and comes back up, but its own workspace/pane state changed
(e.g., it crash-recovered from cold snapshot, losing/renaming panes) while local still holds stale
mirrored state.** Trigger: reconnect after S6.1. Expected: local re-fetches a fresh
`session.snapshot` and reconciles (diff, not blind append) against its stale mirror — added,
removed, and renamed panes must all be handled, and remapped ids for panes that no longer exist must
be retired cleanly (attached local UI focus, if it was on a now-gone remote pane, must fail over
sanely). Severity: **Major**.

**S6.3 — Remote server was mid-way through its own warm handoff (self-update) exactly when the
federation link was established or torn down.** Trigger: race between remote-side warm handoff and
local mount/unmount. Expected: treated as a transient disconnect (S6.1/S6.2 path), not a distinct
failure mode. Severity: **Minor** (should fall out of S6.1/S6.2 handling if those are solid — flag
as a test case, not new design).

---

## 7. Network drop + reconnect

**S7.1 — Transient network blip (WiFi drop, VPN reconnect) during an otherwise healthy session,
seconds-scale.** Trigger: brief connectivity loss. Expected: SSH's own keepalive/multiplexing
(`ServerAlive*`, control-socket reuse — already configured per scout) either survives it
transparently or triggers a clean detect-and-reconnect cycle without user action; in-flight input
is not silently dropped mid-keystroke (either buffered briefly or the pane visibly shows
"reconnecting"). Severity: **Blocker** (this is the single most likely real-world event once the
feature ships — must not require a full unmount/remount for a 2-second WiFi hiccup).

**S7.2 — Extended network outage (minutes) — does the mount auto-retry forever, or give up?**
Trigger: laptop closed / long tunnel. Expected: bounded retry with backoff, then an explicit
"disconnected, click to reconnect" terminal state rather than an infinite silent retry loop chewing
CPU/battery or spamming SSH auth attempts (which can trigger fail2ban-style lockouts on the remote).
Severity: **Major**.

**S7.3 — Reconnect races with the user manually clicking "reconnect" while an auto-retry is also
in flight.** Trigger: impatient user. Expected: idempotent reconnect (single in-flight attempt,
button reflects that state) — same class of bug as S1.2. Severity: **Minor** (same fix as S1.2,
just a different trigger).

---

## 8. Concurrent input

**S8.1 — User has both a local pane and a remote-mounted pane visible/focused rapidly alternating
(tab switching) while typing fast.** Trigger: quick tab switches with buffered keystrokes.
Expected: input routing keyed by the namespaced pane id (S4.1's fix) must never leak a keystroke
meant for the just-left pane into the newly-focused one, regardless of relative local-vs-remote
latency — i.e., focus-switch must be a hard barrier, not a race with in-flight input delivery.
Severity: **Blocker** (this is exactly the bug class S4.1 exists to prevent, called out separately
because it's the concrete user-visible symptom).

**S8.2 — Same remote pane is somehow attached from two local vantage points (e.g., federation
mount AND a classic `herdr --remote` full attach to the same remote server simultaneously).**
Trigger: user runs classic `--remote` attach to a host that's already federation-mounted locally.
Expected: either detect and block the second attach mode with a clear message, or support both
cleanly (remote server already supports multiple `App`-mode clients per scout's `ClientConnectionMode`
enum) — but input from both must not race-corrupt the same remote pane's PTY. Severity: **Major**
(plausible operator mistake, not exotic).

**S8.3 — Copy-paste / bracketed-paste large blocks into a remote pane over high latency.**
Trigger: paste a multi-KB block. Expected: paste is sent as one atomic unit over the tunnel (not
byte-by-byte input events that could interleave with output frames), consistent with how bracketed
paste already works for local panes. Severity: **Minor** (should inherit existing local behavior if
transport is a faithful relay; flag as a regression-test case).

---

## 9. Persistence / resume — across local restart AND remote restart

**S9.1 — Local herdr server restarts (crash or self-update) while a remote workspace was mounted.**
Trigger: local `herdr self-update` or crash-restart. Expected: cold-snapshot path (confirmed
data-only/portable per scout) extended with a `remote_origin` field on `WorkspaceSnapshot`; on
local restore, federated workspaces re-establish their SSH+API-client link instead of respawning a
local PTY. If the remote is unreachable at restore time, the workspace reappears in a
"disconnected, tap to reconnect" state rather than being silently dropped or erroring the whole
session restore. Severity: **Blocker** (confirmed as the natural extension point by the scout — but
must actually be built, it's not free).

**S9.2 — Local restarts via WARM handoff (not cold restart) while a remote workspace is mounted.**
Trigger: local self-update using the SCM_RIGHTS fd-passing path. Expected: explicitly excluded —
confirmed architecturally impossible to carry a remote-backed pane's "connection" through fd-passing
(no local fd exists to pass). Federated workspaces must be deliberately dropped from the warm-handoff
set and instead reconnected via the cold-path logic (S9.1) immediately after the new process comes
up. Breaks if unhandled: warm handoff code, written assuming every pane has a `master_fd: RawFd`,
either panics or silently loses the remote workspace with no reconnect attempt. Severity: **Blocker**
(explicit scout finding: "architecturally blocked," must be designed around, not discovered at
runtime).

**S9.3 — Remote host restarts (reboot) — should the remote-side herdr session resume its own
panes, and does local federation resume matching them back to the same namespaced ids?**
Trigger: remote host reboots, remote herdr auto-starts fresh (or via its own systemd unit).
Expected: remote-side resume is the remote server's own existing cold-snapshot logic (unaffected by
federation); local must detect it's now talking to a *fresh* remote session (different snapshot
identity/boot marker) and decide: re-namespace as effectively a new mount, vs. attempt continuity.
Ambiguous by default — recommend re-namespacing (treat as new mount, old namespace tombstoned) to
avoid silently splicing unrelated post-reboot panes into stale local id slots. Severity: **Major**.

---

## 10. Multiple simultaneous remotes

**S10.1 — User mounts 3+ different remote hosts at once.** Trigger: multi-remote fleet use case
(explicitly the herdr-fleet-style motivation noted in blindspot synthesis). Expected: namespace
prefix must be per-host (not just a boolean "is remote" flag), sidebar groups distinctly per host
(reusing the existing worktree_space grouping pattern per scout), and each host's SSH
link/EventHub-merge/reconnect state machine is fully independent — one host's crash/reconnect storm
(S7.2) must not affect another's. Severity: **Blocker** for the multi-remote case specifically (if
only single-remote is in scope for v1, downgrade this to explicit scope-cut, but it must be a
stated decision, not an accident of the data model).

**S10.2 — Resource/thread scaling: N remote mounts × M panes each, all with independent SSH
channels/keepalives.** Trigger: many remotes. Expected: bounded resource use — shared SSH
multiplexing control-socket per host (already used) keeps N mounts to N connections not N×M; local
server doesn't spawn an unbounded thread-per-pane if channel multiplexing is implemented on top of
one connection per host. Severity: **Major** (ties into §11 resource exhaustion).

---

## 11. Security — untrusted remote data injected into local UI/protocol

**S11.1 — Malicious or compromised remote host returns a crafted `SessionSnapshot` with
adversarial field values (huge strings, path traversal in `cwd`/`identity_cwd`, malformed
`custom_name` with terminal control sequences / ANSI escape injection).** Trigger: compromised or
untrusted remote. Expected: all remote-sourced strings rendered in the local sidebar/UI must be
sanitized/escaped exactly as any other untrusted-input rendering path would be (strip or neutralize
control sequences before they reach the ratatui buffer) — a remote workspace name should not be
able to execute an ANSI escape sequence that manipulates the local terminal (cursor jumps, hidden
text tricks, OSC52 clipboard injection via a fake "workspace name"). Severity: **Blocker** — this is
a genuine new trust boundary the app has never had (today all rendered state is self-generated by
the same process). Scout confirms zero app-layer auth; SSH is the only boundary, and even a
legitimately-authed remote could be compromised independently.

**S11.2 — Remote sends an oversized frame/graphics payload to exhaust local memory.**
Trigger: malicious or buggy remote server sends >32MB Kitty graphics or floods `MAX_FRAME_SIZE`
boundary repeatedly. Expected: the existing `MAX_FRAME_SIZE`/`MAX_GRAPHICS_FRAME_SIZE` caps (already
enforced in `wire.rs` for normal client-server framing) must be enforced identically on the
federation ingestion path — a remote is not a more-trusted peer than a normal client just because
it's "your own" other server. Severity: **Blocker** if the federation code path bypasses existing
frame-size validation (must confirm it reuses the same parser, not a hand-rolled one).

**S11.3 — Remote clipboard OSC52 payload is relayed into the local system clipboard
automatically.** Trigger: a remote pane's shell (or malicious remote) emits an OSC52 clipboard-set
sequence. Expected: same policy as local clipboard OSC52 handling today — if local already
requires/offers a confirmation or is trust-scoped to the local session, remote-sourced clipboard
writes should not silently get MORE trust than local ones; consider whether remote-origin clipboard
writes deserve additional friction (this is genuinely a product decision, flag to user rather than
silently deciding). Severity: **Major** (clipboard hijacking is a known real-world attack vector via
malicious SSH hosts/terminal output — not hypothetical).

**S11.4 — Remote workspace's namespace-prefixed ids are used as-is in a shell/format string
somewhere in error messages, logs, or notifications shown to the user.** Trigger: crafted
`custom_name` or `agent_name` designed to look like a legitimate local notification/prompt
(phishing the user into thinking a local action is happening). Expected: any UI surface that shows
"Workspace X" text must make remote-origin unambiguous (badge/prefix/color) so a malicious remote
can't spoof-name itself to look like a trusted local workspace. Severity: **Major**.

---

## 12. Resource exhaustion (many remote panes)

**S12.1 — Remote workspace has dozens of panes/tabs (e.g., remote already had a large existing
herdr session before federation mounted it).** Trigger: mount a heavily-used remote session.
Expected: local server must not eagerly instantiate N full remote-backed `Pane`/ghostty-terminal
objects (with their own scrollback buffers, detection tasks) for every remote pane regardless of
visibility — lazy-attach (only fully hydrate panes that become visible/focused, keep others as
lightweight metadata-only entries from the snapshot) is likely required to avoid O(all remote panes)
memory/CPU cost on mount. Severity: **Major** (directly threatens the "small/medium" feasibility of
the render/detect slices if applied unconditionally to every remote pane rather than the visible
ones).

**S12.2 — Screen-based agent detection runs per-pane on a timer; naively running it for every
remote pane (visible or not) multiplies local CPU cost by remote pane count.** Trigger: many
detection tasks spawned for remote panes matching local's `spawn_basic_detection_task` cadence.
Expected: detection cadence/scope for remote panes should be throttled or scoped to
visible/attended panes only, not blindly ported 1:1 from the local-pane assumption that panes are
few and cheap to poll. Severity: **Minor-Major** (perf tuning, but could be a real regression if
naively implemented — flag for load-testing before ship).

**S12.3 — SSH tunnel thread/connection exhaustion if per-pane connections are used instead of
multiplexed per-host.** Trigger: see S10.2, applies at single-host scale too if the chosen transport
design opens one SSH channel per pane rather than multiplexing over the existing control-socket.
Expected: reuse the `ManagedSshOptions.control_path` multiplexing already in place — one underlying
connection, many logical channels. Severity: **Major** (architecture decision, must be made
correctly at design time — retrofitting multiplexing later is expensive).

---

## 13. Clipboard / keybinding bridging

**S13.1 — Remote pane's keybindings conflict with local herdr chrome keybindings (e.g., a
detach/prefix key that's meaningful to local herdr sidebar navigation but the user expects it to go
to the remote shell).** Trigger: user focused on a remote-mounted pane presses a herdr-reserved
key. Expected: same precedence rules as any local pane today (herdr already handles this — chrome
keys vs. pane-forwarded keys) should apply identically to remote panes; no new ambiguity should be
introduced just because the pane is remote-backed. Severity: **Minor** (should fall out of reusing
existing pane-input routing if S4.1/S8.1 are solved correctly — flag as regression test).

**S13.2 — `HERDR_REMOTE_KEYBINDINGS=<local|server>` mode (already exists for classic `--remote`)
— does federation need an equivalent per-remote-workspace setting, and can it differ per mounted
host?** Trigger: user wants remote-host-native keybindings for one federated workspace but
local-style for another. Expected: decide explicitly (likely: always local-style for federated
panes, since they coexist with local panes in one chrome — the "server" keybinding mode conceptually
doesn't fit a federated-pane-among-many-panes model the way it does whole-screen `--remote` attach).
Severity: **Minor** (scope/design decision, low risk either way).

**S13.3 — Local clipboard paste into a remote pane (opposite direction of S11.3).**
Trigger: user copies locally, pastes into remote-mounted pane. Expected: should just work as a
normal bracketed paste over the relay (see S8.3) — no special handling needed beyond making sure
the transport for standard input includes paste bytes. Severity: **Minor**.

---

## 14. Agent-status staleness

**S14.1 — Remote agent's foreground-process-based detection signal (the non-portable one) is
unavailable until a relay mechanism is built (per scout, this is a real gap, not just "relay a
value").** Trigger: any remote pane running an agent CLI (claude/codex/etc.) that relies on
process-table detection for state transitions (idle/running/waiting-for-input). Expected for v1:
document that remote agent status is screen-text-only (works, per scout, since ghostty grid
detection is source-agnostic) with reduced fidelity vs. local (missing the low-latency
process-exit/foreground-change signal) — OR build the remote-side probe-and-relay (medium-large
per scout). This is a scope decision, not silently accept degraded status without telling the user.
Severity: **Major** (silently-wrong agent status — e.g., showing "idle" when actually
"waiting-for-approval" — actively misleads a user monitoring multiple agents, which is core to
herdr's value prop).

**S14.2 — Status staleness compounds with network latency/reconnect (S7.x) — a remote pane can
show a stale "running" status for the entire duration of a network blip.** Trigger: brief network
drop while an agent is mid-run remotely. Expected: disconnected state (S6.1/S7.1) should visually
distinguish "last known status" from "live status" (e.g., dim/gray the badge during disconnect)
rather than presenting stale data as current truth. Severity: **Major**.

**S14.3 — Detection relay message itself gets lost/reordered under the same multi-source EventHub
sequencing gap as S2.3.** Trigger: agent status event races with a snapshot reconciliation (S6.2).
Expected: status events use the same as-of-sequence discipline as everything else once
multi-source EventHub exists — no separate ad hoc channel that bypasses ordering guarantees.
Severity: **Blocker** (falls out of S2.3's fix, called out here because agent-status is the feature
users will notice being wrong fastest).

---

## 15. Shutdown / cleanup ordering

**S15.1 — User quits the local herdr client/server entirely (not just unmounting one remote)
while remote workspaces are mounted.** Trigger: full local shutdown. Expected: SSH
channels/API-client connections for all mounted remotes are closed cleanly (not orphaned), and per
S9.1 a cold snapshot capturing "these remotes were mounted" is written so a future restart can
offer to reconnect. Remote-side herdr servers must NOT be killed as a side effect (they're
independent long-lived sessions the user may want to keep running headless) — shutdown must
disconnect, not terminate, the remote. Severity: **Blocker** (killing someone's remote session on
local quit would be a severe, surprising data-loss-adjacent bug).

**S15.2 — Ordering between "stop accepting new local input for a remote pane" and "flush
in-flight output already buffered from the remote" during unmount.** Trigger: unmount while remote
is actively producing output (e.g., a build still running). Expected: in-flight buffered output is
either fully drained/rendered before teardown or explicitly discarded with the pane clearly shown as
"disconnected" — no half-rendered/torn frame left on screen as the last visible state. Severity:
**Minor**.

**S15.3 — Local server crash (not clean quit) while remote mounted — does the remote-side herdr
server notice the SSH link died and clean up its own client-connection state (avoid leaking a
zombie `App`-mode client slot)?** Trigger: local process SIGKILL'd or OOM-killed. Expected: SSH
disconnect propagates to the remote server's socket read returning EOF/error, and the remote
server's existing client-disconnect cleanup path (already exists for any client) runs — this should
be free if federation's API-client role is treated as just another socket client from the remote
server's point of view. Severity: **Minor** (should be free if architecture is right; called out to
make sure it's verified, not assumed).

---

## Top 10 must-handle (blockers, priority order)

1. **S4.1 — id namespacing across every wire boundary.** Without this, federation misroutes input
   between local and remote panes on day one. Root blocker for everything else.
2. **S8.1 — input-routing focus barrier.** The concrete user-visible failure mode of #1; must be
   verified as a hard barrier, not just "ids are unique now."
3. **S2.3 / S14.3 — multi-source EventHub sequencing.** Confirmed unbuilt (single global sequence
   counter); agent-status and snapshot-reconciliation correctness both depend on it.
4. **S6.1 — explicit disconnected state.** Most common real-world failure (any network blip);
   without it, frozen panes look silently fine.
5. **S7.1 — transient-drop resilience without full remount.** WiFi/VPN blips will happen
   constantly in real usage; remount-on-every-blip makes the feature unusable.
6. **S9.2 — warm handoff must exclude federated panes.** Confirmed architecturally impossible via
   `SCM_RIGHTS`; must be designed around explicitly or it crashes/silently drops on local self-update.
7. **S9.1 — cold snapshot/resume for federated workspaces across local restart.** Natural
   extension point per scout, but must actually be built, not assumed free.
8. **S11.1 — sanitize untrusted remote strings before rendering.** New trust boundary the app has
   never had; ANSI-escape injection via workspace/pane names is a real, cheap attack.
9. **S11.2 — enforce existing frame-size caps on the federation ingestion path.** Must confirm
   reuse of existing validated parser, not a bypass.
10. **S15.1 — local shutdown must disconnect, never kill, remote sessions.** Severe surprise/
    data-loss risk if violated; also blocks safe reconnect-after-restart (S9.1) if the remote's own
    session was killed.

## Feasibility / scope-cut signal

No single scenario here makes Tier-2 infeasible — all scouts already independently rated it
"large, new subsystem," and nothing above contradicts that. But three clusters compound the
estimate beyond what the architecture scouts alone suggested:

- **Security (§11)** is a genuinely new requirement, not previously in scope of the 4 scouts (they
  focused on transport/data-model feasibility, not adversarial-remote hardening). If the remote
  host is assumed fully trusted (e.g., "it's always my own machine"), most of §11 can be
  downgraded from Blocker to Minor — **this is a scope decision the plan should make explicitly**,
  not discover during implementation.
- **Multi-source EventHub correctness (§2, §9, §14)** is the connective tissue behind three
  different top-10 blockers (2.3, 6.2, 14.3) — it is arguably the single largest undersized line
  item across all 4 scout reports (each mentions it in passing as "would need real changes" without
  sizing it). Recommend a dedicated design spike before committing to a Tier-2 timeline.
- **Warm-handoff exclusion (S9.2)** touches existing local-only code (`src/server/handoff.rs`) that
  has no concept of "this pane can't participate" — that carve-out needs its own review pass, not
  just a federation-side fix, since the handoff code currently assumes every pane has a `master_fd`.

Recommend: if timeline pressure forces a cut, ship Tier-1 (pane-tunnel, screen-detection-only,
per blindspot synthesis) first and treat this scenario doc as the acceptance-criteria backlog for a
follow-up Tier-2 phase — do not attempt Tier-2 without dedicated design time on EventHub
multi-sourcing and an explicit trust-model decision for remote data.

Status: DONE
