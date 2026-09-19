# Upstream v0.9.1 merge — conflict scout (read-only)

Date 2026-09-19 · `master` 1947af28 × `v0.9.1` 85447762 · base `b99002ac`

## Method / evidence base
- `git merge-base master v0.9.1` → `b99002ac…` (confirmed, no graft needed).
- `git merge-tree --write-tree --messages master v0.9.1` → merged tree **`f01e5342264aee431e66c6163c0360d3f57092a4`**, **42 conflicted paths**.
- Hunks counted from that tree (`git cat-file blob f01e5342:<path> | grep -c '^<<<<<<<'`) → **60 hunks total**.
- Volume: `git diff --numstat b99002ac..master -- <p>` vs `…b99002ac..v0.9.1 -- <p>`. Attribution: `git log --oneline b99002ac..v0.9.1 -- <p>`.
- **Tooling caveat worth recording:** `git show <rev>:<path> | wc -l` returned wrong counts in this session (7 lines for a 67 KB blob) and produced a false "silent deletion" signal. Every content claim here was re-verified with `git cat-file blob`. Do not trust `git show`-piped counts when validating this merge.

## Headline findings
1. **No delete/modify conflicts.** `comm -12` of the 22 upstream deletions against every fork-touched path is **empty**.
2. **No `src/ui/*.rs` deletions, no `src/server/headless/*` deletions.** The 22 deletions are 15 `vendor/libghostty-vt/**`, 1 vendor patch, 6 `workers/plugin-marketplace/**`. Headless churn is content-only.
3. **All 25 fork-only `src/` files survive** — `src/remote/federation/*` (14), `src/server/federation_*.rs` (4), `src/client/shell/remote_mount.rs`, `src/client/shell/tests/remote_mount.rs`, `src/app/remote_clipboard_stage.rs`, `src/remote/host_unix.rs`, `src/terminal/source.rs`, `src/workspace/naming.rs`, `src/image_path.rs`.
4. **Federation symbol census flat across the auto-merge.** master vs merged tree f01e5342: `federation` 1724/1724, `FederationMessage` 489/489, `mount_remote` 60/60, `WorkspaceCloseRemote` 7/7, `TabCloseRemote` 7/7, `WorkspaceMountRemote` 33/33, `remote_mount` 156/156, `recent_remote_mount_targets` 39/39, `NameSource` 161/161, `mirrored_label` 25/25, `FEDERATION_PROTOCOL_VERSION` 37/37 — **delta 0 everywhere**.
5. **One conflict-free silent deletion, and it is upstream's refactor.** `src/client/shell/state.rs` auto-merges; `replace_on_type` count goes 2 → **0**. Upstream #3698 replaced `input: String` + `replace_on_type: bool` with `input: TextEditor`. The 30 dependent sites in `context_menu.rs`, `overlay_input.rs`, `mouse.rs:1641`, `worktrees.rs`, `tests/input.rs`, `tests/popup_focus_projection.rs` break at compile time — and `mouse.rs`, `worktrees.rs`, both test files **do not conflict**.

## Protocol constants
| Constant | base | fork | upstream | verdict |
|---|---|---|---|---|
| `src/protocol/wire.rs:20 PROTOCOL_VERSION` | 22 | **24** | 22 | **no conflict**; wire.rs auto-merges. 24 > 22 → **no further bump required**. Keep 24; do not take upstream's 22. |
| `FEDERATION_PROTOCOL_VERSION` | absent | **7** | absent | fork-only, no conflict. |

Check during the merge (not a conflict): upstream added ~1728 lines of new wire surface (`src/protocol/surface_delta.rs`, `surface_delta/decode.rs`, `surface_reuse.rs`, +88 `wire.rs`, +73 `endpoint.rs`) via 18061191 / d8b36691 / 9ad65d90 **without moving PROTOCOL_VERSION off 22**. Also new: `"endpoint_generation": 1` in upstream `distribution/preview.json`.

