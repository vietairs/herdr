# Root-cause lens reports — remote-workspace resize/split re-render bug

Source: Workflow run wf_329adfc1-16d, 3 parallel investigators (opus, high effort). Advisor adjudication (fable) NOT yet run.

## Lens: RENDER / RESYNC / MIRRORING — how a mounted federation client obtains screen content, and what (does not) force a repaint when local geometry changes. All claims are source-read at /Users/hvnguyen/Projects/herdr-worktrees/remote-workspace-resize-rerender; nothing was executed (no cargo/test/live run).

**Strongest:** Hypothesis 1 combined with 2: `PaneTerminal::resize` deliberately skips the blank-bottom replay-recovery for remote-backed panes (src/pane/terminal.rs:1399, gated by `self.io.is_remote()` at src/pane.rs:2837-2843) on the stated assumption that "the remote repaint be the sole source of truth" (src/pane/terminal.rs:1364-1373) — but no code path ever asks the remote for a repaint. The federation protocol carries full screen content exactly once, in the Open acknowledgement's ScrollbackReplay (src/server/federation_accept.rs:685-698), a repeat Open is a no-op (src/server/federation_accept.rs:679-681), and the only resync added by b5cb8ce8 is metadata-only and fires only on structural pane/tab EventKinds (src/remote/federation/client.rs:500-509, 820; src/remote/federation/reducer.rs:8). The remote-side recovery repaint that does run cannot cross the wire because it is a grid write, not a PTY byte (src/pane/terminal.rs:1421 vs the raw-PTY tee at src/pane.rs:2814-2821). Net: on any local geometry change (window resize, split add, split close — all reaching rt.resize via src/ui/panes.rs:186/203/248/280) the remote pane is left showing ghostty's raw reflow, with the ONLY possible correction being a repaint the remote app volunteers on SIGWINCH. Idle shells/agents do not volunteer one, which is exactly "stale until something else forces a redraw". IMPORTANT REFUTATION of the lens's Q2 premise: the reducer does NOT apply remote frames into a locally-sized grid and there is NO generation/epoch guard discarding mismatched frames. reducer.rs is metadata-only (src/remote/federation/reducer.rs:1-8; a grep for rows/cols/grid/dims in that file returns nothing), and TerminalChannelRouter::route_inbound forwards every Output byte unconditionally with no dimension or generation check (src/remote/federation/client.rs:432-438). The stale rendering is a missing-repaint problem, not a discarded-frame problem.

### H1 [high] The local replay-recovery that repairs a blanked grid after resize is explicitly disabled for remote-backed panes, and the "remote repaint" it defers to is never actually requested — so a remote pane keeps ghostty's raw reflow (blank/garbled bottom) until the remote app volunteers a repaint.

**Mechanism:** PaneRuntime::resize (src/pane.rs:2824-2850) calls self.terminal.resize(..., self.io.is_remote()) at src/pane.rs:2837-2843. In PaneTerminal::resize the replay-recovery gate is `let replay_ansi = if !is_remote_backed && ... active_screen()==Primary && bottom_before_resize` (src/pane/terminal.rs:1399-1408); the recovery write `core.terminal.scroll_viewport_bottom(); core.terminal.write(ansi)` at src/pane/terminal.rs:1418-1423 therefore never runs for a remote pane. The doc comment at src/pane/terminal.rs:1364-1373 justifies this as "let the remote repaint be the sole source of truth" — but no code anywhere requests that repaint (see hypothesis 2), and the local nudge primitive is a no-op for remote: `PaneRuntimeIo::Remote(_) => {}` at src/pane.rs:1288. The three reported triggers (window resize, add split, close split) all funnel through the same call: src/ui/panes.rs:186-191 / 203-208 / 248-253 / 280-285 call rt.resize with the new inner rect every render pass.

**Evidence:**
- src/pane/terminal.rs:1399 — `let replay_ansi = if !is_remote_backed`
- src/pane/terminal.rs:1364-1373 — comment pinning the skip to "the remote repaint be the sole source of truth"
- src/pane.rs:2837-2843 — `self.terminal.resize(rows, cols, cw, ch, self.io.is_remote())`
- src/pane.rs:1288 — `PaneRuntimeIo::Remote(_) => {}` in nudge_child_redraw_after_handoff
- src/pane/terminal.rs:4464-4485 — test `resize_recovery_skips_replay_for_remote_backed_pane` pins the skip
- src/ui/panes.rs:166-211 — resize_tab_panes drives rt.resize from layout geometry each pass

### H2 [high] The federation protocol has NO screen-content resync at all: full pane content crosses the wire exactly once per terminal (the Open acknowledgement's ScrollbackReplay), everything after is incremental raw PTY bytes, and the resync added by b5cb8ce8 is metadata-only and fires only on structural pane/tab events — never on geometry change.

