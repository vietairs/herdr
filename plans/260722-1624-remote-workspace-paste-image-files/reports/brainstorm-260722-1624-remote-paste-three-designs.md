# Brainstorm: paste image/file into a mounted-remote pane — 3 architectures

Date 2026-07-22. Read-only design report, no code changed. Ground truth = the 4-scout blindspot
findings (`plans/260722-1624-remote-workspace-paste-image-files/reports/blindspot-260722-1624-remote-paste-federation-findings.md`)
plus targeted verification of `protocol/mod.rs`, `codec.rs`, `serve.rs`, `client.rs`,
`clipboard_image.rs`, `pane_source.rs`, `id.rs`, `sanitize.rs`, `unix.rs` (ssh spawn).

Extra facts verified beyond the brief (feed into the fork below):
- Codec is `serde_json`, **externally tagged** enum (`{"Clipboard": {...}}`), not bincode — confirmed
  at `codec.rs:55-59`. New enum variants are wire-safe to *add* to the Rust type; they are only unsafe
  to **send** to a peer that doesn't know them (decode fails -> `CodecError::Malformed` on that frame).
- Handshake already negotiates a `BTreeSet<Capability>` (`protocol/mod.rs:59-61`) with the doc
  "unknown capabilities are ignored, not fatal" (`:41-43`) — this is the existing, idiomatic way to
  gate a brand-new message type without touching `FEDERATION_PROTOCOL_VERSION`.
- The precedent for extending an *already-shipped* struct is literally in the code:
  `AgentStatusMessage.agent: Option<String>` uses `#[serde(default, skip_serializing_if =
  "Option::is_none")]` with a comment saying handshake version-equality already guarantees both
  peers agree the field exists; `default` only keeps *pre-field recorded fixtures* decodable
  (`protocol/mod.rs:230-240`). Same idiom applies to extending `ClipboardMessage`.
- `TerminalChannelMessage::{Open,Output,Input,Resize,Close}` all carry `terminal_id` + a
  `mount_generation` fence checked in `serve.rs` (`handle_inbound`, "stale traffic from a prior mount
  generation must never be routed"). `ClipboardMessage` carries **neither**. Any pane-targeted
  clipboard/file message needs both, or a stale/torn-down mount can inject into the wrong generation.
- SSH transport uses `ControlMaster=auto` (`unix.rs:325,1205,2701`) — a **second** ssh session to the
  same host reuses the existing control socket (near-zero extra handshake cost), which matters for
  proposal C.
- `ClipboardMessage`'s own doc comment ("distinguish its own echoed clipboard from a genuinely remote
  one") describes **OS clipboard sync** semantics (copy-on-remote -> paste-locally), not "stage a file
  for a specific pane" semantics. Repurposing it conflates two different features — flagged in the
  framing critique below.

---

## Proposal A — Fire-and-forget remote-side injection (finish the dormant rails as-is)

**Essence:** wire up exactly what P8/M9 left stubbed — client sends bytes+metadata once, remote
stages *and* pastes into the pane itself; no reply ever crosses back.

**Data flow:** capture (client/mod.rs:1805 or drag-drop 1823-1874, generalized) -> extend
`ClipboardMessage` with `#[serde(default)]` fields `terminal_id: Option<String>`,
`mount_generation: Option<u64>`, `filename: Option<String>`, `extension: Option<String>` -> client
calls `send_local_clipboard_to_remote()` (client.rs:452, finally gets its call site) on the mount's
`out_tx` -> wire up `serve.rs:441-446`'s `_ => return` arm to actually handle `Clipboard` (drop
`#[allow(dead_code)]` on `handle_inbound`) -> remote stages via a **generalized** `stage()` (new fn,
not `clipboard_image::stage`, no png fallback) -> remote directly writes the bracketed-paste-wrapped
staged path into the target terminal's PTY input using the same call the remote host's own local
paste already uses (i.e. remote-side reuses its own `pane.rs:3072` injection, not a new code path).

**Wire-compat:** no `FEDERATION_PROTOCOL_VERSION` bump. `ClipboardMessage` already ships in v3;
new fields are `#[serde(default)]` so a peer running an older build of this same struct still decodes
(fields just come back `None`/absent) — matches the `AgentStatusMessage.agent` precedent exactly.

