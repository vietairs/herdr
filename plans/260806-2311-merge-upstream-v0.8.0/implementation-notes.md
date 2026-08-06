# Implementation notes — merge upstream v0.8.0

## Decisions / deviations (append-only)

### 1. `git replace` graft to correct the merge base
- **What:** grafted upstream's rewritten `v0.7.5` (`ef4c23f5`) onto the fork-merged `848f11f1`.
- **Why:** upstream force-pushed after tagging; without the graft the merge base falls back to
  2026-07-13 and replays v0.7.5 as new work (1069 files). With it: 130 commits, 40 conflicts.
- **Evidence:** both commits have tree `2b2745aad19a5b7b1f65fc5789bfac4331c5570a`; `848f11f1` is
  not an ancestor of `upstream/master`. Conflict count 40 vs an expected ~350.
- **Reversibility:** trivial — `git replace -d ef4c23f5...`. Replace refs are local and unpushed;
  the merge commit records the real upstream tip as second parent, so the fix is self-anchoring.

### 2. `src/app/input/clipboard.rs` — added `origin: None` at 3 sites
- **What:** upstream's new `dispatch_pending_clipboard_write()` helper and two test sites
  constructed/matched `AppEvent::ClipboardWrite` without the fork's `origin` field.
- **Why:** the file auto-merged with NO conflict (upstream added the helper; the fork never
  touched that file), but `events.rs` kept the fork's `ClipboardWrite { content, origin }`. This
  is a semantic break git cannot flag. Local selection copy has no remote pane behind it, so
  `origin: None` is correct — it matches the inline dispatch the fork already uses elsewhere.
- **Evidence:** `events.rs:149-152` requires `origin: Option<String>`; `clipboard.rs:22` omitted it.
  Found by group B while resolving `input/mod.rs`, which had the same choice as an actual conflict.
- **Reversibility:** trivial, 3 lines.

### 3. `FEDERATION_PROTOCOL_VERSION` bumped 4 -> 5
- **What:** bumped the federation protocol version and documented why.
- **Why:** upstream inserted `EventKind::WorkspaceReordered` mid-enum. `EventKind` is carried in
  `EventMessage.kind` over the federation channel, so a v4 peer receiving `"workspace_reordered"`
  fails to deserialize the frame. That is the fork's own documented bump criterion ("a new
  top-level variant a peer cannot decode"), matching the `Fault` 1->2 and `ClosePane` 3->4
  precedents. v4 shipped in tags `v0.7.5-hvn.2`..`v0.7.5-hvn.6`, so it cannot be amended in place.
- **Evidence:** `src/api/schema/events.rs` variant order HEAD vs merged; `codec.rs:55-59` confirms
  frames are **serde_json**, not bincode. That distinction matters: the advisory pass predicted a
  bincode discriminant shift causing SILENT misinterpretation. Verified false — serde_json encodes
  the variant by name, so the real failure is a clean decode error. Bumping is still correct, but
  for the decode-error reason, not the corruption reason.
- **Reversibility:** one constant; but reverting reintroduces mid-session frame errors on
  mixed-version mounts.

### 4. Vendored libghostty-vt bumped by upstream (`0f7cd84b8` -> `c5a21edfc`)
- **What:** upstream bumped the vendored source and added its own patch also numbered `0001`,
  colliding with the fork's `0001`. Index entries unioned; fork's resizeCols patch renumbered to
  `0003` in the index heading only, on-disk filename left alone.
- **Why:** CLAUDE.md requires every on-disk patch to be indexed and to reverse-apply cleanly.
- **Evidence:** verified independently of the agent's grep — all three patch files pass
  `git apply --reverse --check` against the merged vendored tree, i.e. all three are genuinely
  applied. No patch was silently dropped by the vendor bump.
- **Reversibility:** index-only edit; patch files untouched.

### 5. Vendored libghostty-vt upgraded 0f7cd84b8 -> c5a21edfc (USER DECISION)
- **What:** took upstream's `vendor/libghostty-vt`, `vendor.json`, and the checked-in
  `src/ghostty/bindings.rs` at `c5a21edfc`; local patches rebased/retired against the new tree.