**Mechanism:** open_terminal on the serving side emits one `TerminalChannelMessage::Open { replay: ScrollbackReplay { bytes } }` (src/server/federation_accept.rs:685-698) whose bytes come from scrollback_replay -> TerminalRuntime::handoff_history_ansi (src/server/federation_actor.rs:217-219, 347-362). After that the OutputPump streams only `subscribe_output_bytes` (src/server/federation_actor.rs:211-216), i.e. raw PTY deltas. A second Open cannot re-trigger the replay: `if pumps.contains_key(&terminal_id) { return; }` (src/server/federation_accept.rs:679-681). The only resync message pair is SnapshotRequest/SnapshotResponse carrying a MountSnapshot, which is session metadata (workspaces/tabs/panes) — reducer.rs states "No pane bytes here (P5 on focus) — metadata only" (src/remote/federation/reducer.rs:8). It is sent only when `is_structural_event_kind(kind)` (src/remote/federation/client.rs:500-509, helper at 820). Nothing in src/remote/federation/ or src/server/federation_*.rs contains the words repaint/redraw/force_render (grep returned nothing).

**Evidence:**
- src/server/federation_accept.rs:685-698 — Open{replay} is the one and only full-content frame
- src/server/federation_accept.rs:679-681 — duplicate Open is a no-op, so re-Open cannot re-replay
- src/server/federation_actor.rs:347-362 — scrollback_replay == handoff_history_ansi
- src/remote/federation/reducer.rs:8 — "No pane bytes here … metadata only"
- src/remote/federation/client.rs:500-509 — SnapshotRequest fires only on structural EventKind
- src/remote/federation/client.rs:820 — is_structural_event_kind (PaneCreated/Closed/Moved, TabCreated/Closed/Moved) — no geometry kind

### H3 [medium] Even on the serving host, the resize-recovery repaint is structurally incapable of reaching the client, because recovery writes go into the server's own ghostty grid while the client is fed only the raw PTY output tee.

**Mechanism:** FederationCommand::Resize applies `runtime.resize(rows, cols, 0, 0)` on the serving side (src/server/federation_actor.rs:236-249), which goes through the normal local path — so the server's own PaneTerminal::resize DOES run the replay-recovery branch (is_remote_backed=false) and writes the recovered ANSI with `core.terminal.write(ansi.as_bytes())` (src/pane/terminal.rs:1421), directly into the server's emulator. The federation client's byte feed is `PaneRuntime::subscribe_output_bytes` (src/pane.rs:2818-2821), documented as "the exact bytes process_pty_bytes consumes, not rendered frames". A grid write is not a PTY byte, so the server's recovery never enters the tee. Likewise, the terminal_responses (in-band size report) the server generates ARE written to its own PTY (src/pane.rs:1249-1258), so the remote app is correctly notified — but the app's response is the only thing that can ever repaint the client.

**Evidence:**
- src/server/federation_actor.rs:236-249 — server-side resize path
- src/pane/terminal.rs:1418-1423 — recovery writes to core.terminal, not the PTY
- src/pane.rs:2814-2821 — subscribe_output_bytes doc: raw PTY bytes, not rendered frames
- src/pane.rs:1240-1272 — PaneRuntimeIo::resize dispatch (Actor writes terminal_responses to the PTY; Remote drops them)

### H4 [medium] The wire resize is fire-and-forget with no ACK and a silent server-side drop path, so a resize can be lost outright with no retry and no client-visible error.

**Mechanism:** RemoteTerminalSourceHandle::resize does `let _ = self.out_tx.send(...Resize{cols, rows})` and returns (src/remote/federation/pane_source.rs:196-203) — the protocol has no Resize response; route_inbound explicitly treats Resize as outbound-only (src/remote/federation/client.rs:442-444). On the server, FederationCommand::Resize returns early and silently when `!lease.is_mounted_controller(epoch, connid)` (src/server/federation_actor.rs:243-245; predicate at src/server/federation_lease.rs:148-157), and also silently when the terminal_id resolves to nothing (src/server/federation_actor.rs:246). PaneRuntime::resize on both ends early-returns when the size tuple is unchanged (src/pane.rs:2830-2833), so a dropped resize is never re-sent by a later identical render pass — the local side believes it is already at that size. This does not produce discarded frames (there is no such guard), it produces a permanent local/remote dimension disagreement.

**Evidence:**
- src/remote/federation/pane_source.rs:196-203 — fire-and-forget Resize send
- src/remote/federation/client.rs:442-444 — Resize is never received back; no ACK exists
- src/server/federation_actor.rs:243-248 — silent drop on lease mismatch or unknown terminal
- src/server/federation_lease.rs:148-157 — is_mounted_controller exact (epoch, connid) match
- src/pane.rs:2830-2833 — `if self.current_size.get() == size { return; }` suppresses any resend

### H5 [low] Pixel cell metrics are lost across the federation wire (forced to 0), so the in-band size report the remote app receives carries zero pixel dimensions.