**Staging + paste text:** remote `$TMPDIR/herdr-clipboard-images-<uid>/` equivalent (generalized dir
name), atomic `create_new` + retry loop reused verbatim. Paste text = the **remote** absolute path,
bracketed-paste wrapped, written directly into PTY input — never round-trips to the client at all.

**Who injects:** remote server, unconditionally, targeting `terminal_id` from the message.

**Failure modes:** mount drops mid-transfer -> `out_tx.send` on a torn-down mount is a silent
`SendError`, swallowed today (`let _ =` at client.rs:459) -> **user sees nothing, paste vanishes**.
Oversize -> already capped by `Channel::Clipboard` 16 MiB at codec/frame level, connection likely
killed rather than a clean per-message error. Remote disk full -> stage `io::Error`, nothing to send
it back to, dead-ends in a `tracing::warn!` at best. Unknown extension -> fixed for real (drop the
png fallback), but the *failure itself* (reject vs. best-effort) has no way to reach the user's
screen. Control chars/traversal in filename -> must be stripped/basename-only before touching the
filesystem, since this is remote-received attacker-shaped(-ish) data reaching a real `fs::write`.

**Security:** same trust tier as everything else in federation (SSH-authenticated, not sandboxed).
New surface: the remote now executes a filesystem write **and a PTY input write** triggered purely by
an unauthenticated (at app layer) peer message with no correlation/ack — slightly worse than today's
`AgentStatus`/`Event` messages because it has a side effect on the pane's input stream, matching the
existing `Terminal::Input` trust level but without that message's generation fencing unless added.

**Arbitrary files:** in scope by construction (no image-only capture assumption baked into the wire
message), but the *client-side capture* is still image-only until `client/mod.rs` is generalized —
that's a separate, pre-existing gap this proposal doesn't fix by itself.

**Tests (loopback):** `clipboard_message_with_terminal_id_stages_and_injects_remote_paste`;
`clipboard_message_without_target_terminal_is_dropped_not_panicked`;
`stale_mount_generation_clipboard_message_is_ignored` (once generation fencing is added);
`clipboard_message_oversize_payload_rejected_at_channel_cap`;
`clipboard_message_unknown_extension_no_longer_falls_back_to_png`.

**Cost:** ~6-9 files (`protocol/mod.rs`, `serve.rs`, `client.rs`, new generalized stage module,
`sanitize.rs` additions, client capture generalization, loopback tests). Rough 350-500 LOC incl.
tests. **Riskiest thing:** no ack path at all — every failure mode above is invisible to the user by
design; this is the "ship the happy path, hope it's enough" option.

---

## Proposal B — Correlated stage-then-inject (mirror `SplitPaneRequest`/`Response`)

**Essence:** remote only stages bytes and hands back a path; client injects exactly like it already
does for local paste. Zero new PTY-write code, all new code is on the staging/correlation side.

**Data flow:** capture (same as A) -> client mints `request_id: u64` -> new variant
`FederationMessage::ClipboardStageRequest { request_id, terminal_id, mount_generation, filename,
extension, payload }` on `out_tx` -> client stores `request_id` in a pending-map (new, mirrors however
`SplitPaneResponse` is correlated back to its caller — same pattern as `client.rs`'s split-response
handling) -> `serve.rs` handles the new variant, calls generalized `stage()`, replies
`ClipboardStageResponse { request_id, Ok(remote_path) | Err(StageError) }` -> client's pending-map
resolves -> on `Ok`, client does **exactly** what local paste does today: `RawInputEvent::Paste
(remote_path)` -> `app/mod.rs:1711` -> `runtime.try_send_paste` -> `pane.rs:3072`. On `Err`, surface a
toast/status line naming the reason.