- **Why:** the fork was running an OLDER vendored libghostty than upstream v0.7.5 — a divergence
  that predates this merge (introduced during the 354-conflict v0.7.5 merge, which resolved the
  vendored tree to the fork's side). Upstream v0.8.0's `ghostty/mod.rs` is built on the newer
  clipboard-write FFI, producing 24 "not found in `ffi`" errors.
- **Evidence:** `848f11f1` and `ef4c23f5` (identical trees `2b2745aa`) BOTH vendor `c5a21edfc` and
  both carry 30 `GhosttyClipboardWrite` symbols in `bindings.rs`; fork master has `0f7cd84b8` and 0.
  So upstream never bumped for v0.8.0 — the fork fell behind. (Group D's report claimed upstream
  bumped the vendor; that was backwards and is corrected here.)
- **Reversibility:** moderate. Reverting means `git checkout master -- vendor/ src/ghostty/bindings.rs`
  and reverting `ghostty/mod.rs` clipboard sections; but it re-opens the divergence and forfeits
  upstream v0.8.0's clipboard-write capability.
- **Decision owner:** user, asked explicitly (overrode `--auto`) because the option set changed
  both scope and risk — the alternative was a documented permanent divergence.

### 6. Latent non-marker merge bugs found by compile verification (group A)
- **What:** three defects git never flagged, because they sat OUTSIDE conflict markers:
  (a) `src/ghostty/mod.rs` `struct Terminal`'s field list stayed at the fork's version while
  `Terminal::new` inside the markers had already moved to upstream's fields;
  (b) a duplicate `take_pwd_changes` (fork's polling version left alongside upstream's new
  callback version); (c) `src/pane/osc.rs` kept a call to `parse_osc52_clipboard_write`, which
  upstream deleted in a NON-conflicting hunk of the same file.
- **Why it matters:** this is the merge's dominant risk class — a clean `git merge` says nothing
  about semantic consistency. All three were caught only by `cargo check`, not by review of hunks.
- **Resolution:** `Osc52Forwarder` was removed (superseded by libghostty-vt's native clipboard
  callback, which the merged `terminal.rs` already calls); the genuinely fork-original
  `CwdOscTracker` was ported onto upstream's new `OscStreamCollector`.
- **Reversibility:** low-risk to revisit, but needs review — flagged for the code-review stage.

### 7. Live conflict markers survived stage 3 in a generated artifact
- **What:** `docs/next/api/herdr-api.schema.json` still contained literal `<<<<<<<`/`=======`/
  `>>>>>>>` markers after the 5-group conflict fan-out reported "0 markers". Caught only by the
  schema-artifact test failing, not by review.
- **Why it matters:** the stage-3 marker sweep was run with `git grep`, which reads the INDEX. The
  merge ran `--no-commit` and nothing was ever staged, so the index still holds conflict stages and
  the sweep did not see the working tree. Working-tree `grep -rIn` now confirms 0 markers.
- **Resolution:** regenerated via its documented path (`HERDR_UPDATE_API_SCHEMA=1`), not hand-edited,
  so the artifact matches the merged Rust types rather than a human guess at the union.
- **Reversibility:** trivial — regenerate again. But the LESSON is not reversible: verify merge
  cleanliness against the working tree, never `git grep`, until the merge is staged.

### 8. Two parallel-only test failures are pre-existing, not merge regressions
- **What:** at `--test-threads=4`, `pane_graphics_stream::...inactive_owner_cancels_idle_stream...`
  and `plugins::...manifest_action_invoke_injects_plugin_paths` fail; serially 3364/3364 pass.
- **Evidence:** `git diff master -- src/api/server/pane_graphics_stream.rs` is EMPTY (byte-identical
  to fork master); the `plugins/mod.rs` diff is four `ogulcancelik` -> `herdrdev` URL-fixture string
  renames, none inside the failing test. Both tests predate the merge on fork master and upstream.
  Cause is test isolation (timeout under load; config-dir/env contamination), not this merge.
- **Reversibility:** n/a — nothing was changed for them. Tracked as separate test-isolation debt.

### 9. Upstream's kitty-graphics generation fast path was silently dropped (BLOCKING, fixing)
- **What:** `kitty_graphics()`, `kitty_graphics_generation()`, `kitty_graphics_u64()` and both
  early-return branches in `kitty_image_placements_with_data_filter` vanished during conflict
  resolution of `src/ghostty/mod.rs`.
- **Why it matters:** `kitty_empty_generation` survives as WRITE-ONLY dead state, so every render
  pass re-walks the full placement iterator even for panes that never transmit an image. v0.8.0's
  headline is CPU reduction; this silently reverts part of it. The compiler cannot catch it — the
  field is still "used" by its initializer, so no dead-code warning fires.
- **Evidence:** orchestrator-verified, not taken from the report: all three helpers are `1` in
  `MERGE_HEAD:src/ghostty/mod.rs` and `0` in the worktree; `kitty_empty_generation` drops 7 -> 2
  occurrences, remaining only at `:792` (declaration) and `:818` (`Cell::new(None)`).
- **Reversibility:** trivial and safe — the fork never edited this function, so upstream's version
  restores verbatim with no conflict to re-resolve.

### 10. `send_bytes_after` is a no-op for remote panes on a FALSE justification (BLOCKING, fixing)
- **What:** `src/pane.rs` `PaneRuntimeIo::Remote(_) => {}`, commented as unavoidable because
  `RemoteTerminalSourceHandle` "is not cheaply cloneable (owns reader/forward `JoinHandle`s)".
- **Why it matters:** the claim is wrong — only the sender needs cloning, and
  `pane_source.rs:83` is `input_tx: mpsc::Sender<Bytes>`, cheaply cloneable and `'static`. Real
  impact is worse than the "convenience feature" the comment claims: `src/app/api/agents.rs:102-109`
  sends the prompt text over the wire, no-ops the delayed Enter, then returns
  `encode_success(AgentPrompted)`. Any API/CLI/plugin caller prompting an agent in a FEDERATED pane
  gets success while the prompt sits unsubmitted. API contract violation, fork-feature-specific.
- **Evidence:** orchestrator-verified by reading all three sites directly.
- **Reversibility:** trivial — mirror the `Actor` arm's spawn+sleep+send using a cloned `input_tx`.
- **Lesson:** a plausible-sounding comment justifying a dropped behavior is not evidence. This one
  was written during conflict resolution and would have shipped unchallenged.

### 11. Fork's `unicode_width`-crate fallback retired — vendor upgrade obsoleted it
- **What:** replaced the fork's `unicode_codepoint_width`/`unicode_grapheme_width` (implemented over
  the `unicode-width` crate) with upstream's FFI versions, and ported the 7th dropped test
  `unicode_width_helpers_match_terminal_layout_rules`.