**Mechanism:** TerminalChannelMessage::Resize carries only {terminal_id, mount_generation, cols, rows} — no cell_width_px/cell_height_px (src/remote/federation/pane_source.rs:178-204, where both px params are `_`-ignored). The server then calls `runtime.resize(rows, cols, 0, 0)` (src/server/federation_actor.rs:247). The in-band size report generated from that resize (shape visible in the local test expectation `\x1B[48;40;100;720;900t`, src/pane/terminal.rs:4455-4460) will therefore report 0x0 pixels to the remote app. Apps that size graphics or layout from pixel geometry get wrong values on a federated pane, which can present as incorrectly-sized content.

**Evidence:**
- src/remote/federation/pane_source.rs:178-204 — resize signature ignores _cell_width_px/_cell_height_px; wire message has no px fields
- src/server/federation_actor.rs:247 — `runtime.resize(rows, cols, 0, 0)`
- src/pane/terminal.rs:4450-4461 — local in-band size report includes real pixel dims

**Evidence gaps:**
- NOTHING WAS EXECUTED. No cargo build, no cargo test, no live herdr/federation session. Every claim is static source reading of the worktree; the causal chain from 'replay-recovery skipped' to the user's screenshot is inferred, not observed.
- Not empirically confirmed which remote app was in the reported pane, nor whether it repaints on SIGWINCH. If the remote app DOES repaint reliably, hypothesis 1 predicts only a transient (1 RTT) glitch, not the reported persistent staleness — a live repro reading the pane before/after resize (e.g. `herdr agent read <pane> --source detection --format ansi`) would discriminate.
- Not verified whether the lease check (src/server/federation_actor.rs:243) actually rejects in practice for a normal mount — i.e. whether hypothesis 4 is live or purely theoretical. Instrumenting/logging the drop, or checking whether SendInput (same guard) works in the same session (typing works => lease is held => resize is NOT being dropped, which would eliminate hypothesis 4), would settle it.
- Did not trace `handoff_history_ansi` returning None for alternate-screen panes (test at src/pane.rs:3716-3724) through to a live mount. If true in production, a remote pane running a full-screen TUI gets an EMPTY replay even at mount time — a related but separate defect worth confirming.
- Did not audit whether a herdr TUI attached on the serving host concurrently fights the federated resize: src/ui/panes.rs resizes every runtime each render pass from ITS OWN geometry, which would ping-pong the shared pane's dimensions. Whether the reported setup has a local TUI attached on the remote host is unknown.
- Did not examine src/server/render_stream.rs or src/protocol/render_ansi.rs; they were listed as key files but appear to serve the non-federation attach/render-stream path. If federation ever routes through them, conclusions about the byte path would need revisiting.

## Lens: Wire/protocol path: resize command from the mounted client through serialization, accept-loop routing, lease authorization, and the remote pane runtime — plus what (if anything) sends fresh content back.

**Strongest:** Hypothesis 1: the wire has no post-resize repaint. `Resize` is fire-and-forget into `runtime.resize(rows, cols, 0, 0)` (src/server/federation_actor.rs:247); the only full-paint frame in the protocol is `Open{replay}` emitted once per terminal at hydrate time (src/server/federation_accept.rs:687-703), and the live channel carries only raw PTY delta bytes (src/server/federation_accept.rs:736-770). Meanwhile the client deliberately disables its own resize-recovery repaint for remote-backed panes (src/pane/terminal.rs:1399, gated `!is_remote_backed`) on the stated assumption that "the remote repaint [is] the sole source of truth" (src/pane.rs:2832-2835) — but no code path makes the remote produce that repaint. So a resized-but-quiescent (or only partially repainting) remote screen yields zero frames and the client shows the stale, merely-reflowed grid until unrelated output forces a redraw. This explains all three reported triggers (window resize, add split, close split) equally, and explains why local panes are unaffected.

### H1 [high] Nothing forces a repaint after a resize. The federation protocol carries a full paint ONLY in the `Open{replay}` frame; after that the remote streams raw PTY delta bytes. So a resize whose remote app does not spontaneously repaint produces zero frames and the client keeps showing the old-size grid — and unlike a local pane, the client's own resize-recovery replay is deliberately disabled for remote-backed panes.