**Wire-compat:** **new enum variants**, not a reuse of `ClipboardMessage`. No `FEDERATION_PROTOCOL_
VERSION` bump — instead add `Capability::ClipboardFileTransfer` to the handshake's negotiated set
(the codebase's own documented mechanism for "unknown capabilities are ignored, not fatal"). Client
checks the agreed-capability set before sending the new request; if absent, falls back to today's
"can't paste into remote pane" behavior (or a clear error) instead of sending a variant an old peer
can't decode. This is more machinery than A but is the *documented, load-bearing* compat mechanism —
A's `#[serde(default)]` trick only works because it's extending a struct that's already in every
build; a brand-new variant has no such luxury.

**Staging + paste text:** identical staging location/hardening to A. Paste text = remote path,
**client-side** bracketed-paste wrap, reusing the existing local-paste code path verbatim (this is
the point of the proposal — the injection code is untouched, only its input source changes from "read
local temp file path" to "path arrived over the wire").

**Who injects:** local client, targeting the pane exactly as local paste does today (pane already
selected/focused client-side; no new "which pane" resolution needed beyond what local paste has).

**Failure modes:** mount drops mid-transfer -> pending request never resolves -> add a client-side
timeout (new: nothing like this exists yet) -> explicit "paste failed: mount disconnected" surfaced
to user, **not silent**. Oversize -> reject *before* sending (client already checks 16 MB
client-side per the existing image guard; generalize that check to the new payload class) plus a
server-side belt-and-suspenders check that returns `Err(TooLarge)` instead of a raw connection kill.
Remote disk full -> `Err(io error)`, shown to user via the same toast path. Unknown extension -> same
fix as A, but now the *rejection* is a clean `Err` variant surfaced synchronously, not a swallowed
log. Control chars/traversal/unicode in filename -> sanitize server-side before `fs::write` (basename
only, strip control bytes per `sanitize.rs`'s existing filter), AND sanitize the *returned path*
client-side before it's typed into a PTY (`sanitize::sanitize_remote_string` equivalent applied to
`remote_path` before it becomes paste text — this is the literal "remote path echoed back is
remote-sourced text" risk called out in the brief).

**Security:** same SSH trust tier; the *added* surface vs. A is a request_id space (bounded, u64,
already the pattern `SplitPaneRequest` uses) and a pending-request map bounded by outstanding
requests (cap it, e.g. reject a second in-flight stage per terminal). Meaningfully better than A: no
side effect (PTY write) happens without the client's own explicit, already-battle-tested injection
path running, so there's exactly one PTY-write code path in the whole codebase, not two.

**Arbitrary files:** same scope note as A — wire+stage generalizes cleanly, client capture still needs
separate generalization work.

**Tests (loopback):** `stage_request_response_round_trip_returns_remote_path`;
`stage_request_times_out_when_mount_drops_mid_transfer`;
`stage_request_rejected_when_capability_not_negotiated_with_old_peer`;
`stage_response_error_surfaces_disk_full_reason`;
`stale_generation_stage_request_is_ignored_and_responds_nothing`.

**Cost:** ~8-11 files (2 new protocol variants + `Capability` enum entry, `serve.rs` handler,
`client.rs` pending-map + timeout, generalized stage module, `sanitize.rs`, injection call-site reuse
in `app/mod.rs`, capture generalization, tests). Rough 500-700 LOC incl. tests — more than A because
of the request/response correlation machinery, but every added LOC buys observability A doesn't have.
**Riskiest thing:** the pending-request map is new state that must survive/clean up correctly across
mount teardown (`end_federation_mount` aborts tasks — a request awaiting reply must not hang forever
or panic on a dropped channel).

---

## Proposal C — Out-of-band transfer over a second SSH channel, decoupled from the federation duplex

**Essence:** don't put file bytes on the federation control/event stream at all; open a second,
`ControlMaster`-multiplexed SSH invocation dedicated to bulk transfer, keeping the interactive duplex
(terminal output, events) free of large-payload head-of-line blocking.

**Data flow:** capture (same as A/B) -> instead of `out_tx.send(FederationMessage::...)`, client spawns
`ssh -S <same ControlPath> <host> herdr internal stage-clipboard-file --terminal <id>` (new CLI
subcommand, not a new federation message) with payload piped over stdin -> remote binary (invoked
fresh, not through the running `serve.rs` federation loop at all) reads stdin, calls the same
generalized `stage()`, and either (a) injects server-side like A by talking to the *already-running*
remote herdr server over its own local Unix socket / JSON API (the remote host already runs its own
herdr server that owns the pane — this subcommand becomes a thin client of that server's own API,
per the runtime/client boundary guardrail: "shared runtime/session fact... exposed through the JSON
API/event path when practical"), or (b) prints the staged remote path to stdout, which the client's
ssh subprocess captures and injects locally like B.

**Wire-compat:** **the federation binary protocol is untouched** — no new `FederationMessage` variant,
no `ClipboardMessage` change, no `FEDERATION_PROTOCOL_VERSION` question at all, because this isn't on
that protocol. The only new "wire" is a CLI arg contract + stdin/stdout on the new subcommand, versioned
independently (and simpler to change later, since it's not framed/length-prefixed like the duplex).

**Staging + paste text:** same staging dir/hardening as A/B, but staged by invoking the **remote's own
already-running server's API** rather than by teaching the federation duplex protocol handler to do
filesystem work — this keeps `serve.rs`'s federation-message handler free of a new side-effect class.
Paste text = remote path, injected via whichever of (a)/(b) is chosen.

**Who injects:** flavor (a) remote server (like A, but via its own real JSON API instead of an ad hoc
federation-message side effect); flavor (b) local client (like B, but data arrived via ssh stdout
instead of the federation frame reader).

**Failure modes:** mount drops mid-transfer -> the ssh subprocess is independent of the mount's drive
task, so this is actually **more resilient** than A/B to a mount teardown racing the transfer (it's a
separate process with its own exit code) but that also means it does *not* get cleanly cancelled by
`end_federation_mount()` today — a new lifecycle needs to link it to mount teardown (kill the
subprocess), or an orphaned upload can outlive its mount. Oversize -> no 16 MiB `Channel` cap applies
at all (this is a real relaxation, good for "arbitrary files" which may legitimately be bigger than a
clipboard image, bad if you wanted the existing cap as a safety net — must add an explicit new cap).
Remote disk full / unknown extension / control chars -> identical fixes to B, enforced in the new
subcommand instead of `serve.rs`. Additional new failure mode neither A nor B has: **ssh spawn
failure** (rare, but a new process-spawn surface, needs its own error path distinct from "mount
disconnected").

**Security:** *reduces* federation-duplex attack surface (nothing new decodes through `codec.rs`) but
*adds* a brand-new remote-invokable CLI entry point (`herdr internal stage-clipboard-file`) that must
itself validate the terminal id belongs to a real pane, be unreachable except over the SSH transport
already used for mounting (no additional listening port), and not be discoverable/callable outside
that SSH context. Net surface is arguably similar, differently shaped — worth a second opinion, not
obviously safer or less safe than A/B.

**Arbitrary files:** best-fitted of the three for genuinely large arbitrary files (no 16 MiB channel
cap, no shared-stream contention) but that's also scope creep if the real ask is "an image and the
occasional small text/PDF."

**Tests:** this is the one proposal that does **not** fit the loopback harness (`LoopbackFederationServer`
only exercises the federation duplex, not a spawned CLI subprocess over real/fake ssh) — would need a
new, separate test harness (spawn the subcommand against a local Unix-socket test server, or an
integration test with an actual `sshd` fixture if one exists in `just test`). This alone is a material
cost the other two don't have. Rough tests: `stage_subcommand_writes_file_to_staging_dir_with_original_
name`; `stage_subcommand_rejects_unknown_terminal_id`; `stage_subcommand_flavor_a_injects_via_local_
api`; `stage_subcommand_orphaned_process_killed_on_mount_teardown` (once that lifecycle link exists).

**Cost:** ~10-14 files (new CLI subcommand + its own arg parsing, generalized stage module, either a
new local JSON API endpoint (flavor a) or client-side ssh-stdout capture + injection reuse (flavor b),
mount-teardown-to-subprocess lifecycle link, new test harness scaffolding). Rough 600-900 LOC incl.
tests/harness. **Riskiest thing:** it is architecturally the most "correct" per the runtime/client
boundary guardrail (flavor a treats staging as a real server API action) but is also the one most
likely to become its own multi-week feature once the new test harness and lifecycle-linking are
accounted for — real risk of scope creep past what "paste a file" needed.

---

## Comparison

| | A: fire-and-forget inject | B: correlated stage+client-inject | C: out-of-band ssh channel |
|---|---|---|---|
| Wire change | additive fields on shipped struct, `#[serde(default)]`, no version bump | 2 new variants + new `Capability`, no version bump | none — off-protocol entirely |
| User feedback on failure | none (silent) | yes (toast/timeout) | yes (process exit code) |
| New PTY-write code path | yes (remote-side) | no (reuses local inject) | yes (a) or no (b) |
| Size cap | existing 16 MiB `Channel::Clipboard` | existing 16 MiB, enforceable both ends | none by default — must add one |
| Loopback-testable | yes | yes | **no** — needs new harness |
| Security surface added | side-effecting message, no ack, no gen fence unless added | correlated but bounded, one inject path | new CLI entrypoint, off the audited codec |
| Reversibility | easy — revert 2 files | medium — revert protocol+capability+client state | hardest — new subcommand becomes a used interface fast |
| Rough LOC | 350-500 | 500-700 | 600-900 |

## Recommendation

**Smallest v1: Proposal A**, scoped to images only (reuse existing capture, skip generalizing
arbitrary-file capture), with the png-fallback bug fixed and a `mount_generation` fence added — but
be explicit with the user that "silent failure on mount drop" is an accepted v1 gap, not an oversight.

**Most complete / what I'd actually build: Proposal B.** It's ~150-200 LOC more than A for a real
reason: it turns "did the paste work" from an invisible coin-flip into an observable, retryable
outcome, and it reuses the local-paste injection path 1:1 instead of adding a second PTY-write
surface. It also fits the loopback harness cleanly (A does too, C doesn't), and its wire-compat story
(capability-gated new variants) is the codebase's own documented mechanism, not an improvised one.

**Proposal C is not recommended right now.** It solves a problem the user didn't ask about (large
arbitrary-file transfer needing to bypass a 16 MiB interactive-stream cap) at a cost the user didn't
budget for (new subcommand, new test harness, new lifecycle-linking). Revisit it only if "arbitrary
file" turns out to mean "hundred-MB build artifacts," not "the occasional screenshot or PDF."

## Framing critique — what I think is wrong or underspecified in the brief

1. **Reusing `ClipboardMessage` (Proposal A's premise) is a feature-conflation smell.** Its own doc
   comment describes OS clipboard sync (remote-copy -> local-paste), not "stage a file for a specific
   pane." The brief treats "the rails already exist, wire them up" as obviously correct; I'd push back
   — B's brand-new, purpose-built variants are barely more wire-compat work (capability gate vs.
   `#[serde(default)]`) and don't semantically overload a struct meant for something else.
2. **"No `FEDERATION_PROTOCOL_VERSION` bump needed" is true only for A.** B needs a new negotiated
   `Capability`; C needs nothing on this protocol at all. Framing it as one flat fact undersells the
   real fork.
3. **The brief bundles "image" and "arbitrary file" as one scope**, but the client-side *capture* path
   is image-only today (`read_image_file_from_terminal_drop`) — there is no generic file-drop/paste
   capture at all. This is really two features (generalize local capture; add federation transport).
   All three proposals above generalize the *wire/stage* side but leave capture generalization as a
   distinct, unscoped chunk of work the brief doesn't size.
4. **Server-side injection (A, and C flavor a) is arguably the architecturally correct answer**, not
   the "worse" one — the PTY lives on the remote host, so the remote server directly acting on its own
   pane is more consistent with "state/runtime lives where the resource lives" than a client
   pretending to know a remote path and typing it in from across an SSH link. B's appeal is testability
   and failure-visibility, not architectural purity. Worth being honest that B is a pragmatic choice,
   not an obviously "more correct" one.

## Unresolved questions

- Is arbitrary-file support actually needed in v1, or is "image paste over federation" (matching
  today's local-only capability) the real ask, with generic file paste a fast-follow?
- What's acceptable UX on failure — silent (A), toast/timeout (B), or is a retry affordance required?
- Any real-world payload sizes expected above 16 MiB (PDFs, build artifacts)? Decides whether C's
  uncapped-transfer property is worth its cost.
- Should the staged remote file's name be preserved for the agent's benefit, or is an opaque
  `client-<id>-clipboard-<nanos>.<ext>` fine (today's local behavior)?
- Who owns the remote staging directory's lifecycle across repeated mounts/unmounts of the same host —
  same 24h sweep as today, or tied to mount lifetime?