- **Why:** the fork's doc comment justified the fallback as "declared in the vendored C header but
  not exported by the currently-linked static library on this vendored source commit (undefined
  symbol at link time)". That was TRUE for the old vendor and is now FALSE — this merge upgraded
  the vendored tree. Third instance of the same pattern (cf. notes 4 and 5: two patches also
  retired as redundant). The fix agent declined this port on the stale comment's authority; the
  comment was the thing to check, not to trust.
- **Evidence:** `git show master:vendor/libghostty-vt/src/lib_vt.zig` has NO
  `ghostty_unicode_codepoint_width`; the upgraded tree exports it at `lib_vt.zig:199-200`, in the
  same unconditional `@export` block as symbols the fork already links (`osc_next`,
  `paste_encode`, ...). Empirically confirmed: builds and links, 32/32 ghostty tests pass.
- **Behavioral gain, not just test coverage:** the fork's `unicode_grapheme_width` always consumed
  exactly ONE codepoint, so ZWJ emoji families, regional-indicator pairs and skin-tone modifiers
  all mis-measured. The ported test's family case asserts `consumed=5`, which the fork version
  could never satisfy. Affects IME preedit width prediction.
- **Reversibility:** trivial (two functions). `unicode-width` crate stays in `Cargo.toml` — still
  used by `src/ui/text.rs`, `src/pane/terminal.rs`, `src/app/input/mod.rs`,
  `src/protocol/render_ansi.rs`.

### 12. Two review-sourced cleanup premises were WRONG; agent correctly refused
- **What:** the review listed `src/workspace.rs:4` `Ordering` as unused and `config_io.rs:103`
  `save_pane_history_persistence` as orphaned. Both were passed to the fix agent by the
  orchestrator; the agent refused both after checking.
- **Why they were wrong:** `Ordering::Relaxed` IS used in `workspace.rs` (lines 118, 179, 184-185)
  inside `#[cfg(not(test))]` blocks guarding the process-global `NEXT_WORKSPACE_ID`; it only looks
  unused under `cargo check --all-targets`, which compiles the test cfg. Removing it breaks the
  real build. `save_pane_history_persistence` has a live caller at `src/app/mod.rs:3855`.
- **Lesson:** `cargo check --all-targets` "unused" warnings are not authoritative for symbols
  gated on `#[cfg(not(test))]`. Verify the non-test build before deleting.
- **Reversibility:** n/a — nothing was changed.

### 13. Fork's `CLAUDE.md` kept over upstream's rewrite (USER DECISION)
- **What:** restored fork master's `CLAUDE.md` instead of taking upstream v0.8.0's rewritten version.
- **Why:** upstream's rewrite re-keys maintainer workflow to `.github/MAINTAINERS` with a PR-based
  flow ("never merge a PR; Can performs the final merge") and changes the `PROTOCOL_VERSION` bump
  rule from "compare against the latest released tag" to "compare against protocols published in
  BOTH stable and preview channels; bump only once before publication". Those describe herdrdev's
  process and release channels, not this fork's.
- **Evidence:** `git diff master -- CLAUDE.md` is now empty (byte-identical to fork master).
- **Reversibility:** trivial (`git checkout MERGE_HEAD -- CLAUDE.md`), but note this file will now
  conflict on EVERY future upstream merge. That recurring cost was the accepted trade.
- **Decision owner:** user, asked explicitly — governance/process is not an orchestrator call.

## Open items for the ship gate

- Windows build is NOT covered by a mac `cargo build`. The fork already carries a known Windows
  clippy baseline; platform cfg-gating regressions from this merge would not surface locally.
- Federation needs a live cross-build mount test: merged<->merged must work; merged<->v4 peer must
  now cleanly REJECT at handshake rather than error mid-session.
- Both local and remote servers must be rebuilt/restarted before any federation test (stale server
  images have previously produced misleading MountSnapshot failures).