**Mechanism:** Client: PaneRuntime::resize (src/pane.rs:2824-2851) resizes the local mirror emulator and calls io.resize. PaneTerminal::resize (src/pane/terminal.rs:1374-1436) does `core.terminal.resize(cols, rows, ...)` at :1412, but the resize-recovery replay at :1399 is gated `if !is_remote_backed && ...` — for a remote pane the branch is skipped, so when the reflowed bottom goes blank nothing repaints it locally. The comment at src/pane.rs:2832-2835 and src/pane/terminal.rs:1370-1373 states the design intent: "let the remote repaint be the sole source of truth". But on the server side, Resize only reaches `runtime.resize(rows, cols, 0, 0)` (src/server/federation_actor.rs:240-248) — i.e. a TIOCSWINSZ/SIGWINCH on the remote PTY — and the only thing that ever streams back is `output_pump` draining the raw PTY output tee (src/server/federation_accept.rs:736-770; subscription is `runtime.subscribe_output_bytes()` via FederationCommand::SubscribeOutput, src/server/federation_actor.rs:209-215). The one full-paint frame, `Open{replay: ScrollbackReplay}`, is emitted only inside `open_terminal` (src/server/federation_accept.rs:671-703, replay from `scrollback_replay` -> `handoff_history_ansi`, src/server/federation_actor.rs:347-362) — i.e. once per terminal at mount/hydrate time. There is no repaint/resync request bound to geometry: the client's only resync path, `SnapshotRequest`, is triggered by inbound structural server events (src/remote/federation/client.rs:486-510), never by a local geometry change. Net: resize crosses the wire correctly, then the client waits for bytes that a quiescent (or partially-repainting) remote app never sends.

**Evidence:**
- src/pane/terminal.rs:1399 — `let replay_ansi = if !is_remote_backed && ...` disables the local resize-recovery repaint for remote panes
- src/pane/terminal.rs:1418-1423 — the skipped branch is exactly the "bottom went blank after resize, replay recent ANSI" recovery local panes rely on
- src/server/federation_actor.rs:240-248 — FederationCommand::Resize does `runtime.resize(rows, cols, 0, 0)` and nothing else; no replay, no forced redraw, no reply frame
- src/server/federation_accept.rs:629-643 — inbound TerminalChannelMessage::Resize only forwards FederationCommand::Resize; no outbound frame is produced
- src/server/federation_accept.rs:687-703 — the `Open{replay}` frame (the only full paint on the wire) is emitted solely by `open_terminal`
- src/server/federation_accept.rs:736-770 — `output_pump` streams only `tee::drain_available(&mut rx)` PTY deltas as `Output` frames
- src/remote/federation/client.rs:486-510 — SnapshotRequest resync is driven by structural inbound events, not by geometry
- src/pane.rs:2832-2835 — comment: remote repaint is "an async round trip over the federation link ... not a same-machine SIGWINCH repaint", justifying skipping local recovery

### H2 [medium] When the remote herdr server has its own foreground client (its own TUI attached), its render loop re-resizes every pane to the REMOTE client's geometry on every frame, silently clobbering the federation-applied size — and the cell-pixel asymmetry guarantees the dedupe never damps the ping-pong.

**Mechanism:** On the remote, `render_and_stream` renders each App-mode client with `resize_panes = is_foreground` (src/server/headless.rs:3765-3782), and `render_virtual_with_runtime_registry` then calls `crate::ui::compute_view_with_cell_size` (src/server/render_stream.rs:293-297), which resizes every pane runtime to the remote client's own rects (src/ui/panes.rs:275-286). That runs after — and repeatedly against — the federation resize applied at src/server/federation_actor.rs:247. Worse, the federation path passes cell pixels `0, 0` while the remote's own render passes its real HostCellSize, so the size tuple compared at src/pane.rs:2827-2830 always differs and neither side is ever deduped away: both resizes execute every frame, each issuing a real SIGWINCH. The remote app then repaints for the REMOTE geometry and those bytes land in a client mirror sized for the LOCAL geometry — wrong-width/wrapped content that persists. Note the benign case: with no clients attached, `resize_panes = self.app.state.view.pane_infos.is_empty()` (src/server/headless.rs:3742) is false after the first virtual frame, so the clobber does not occur.

**Evidence:**
- src/server/headless.rs:3765-3782 — per-client render passes `is_foreground` as `resize_panes`
- src/server/render_stream.rs:293-297 — `if resize_panes { compute_view_with_cell_size(...) }`
- src/ui/panes.rs:275-286 — compute_pane_infos calls `rt.resize(inner_rect.height, inner_rect.width, cell_size.width_px, cell_size.height_px)`
- src/server/federation_actor.rs:247 — `runtime.resize(rows, cols, 0, 0)` (cell px hardcoded 0)
- src/pane.rs:2827-2830 — dedupe compares the full `(rows, cols, cell_width_px, cell_height_px)` tuple, so 0-px vs real-px never match
- src/server/headless.rs:3742 — client-less path only resizes on the very first virtual frame

### H3 [medium] Cell pixel dimensions are structurally dropped on the wire, so the remote PTY's winsize always reports 0 pixel width/height. Any remote app that sizes output by pixels (kitty graphics/sixel/image protocols) renders at the wrong scale after every geometry change and never corrects.

**Mechanism:** `RemoteTerminalSourceHandle::resize` takes `_cell_width_px`/`_cell_height_px` and discards them; the `TerminalChannelMessage::Resize` variant has no pixel fields at all (src/remote/federation/pane_source.rs:178-204; src/remote/federation/protocol/mod.rs:174-179). The receiving side therefore has no pixel information to forward and hardcodes zeros (src/server/federation_actor.rs:247; src/remote/federation/serve.rs:96 + :472-478 has the same 3-arg `resize(terminal_id, cols, rows)` shape). Those zeros land in `PtySize { pixel_width, pixel_height }` at src/pty/actor.rs:186-191 (and the unix twin), so the remote child sees a winsize with zero pixel extents on every resize.