## Conflict table (T = take-both · M = mechanical · S = semantic)
| File | Hunks | Why | Diff | Fork feature at risk |
|---|---|---|---|---|
| `src/app/api.rs` | 1 | Fork added `Method::WorkspaceCloseRemote` beside `WorkspaceClose(params)`; upstream (f0cb0e06/1682cab3) changed `WorkspaceClose` to take `target`. | **S** | **Remote workspace close** — take-upstream deletes the `WorkspaceCloseRemote` arm. |
| `src/app/window_title.rs` | 1 | Fork falls back to `terminal.mirrored_label`; upstream ad415672 rewrote the block for per-client-view titles reading only `manual_label`. | **S** | **Mirrored remote labels** — take-upstream silently blanks federation window titles. |
| `src/client/shell/agent_sidebar.rs` | 1 | Fork inlined the `NameSource` ladder; upstream 3b478e69 extracted the body into `agent_row(...)` + machine scoping. | **S** | **Name ladder + `NameSource::Mirrored`** — upstream's side is ONE line replacing ~55 fork lines. Most deceptive resolution in the set. |
| `src/client/shell/context_menu.rs` | 1 | Fork sets `replace_on_type` from `NameSource::Override` + `ClientRenameTarget::Pane{original_name}`; upstream b0f21ed2 → `TextEditor::new(text, replace)`. | **S** | Pane rename ladder. |
| `src/client/shell/overlay_input.rs` | 1 | Same `replace_on_type`→`TextEditor` swap on pane-rename prefill. | **S** | Same. |
| `src/client/shell/overlays.rs` | 1 | Pure adjacency: fork's entire `render_remote_mount_overlay` + `REMOTE_MOUNT_*` consts vs upstream 120c6820's new `navigator_following_siblings`. | **T** | Dropping fork side deletes the **whole mount-dialog renderer**. |
| `src/client/tests/mod.rs` | 1 | Fork's unbridged-empty-paste test vs upstream Windows VT-input tests at the same `#[cfg(unix)]` anchor. | **T** | Remote clipboard-image paste guard. |
| `src/config/write.rs` | 1 | Fork's two `RecentRemoteMountTargets` TOML-escaping tests vs upstream eba7758c BOM test. | **T** | Mount recents persistence tests. |
| `src/ghostty/mod.rs` | 1 | Fork's `unicode_*_width` FFI vs upstream f8e59221 `enum LinkTarget`. | **T** | none |
| `src/pane.rs` | 1 | Fork `test_set_reported_cwd` vs upstream `test_contend_during_dirty_collection`. | **T** | none |
| `src/pane/terminal.rs` | **6** | Upstream 4c3b613c renamed `OrderedPtyResponseEvent`→`OrderedColorOrC1Event`; 6e7d415b/98a8c6ce replaced the manual dirty-row walk with `rows.next_dirty()`. | **S** | Fork's combined OSC-4 multi-index palette reply; fork's walk carries hyperlink/selection `fallback!` guards. |
| `src/remote/attach.rs` | **5** | Fork added `session_name`/`with_session_name()`/`invocation()` + `#[cfg(unix)] UnixStream`; upstream 59167658 added `RemoteExecutable::WindowsPath` + PowerShell launchers, c77af189 added `bridge_idle_timeout`. | **S** | **Windows federation + named-session mount.** |
| `src/server/client_commands.rs` | 1 | Fork's four `CLIENT_SHELL_METHODS` assertions vs upstream f8e59221's `PaneLinkResolve`. | **T** | **The client-shell method gate** — already silently killed mount + close-on-host + balance-splits once (fix a64d548d). |
| `src/server/headless/tests/mod.rs` | 1 | Both appended at the same file tail. | **T** | Mount terminal-size ownership guard. |
| `tests/cli/sessions.rs` | 2 | Fork uses `CURRENT_PROTOCOL`/`CURRENT_ENDPOINT_PROTOCOL_GENERATION`; upstream hardcodes `22`/`1`, adds `remote_host_bridge: true`. | **M** | Keep fork constants, add upstream's assertion. |
| `tests/support/mod.rs` | 1 | Fork made the frame reader non-blocking; upstream 1f1b20cb set a 200 ms read timeout with a read-back guard — same bug, two fixes. | **S** | macOS live-handoff/shutdown frame reads. Decide empirically. |
| `.github/workflows/ci.yml` | 2 | Fork pins Zig 0.15 via Homebrew; upstream moved to Zig 0.16, split lint/test per OS. | **S** | **Fork CI green.** Coupled to the vendored-base decision. |
| `.github/workflows/release.yml` | **6** | Upstream added `validate-release-source` + `if: github.repository == 'herdrdev/herdr'` to six jobs; fork carries `if: ${{ !contains(github.ref_name, '-hvn.') }}` in two. | **S** | **Fork `-hvn` release gating** — upstream's guard is always false on the fork → pipeline silently does nothing, green CI, zero assets. |
| `.github/workflows/preview.yml` | 3 | Upstream added repository + tag-prefix guards to three jobs. | **S** | **Fork preview channel.** |
| `AGENTS.md` (`CLAUDE.md` is a symlink) | 2 | Both rewrote release/preview prose. | **S** | Symlink trap — `git diff` on `CLAUDE.md` is always empty. |
| `distribution/preview.json` | 1 | Fork snapshot vs upstream snapshot + new `endpoint_generation`. | **S** | Take **ours**, add `endpoint_generation` by hand. |
| `justfile` | 1 | Fork's `test_client_shell_method_advertisement` vs upstream's new release/windows-cross tests. | **M** | Union. |
| `scripts/preview.py` | 2 | Fork guards with `git_commit_exists`; upstream with `git_is_ancestor`, drops `base_version` from `build_notes`. | **S** | Both guards wanted; watch the `build_notes` arity change. |
| `scripts/test_preview.py` | 1 | Adjacent tests. | **M** | Take both; reconcile the shared test name. |
| `vendor/libghostty-vt.patches.md` | 1 | Fork keeps `0001 default grapheme cluster mode` active; upstream **deleted** it and added `0004`/`0005` on base `44f2a44d` (fork on `c5a21edf`). | **S** | `just check` maintenance tests. |
| `docs/next/CHANGELOG.md` | 1 | Both appended under the same `## Unreleased` headings. | **T** | **Take-both, never pick-one.** |
| `docs/next/.../connecting-machines.mdx` | 1 | Fork platform support vs upstream Windows-servers rewrite. | **S** | Neither sentence is currently true post PR #24/#25. |
| `docs/next/.../{ja,zh-cn}/cli-reference.mdx` | 1 ea | Translation-parity trim vs upstream additions. | **T** | `just release-docs-check` heading gate. |
| `docs/next/.../{ja,zh-cn}/session-state.mdx` | 1 ea | Purely additive upstream paragraphs. | **T** | none |
| `docs/preview/website/src/content/docs/**` (11 files) | 1–4 ea | Fork's preview snapshot is fork-owned. | **S** (policy) | Snapshotting against an upstream pin **deleted the federation docs** once. Resolve as **ours**. |