**Evidence:**
- src/remote/federation/pane_source.rs:178-204 — `_cell_width_px`/`_cell_height_px` unused; only `{terminal_id, mount_generation, cols, rows}` is sent
- src/remote/federation/protocol/mod.rs:174-179 — Resize variant carries no pixel fields
- src/server/federation_actor.rs:247 — zeros passed for both pixel args
- src/remote/federation/serve.rs:96 and :472-478 — host trait `resize(&self, terminal_id, cols, rows)`, same pixel-free shape
- src/pty/actor.rs:186-191 — cell px feed `PtySize.pixel_width/pixel_height`

### H4 [high] Argument order/units are correct end-to-end and the mount lease never silently drops a legitimate resize — these two suspected causes are eliminated.

**Mechanism:** Order: client `rt.resize(inner_rect.height, inner_rect.width, ...)` (rows, cols) at src/ui/panes.rs:203-208/280-285 -> TerminalRuntime::resize(rows, cols, ...) src/terminal/runtime.rs:277-279 -> PaneRuntime::resize(rows, cols, ...) src/pane.rs:2824 -> TerminalSource::resize(rows, cols, ...) src/terminal/source.rs:33-40, src/pane.rs:1240-1268 -> serialized as NAMED struct fields `cols`/`rows` (src/remote/federation/pane_source.rs:196-203), so transposition is impossible across serde. Receiver destructures by name (src/server/federation_accept.rs:629-643; src/remote/federation/serve.rs:472-478) and `federation_actor.rs:247`'s `runtime.resize(rows, cols, 0, 0)` matches `TerminalRuntime::resize(&self, rows, cols, cell_width_px, cell_height_px)` at src/terminal/runtime.rs:277. Border/gap inclusion is handled identically for local and remote panes via `pane_inner_rect` + `stable_terminal_inner_rect` (src/ui/panes.rs:198-201), so no remote-specific off-by-one. Lease: authorization is per-CONNECTION (`is_mounted_controller(epoch, connid)`, src/server/federation_actor.rs:236-239 / src/server/federation_lease.rs Phase::Mounted), and every pane on a mount multiplexes the single mount out_tx (src/remote/federation/pane_source.rs:59-62), so a split adding a second pane cannot change controller identity or cause a drop. Resizes are also not debounced or coalesced on the wire: the only dedupe is `if self.current_size.get() == size { return; }` on genuinely unchanged local geometry (src/pane.rs:2828), and `out_tx` is an unbounded channel with one frame per change.

**Evidence:**
- src/ui/panes.rs:203-208 and :280-285 — (height, width) i.e. (rows, cols)
- src/terminal/runtime.rs:277-279 and src/pane.rs:2824 — (rows, cols, cell_w, cell_h)
- src/remote/federation/pane_source.rs:196-203 — named-field serialization `cols`, `rows`
- src/server/federation_accept.rs:629-643 and src/remote/federation/serve.rs:472-478 — named-field destructuring
- src/server/federation_actor.rs:247 vs src/terminal/runtime.rs:277 — call matches signature
- src/server/federation_actor.rs:236-239 — lease check is `is_mounted_controller(epoch, connid)`, per connection not per pane
- src/remote/federation/pane_source.rs:59-62 — all panes multiplex ONE mount out_tx
- src/pane.rs:2828 — the only dedupe, keyed on actual local geometry

### H5 [low] Against a standalone `herdr federation-serve` host, every Input/Resize/Open frame is silently dropped after a re-mount within the same client process, because the client mints an incrementing mount_generation while the serve host hard-filters on the constant 1.

**Mechanism:** `FederationClient::mount` sets `generation = self.next_generation.fetch_add(1, SeqCst) + 1` (src/remote/federation/client.rs:207-212), so the first mount is 1 and any subsequent mount in the same process is 2, 3, ... Every outbound Resize is tagged with that generation (src/remote/federation/pane_source.rs:199). `serve::handle_inbound` drops any terminal message whose generation differs from the module constant `MOUNT_GENERATION: u64 = 1` (src/remote/federation/serve.rs:44 and :448-451). The co-located accept path (`herdr --remote` onto a live server) does NOT apply this filter (no generation check anywhere in src/server/federation_accept.rs — it only stamps its own outbound frames at :696/:759/:924), so this affects only the serve-host transport; listed for completeness since it would present as "resize does nothing" after a reconnect.

**Evidence:**
- src/remote/federation/client.rs:207-212 — incrementing generation per mount
- src/remote/federation/serve.rs:44 — `const MOUNT_GENERATION: u64 = 1;`
- src/remote/federation/serve.rs:448-451 — `if term_msg.mount_generation() != MOUNT_GENERATION { return; }`
- src/server/federation_accept.rs — no inbound generation validation (grep shows mount_generation only at :696, :759, :924 outbound stamps)

**Evidence gaps:**
- Nothing was executed — no build, no tests, no live mount. Every claim is static read of the worktree at /Users/hvnguyen/Projects/herdr-worktrees/remote-workspace-resize-rerender. A live check (mount a remote workspace, resize, and trace whether a Resize frame is sent AND whether any Output frame follows) would separate H1 from H2 decisively.
- Unknown whether the reported repro had a TUI client attached on the REMOTE host. That single fact decides whether H2 (remote-side geometry clobber, src/server/headless.rs:3765-3782) is active or inert.
- No tracing/logging exists on the client-side resize send path (src/remote/federation/pane_source.rs:196 uses `let _ = out_tx.send(...)`, errors swallowed), so a torn-down/replaced mount out_tx would drop resizes with zero observable signal. Not verifiable without adding instrumentation.
- Did not determine what initial rows/cols `build_remote_pane` is called with (src/app/creation.rs:736-760 takes them as parameters); if they already equal the local geometry at mount time, the dedupe at src/pane.rs:2828 means the very first geometry is never announced to the remote at all. Caller of `materialize_federation_mount` was not traced.
- Did not measure Resize frame volume during a continuous window drag — each compute_view frame with changed geometry emits one unbounded-channel frame with no coalescing (src/pane.rs:2828 is the only damping), which could matter for link congestion but was not observed.

## Lens: Local geometry / TUI side: how window resize, split add, and split close reach a pane runtime's (cols,rows), and how a federated (mounted) pane diverges from a local PTY pane on that path.

**Strongest:** Hypothesis 2: the local resize path is fully shared and does fire for mounted panes, and the local mirror grid IS resized — the single local-side divergence is `is_remote_backed` at src/pane/terminal.rs:1399, which disables the replay-recovery repaint that makes a LOCAL pane look correct in the very frame the resize is applied. A remote pane's post-resize frame is therefore whatever ghostty's reflow left (blank/cropped for alternate-screen agents), and recovery is outsourced entirely to an authoritative repaint arriving back over the federation link. Combined with hypothesis 3 (nothing arbitrates the mounted client's Resize against the remote server's own per-frame compute_view resize, and the mounted client sends 0x0 cell pixels), that repaint can be absent, late, or laid out for the wrong geometry — which is exactly "stale/incorrectly-sized content that persists until something else forces a redraw".

### H1 [high] TerminalSource::resize IS called for remote panes on all three triggers; there is no remote/mounted branch or guard anywhere on the local resize path. So "the resize never fires" is NOT the bug.

**Mechanism:** Every foreground render pass runs compute_view with resize_panes=true (src/server/render_stream.rs:297 -> src/ui.rs:135 compute_view_with_cell_size -> src/ui.rs:211 compute_view_internal -> src/ui/tab_surface.rs:29 compute_tab_surface -> src/ui/panes.rs:215 compute_pane_infos). compute_pane_infos derives each pane's target rect from ws.layout.panes(area) (src/ui/panes.rs:265), subtracts chrome via pane_inner_rect/apply_pane_chrome (src/ui/panes.rs:46, :88) and the 1-col scrollbar gutter (src/ui/panes.rs:33 stable_terminal_inner_rect, :139 stable_scrollbar_gutter), then calls rt.resize(inner.height, inner.width, cell_px…) at src/ui/panes.rs:277 (and :245 for zoomed, :185/:202 for background tabs via resize_tab_panes). The only guard is app.direct_attach_resize_locks (src/ui/panes.rs:276), which is populated solely by `herdr attach`-style direct terminal-attach clients (src/server/headless.rs:2706, :1517) — never by federation. Remote runtimes are registered in the same TerminalRuntimeRegistry as local ones at mount time (src/app/creation.rs:679 self.terminal_runtimes.insert, built by build_remote_pane at :737/:753 TerminalRuntime::spawn_remote), so runtime_for_tab_pane / runtime_for_pane_in_workspace resolve them identically. From there PaneRuntime::resize (src/pane.rs:2824) resizes the local mirror and forwards to io.resize -> RemoteTerminalSourceHandle::resize (src/remote/federation/pane_source.rs:178), which emits TerminalChannelMessage::Resize on the mount tunnel.

**Evidence:**
- src/ui/panes.rs:265-285 (compute_pane_infos loop, rt.resize at :277, only guard is direct_attach_resize_locks at :276)
- src/ui/panes.rs:166-211 (resize_tab_panes for background tabs, same single guard at :185/:202)
- src/server/render_stream.rs:284-300 (render_virtual_with_runtime_registry calls compute_view before drawing every frame)
- src/app/creation.rs:667-683 and :737-780 (remote panes get real TerminalRuntimes inserted into self.terminal_runtimes)
- src/pane.rs:2824-2850 (PaneRuntime::resize -> self.terminal.resize(...) + self.io.resize(...))
- src/remote/federation/pane_source.rs:178-204 (Resize crosses the wire)
- src/server/headless.rs:2706 and :1517 (direct_attach_resize_locks is a terminal-attach concept only)