## Prose on the SEMANTIC ones, ranked by cost of getting it wrong
**1. `release.yml` + `preview.yml` (9 hunks).** Upstream's guard `github.repository == 'herdrdev/herdr'` is false on `vietairs/herdr`, so a clean take-upstream yields a release pipeline that does nothing while reporting green. Adopt upstream's `validate-release-source` job and tag-shape predicates, replace/delete every repository clause, re-apply the two `-hvn` exclusions. It is the only conflict whose failure is invisible until a release is attempted.

**2. `agent_sidebar.rs` + `window_title.rs` + `context_menu.rs` + `overlay_input.rs` (4 hunks + ~30 non-conflicting compile-break sites).** The client shell's naming/text-input plumbing was refactored, and the fork's four-namespace work lives exactly there. `agent_sidebar`'s upstream side is a single `agent_row(...)` line standing in for ~55 fork lines — taking it makes the conflict vanish and the file compile while deleting the ladder. Move the ladder INTO upstream's extracted `agent_row`. The `TextEditor` migration is mechanical but lands mostly in non-conflicting files — expect a compile-error sweep, not a marker sweep.

**3. `src/remote/attach.rs` (5 hunks).** Two feature streams on one struct. Hunks 3/4 are literally `session_name: None` vs `bridge_idle_timeout: false` — both belong. Re-run `just windows-lint` (msvc) after.