### H2 [high] The one real local-side local-vs-remote divergence is the resize replay-recovery heuristic, which is deliberately disabled for remote-backed panes. A local pane repaints itself synchronously in the SAME frame as the resize; a remote pane paints nothing and waits for a round trip that may never produce a repaint. This is the strongest explanation for "stale content until something else forces a redraw".

**Mechanism:** PaneTerminal::resize takes an is_remote_backed flag (src/pane/terminal.rs:1374-1380), fed by self.io.is_remote() (src/pane.rs:2841, PaneRuntimeIo::is_remote at src/pane.rs:1162 -> true only for PaneRuntimeIo::Remote, src/pane.rs:1148). At src/pane/terminal.rs:1399 the replay_ansi capture is gated `if !is_remote_backed && active_screen == Primary && bottom_before_resize`; at :1417-1424, when the post-reflow bottom is blank, a local pane re-writes its captured recent ANSI into the grid, so the frame rendered immediately afterwards (compute_view resizes and render_panes draws in the same render_virtual call, src/server/render_stream.rs:297 then :309) already shows correct content. For a remote pane replay_ansi is None, so the frame drawn right after the resize shows whatever ghostty's reflow left — for an alternate-screen agent that is a cropped/blank grid. Correction depends entirely on bytes arriving back over the federation link (remote SIGWINCH repaint -> output tee -> RemoteTerminalSourceHandle reader task on_read -> render_dirty). If the remote app does not emit a full repaint (shell prompt only, app ignoring SIGWINCH, resize dropped/reverted remotely), the mirror stays visually wrong indefinitely, and the user's "something else forces a redraw" is in practice "I typed something and the remote app repainted". Note the behaviour is pinned by a test: src/pane/terminal.rs:4465 resize_recovery_skips_replay_for_remote_backed_pane.

**Evidence:**
- src/pane/terminal.rs:1364-1372 (doc comment stating the remote skip is intentional)
- src/pane/terminal.rs:1399-1408 (`let replay_ansi = if !is_remote_backed && ...`)
- src/pane/terminal.rs:1417-1424 (blank-bottom replay write, unreachable for remote)
- src/pane.rs:2836-2842 (is_remote() threaded in)
- src/pane.rs:1162-1170 (is_remote true only for PaneRuntimeIo::Remote)
- src/pane/terminal.rs:4465-4487 (test pinning the skip)
- src/remote/federation/pane_source.rs:117-131 (the only path that can repaint a remote mirror is inbound bytes -> on_read)

### H3 [medium] There IS a local mirror grid and it IS resized (so "mirror never resized" is not the bug) — but the mirror resize and the wire resize are unarbitrated dual writes to the same logical terminal size, and on the remote host the mounted client's Resize competes with the remote server's own per-frame compute_view. If the remote host has any foreground client, our Resize is reverted within one remote frame and the repaint we receive is laid out for the REMOTE's geometry, which renders as permanently wrong wrapping in the local mirror.

**Mechanism:** Local: PaneRuntime::resize (src/pane.rs:2824) resizes the local ghostty mirror via self.terminal.resize (src/pane/terminal.rs:1412 core.terminal.resize(cols, rows, ...)) AND sends the wire Resize. Remote: src/server/federation_accept.rs:629-643 routes it to FederationCommand::Resize, handled at src/server/federation_actor.rs:236-248 with `runtime.resize(rows, cols, 0, 0)` on the remote server's own App runtime. That same remote runtime is also resized every frame by the remote server's own render loop if it has a foreground client (src/server/render_stream.rs:297 -> src/ui/panes.rs:277) and on every ClientResize (src/server/headless.rs:3054 -> :1004 resize_shared_runtime_to_effective_size -> :1028 compute_view). Nothing arbitrates the two writers, and PaneRuntime::resize's `if self.current_size.get() == size { return; }` (src/pane.rs:2829) makes the last writer win. Also note the mounted client's resize forces cell pixel size to 0x0 on the remote (federation_actor.rs:247) while the local mirror is resized with the real host cell size (src/ui/panes.rs:280-281) — a permanent tuple mismatch that guarantees each side's resize looks like a change to the other, i.e. sustained ping-pong when a remote foreground client exists.

**Evidence:**
- src/pane.rs:2824-2850 (dual write: mirror + wire, with equality early-return at :2829)
- src/pane/terminal.rs:1412-1414 (local mirror grid actually resized)
- src/server/federation_actor.rs:236-248 (`runtime.resize(rows, cols, 0, 0)` — remote applies mounted client's size, zero cell px)
- src/server/render_stream.rs:297 + src/ui/panes.rs:277 (remote server re-resizes its own runtimes every foreground frame)
- src/server/headless.rs:1004-1046 (remote-side ClientResize path also resizes and forces full redraw)
- src/ui/panes.rs:280-281 (local side passes real cell_size px)

### H4 [high] Split add and split close DO take a different route from a raw window resize, but not in a way that skips the resize: window resize gets an explicit resize + forced full redraw for every client, while split add/close only re-layout lazily on the next render pass and get no forced full redraw. For a mounted workspace the split path additionally lands asynchronously via an AppEvent, so the layout change and the remote's knowledge of it are decoupled.

**Mechanism:** Window resize: ServerEvent::ClientResize -> self.resize_shared_runtime_to_effective_size() (src/server/headless.rs:3054) which runs compute_view (:1028/:1035) and then calls client.request_full_redraw() for every client (:1046), explicitly because "Shared runtime size changes affect pane wrapping" (:1039-1042). Split add/close: no resize_shared_runtime_to_effective_size call anywhere on those paths; the layout mutation (src/app/api/panes.rs:1671 close_pane; :84/:182 dispatch_remote_pane_split for remote workspaces) just makes the loop set needs_render (src/server/headless.rs:765-766), and the sibling panes' new (cols,rows) are only applied when the next render's compute_view runs. For remote workspaces the split is fire-and-forget: the local layout does not change until AppEvent::FederationSplitPaneReady arrives (src/remote/federation/client.rs:677-691 -> src/app/api.rs:173 -> src/app/creation.rs:832 handle_federation_split_pane_ready), so the sibling's resize is emitted a full round trip after the user's keystroke — and per hypothesis 2 the frame it produces has no replay recovery.

**Evidence:**
- src/server/headless.rs:3040-3055 (ClientResize -> resize_shared_runtime_to_effective_size)
- src/server/headless.rs:1004-1047 (compute_view + request_full_redraw for all clients, with the wrapping rationale comment)
- src/server/headless.rs:693-720, 765-766 (render only when needs_render; compute_view is where split-driven resizes land)
- src/app/api/panes.rs:84 and :182 (remote split dispatched fire-and-forget)
- src/app/creation.rs:823-860 (FederationSplitPaneReady splices the pane in later)
- src/app/api/panes.rs:1671 (close_pane — no resize/full-redraw call)

### H5 [high] There is no local-pane-only dirty/redraw flag that remote panes miss. Neither local nor remote panes set render_dirty from resize itself; both depend on post-resize bytes. The asymmetry is purely that a local pane's bytes are synchronous/self-generated while a remote pane's are a network round trip.

**Mechanism:** PaneRuntime::resize sets only mark_detection_content_changed (src/pane.rs:2842) — no render_dirty/render_notify. render_dirty is set exclusively from the byte-read closures (src/pane.rs:1926-1935, :2172-2181) shared by both transports: a local PTY actor's reader and the RemoteTerminalSourceHandle reader task (src/remote/federation/pane_source.rs:120-131) both funnel into process_pty_bytes and flip render_dirty. So the redraw mechanism is identical; only the latency and the existence of a repaint differ. The frame that immediately follows the resize is guaranteed to be drawn in both cases (compute_view and render happen in the same render_virtual call), which is precisely why the missing replay recovery for remote panes is visible.

**Evidence:**
- src/pane.rs:2836-2850 (resize sets no render dirty flag)
- src/pane.rs:1920-1936 and 2166-2182 (render_dirty flipped from on_read only)
- src/remote/federation/pane_source.rs:117-131 (remote reader task calls the same on_read)
- src/server/render_stream.rs:294-312 (compute_view then draw in one pass)

**Evidence gaps:**
- Nothing was executed — every claim here is from reading source in /Users/hvnguyen/Projects/herdr-worktrees/remote-workspace-resize-rerender. No build, no test run, no live mount was verified.
- Unverified: whether the remote host in the user's repro has a foreground client of its own. This decides hypothesis 3 (resize ping-pong / reverted remote size) entirely. Needs a live check: mount, resize locally, then read the remote pane's actual size on the remote host.
- Unverified: whether the Resize frame actually reaches the remote runtime at all. src/server/federation_actor.rs:242 drops it unless lease.is_mounted_controller(epoch, connid); I inferred this passes because Input (same gate, src/server/federation_actor.rs:229) demonstrably works for the user, but that is an inference, not a measurement. A tracing log at federation_actor.rs:246 would settle it.
- Unverified: what the remote application actually emits after SIGWINCH in the failing case (full repaint vs nothing). Capturing the inbound federation byte stream right after a local resize would distinguish "no repaint arrives" from "a repaint arrives but at the wrong geometry".
- Unexamined: the effect of the mounted client forcing cell pixel size 0x0 on the remote (src/server/federation_actor.rs:247) on kitty-graphics/sixel panes, and whether the remote's in-band mode-2048 size report (src/pane/terminal.rs:4450) is what the remote app relies on.
- Unexamined: whether ghostty's alternate-screen resize on the local mirror preserves or discards content — that determines how bad the no-replay frame actually looks for agent panes.