**4. `src/app/api.rs`.** Three lines, maximum blast radius. Keep both match arms, adopt upstream's `target` shape for the local close.

**5. `src/pane/terminal.rs` (6 hunks).** Re-add `PaletteQueryBatch` as a third variant of upstream's enum; verify `rows.next_dirty()` preserves the fork's `fallback!` exits.

**6. `tests/support/mod.rs`.** Resolve by running the handoff/shutdown tests on this mac both ways, not by reading.

**7. Vendor bump.** Real question is whether base `44f2a44d` still builds under `ZIG=~/.local/zig-0.15.2/zig` — that also decides `ci.yml`.

**8. `docs/preview/**` + `distribution/preview.json`.** Policy: resolve `--ours` wholesale, regenerate at the fork's next preview.

## Effort estimate
| Bucket | Files | Hunks | Est. |
|---|---|---|---|
| Take-both / trivial src | 7 | 7 | 45 min |
| Docs next + translations | 6 | 6 | 30 min |
| `docs/preview/**` + `distribution/preview.json` as `--ours` | 12 | 14 | 20 min |
| Scripts + justfile + vendor index | 4 | 5 | 1 h |
| Workflows + `AGENTS.md` | 4 | 13 | 1.5 h |
| SEMANTIC: naming ladder / TextEditor (incl. ~30 non-conflicting break sites) | 4 (+4) | 4 | 2–3 h |
| SEMANTIC: `remote/attach.rs`, `app/api.rs`, `pane/terminal.rs`, `tests/support` | 4 | 13 | 2–3 h |
| Verify: `just check` (mac), `just windows-lint` (msvc), federation live mount smoke on 2 hosts | — | — | 2 h |

**Total ≈ 10–12 h focused**, ~6 h of it the src semantic core. Small in hunks (60) versus v0.8.0's 40-conflict / the pre-graft 354-conflict merges, but the danger is concentrated: 4 of the 14 src conflicts are places where upstream's side is shorter, compiles, and deletes fork behavior.

## Pre-merge checklist
- [ ] No `git replace` graft — merge-base verified `b99002ac`.
- [ ] Keep `PROTOCOL_VERSION = 24`; do not take upstream's 22.
- [ ] `docs/preview/**` and `distribution/preview.json` → **ours**; add `endpoint_generation` by hand.
- [ ] Rewrite every upstream `github.repository == 'herdrdev/herdr'` guard; keep the two `-hvn` exclusions.
- [ ] `docs/next/CHANGELOG.md`: concatenate both sides under matching headings.
- [ ] After resolving, re-run the 11-symbol census and compare to master — any drop means a resolution ate fork behavior.
- [ ] `git grep -c replace_on_type` must end at **0**.
- [ ] `just windows-lint` (msvc) — `src/remote/attach.rs` is the Windows-federation collision point.

## Unresolved questions
1. Does vendored base `44f2a44d` still build with Zig 0.15.2, or does upstream's `ci.yml` force Zig 0.16?
2. `ClientRenameTarget::Pane { original_name }` is fork-only with no slot in upstream's `TextEditor` shape — still needed?
3. `tests/support/mod.rs`: is the fork's non-blocking reader still required? Needs an empirical run.
4. `connecting-machines.mdx`: neither side's Windows sentence matches fork reality post PR #24/#25 — fresh prose owed.
