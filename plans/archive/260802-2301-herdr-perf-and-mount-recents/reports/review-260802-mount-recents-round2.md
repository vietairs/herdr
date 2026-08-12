# Re-review (round 2) — mount-remote recents + resync-index purge

- Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/mount-recents-perf`
- Base `origin/master` @ `14ef6b3b`, uncommitted diff: 10 files, +736/-20
- Prior review: `review-260802-mount-recents.md` (1 blocker B1, 3 majors M1–M3, minors m1–m7)
- Date: 2026-08-03

**Verdict: DONE_WITH_CONCERNS — 0 blockers, 0 unresolved majors. Minors only.**

## Verification performed (round 2)

| Check | Result |
|---|---|
| `cargo fmt --check` | pass (exit 0) |
| `cargo test --bin herdr -- --test-threads=4 remote_mount recent` | 38 passed / 0 failed |
| `cargo test --bin herdr -- --test-threads=4 federation resync mount` | 264 passed / 0 failed |
| `cargo test --bin herdr -- --test-threads=4` (full) | 3184 passed / 3 failed — all 3 are the known parallel-contention flakes (`pane_graphics_stream::inactive_owner…`, `plugins::manifest_action_invoke_injects_plugin_paths`, `headless::…AddrInUse`); **all 3 pass under `--test-threads=1`**, re-verified |
| `cargo clippy --all-targets -- -D warnings` | 12 errors, all pre-existing and outside every diff hunk (`cli.rs`, `workspace.rs`, `api/client.rs`, `federation/id.rs`, `federation/protocol/mod.rs`, `pane/osc.rs`, `pane_source.rs`, `workspaces.rs:1833/1858`, `mouse.rs:2412`) |
| `python3 scripts/config_reference_check.py` | 6 missing keys, all pre-existing — the diff's new key is gone from the list |
| **Independent** TOML round-trip sweep vs pinned `toml 0.8.23` (scratch crate) | 15 adversarial targets, all round-trip byte-for-byte, all single-line, unrelated key survives |
| **Independent** geometry sweep: screen heights 4..=50 × recents 0..=7, exact `centered_popup_rect` + `Layout::vertical` constraints + `centered_button_row` reproduced in a scratch ratatui 0.30.0 crate | no overflow, no button-row overlap, no button-row hit-test capture at any height ≥ 8; one cosmetic nit at 6–7 (n1) |

---

## Prior findings — resolution status

### B1 (blocker) — unescaped TOML interpolation → **RESOLVED**

`src/app/config_io.rs:141-150` now serializes through the toml crate:

```rust
let value = toml::Value::from(targets.to_vec()).to_string();
```

Verified independently against the crate's *pinned* version (`toml 0.8.23`, `Cargo.lock:1863` — note the prior review measured `toml 0.9`), not just via the in-repo test. Every adversarial target round-trips and leaves the file parsable with its unrelated keys intact:

| target | serialized | single-line | round-trip | `[ui] accent` survives |
|---|---|---|---|---|
| `ok@host` | `["ok@host"]` | yes | yes | yes |
| `DOMAIN\user@host` | `['DOMAIN\user@host']` | yes | yes | yes |
| `we"ird@host` | `['we"ird@host']` | yes | yes | yes |
| `it's\a@host` | `['''it's\a@host''']` | yes | yes | yes |
| `a'b"c\d@host` | `['''a'b"c\d@host''']` | yes | yes | yes |
| `'\'` | `[''''\'''']` | yes | yes | yes |
| `'''@host`, `#@host`, `]@host`, `üñî@host`, `\t`, `\r`, `\u{7f}`, NUL | escaped/basic strings | yes | yes | yes |

**No new defect from the escaping.** Plain targets still round-trip (`["ok@host"]`), and — the one thing that could have broken the line-based `upsert_section_raw` (`src/config/io.rs:653`) — toml 0.8 never emits a *newline* for these values: it picks literal (`'…'`) or multi-line-literal (`'''…'''`) delimiters but keeps them inline. The upsert's key-prefix match (`recent_remote_mount_targets `) has no colliding key.

In-repo coverage is on-point too: `record_successful_remote_mount_target_escapes_quotes_and_backslashes` (`src/app/mod.rs:3519-3561`) writes both hostile targets, re-parses the file through `crate::config::Config`, and asserts both the exact list and that `ui.accent = "red"` survived.

### M1 (major) — recents painted/hit-tested past the layout rect → **RESOLVED**

`src/ui/remote_mount.rs:78-80` introduces the rect-derived budget, and **both** consumers use it — render at `:188` and `remote_mount_recent_at` at `:106`:

```rust
fn remote_mount_visible_recents_rows(list_rect: Rect, recents_count: usize) -> usize {
    (list_rect.height.saturating_sub(1) as usize).min(remote_mount_recents_rows(recents_count))
}
```

Re-ran the review's own method rather than trusting the branch's 5 sampled heights: a scratch crate reproducing `centered_popup_rect` (`src/ui/widgets.rs:39`), the exact 7-constraint `Layout::vertical` from `remote_mount_rows` (`:52-70`), and `centered_button_row`'s bottom-pinned `y = inner.y + inner.height - 1` (`src/ui/widgets.rs:264`), swept **screen heights 4..=50 × recents 0..=7**. Assertions per cell: last painted recents row inside `inner`; last recents row < button row; last recents row < error row; `remote_mount_recent_at(button.y)` is `None`; `remote_mount_recent_at(error_row.y)` is `None`. **Zero violations at every height ≥ 8.**

The prior failing cases are fixed at the source: at 12 screen rows with 5 recents the list rect shrinks to h2, `visible_rows` collapses to 1, and the mount button stays clickable. No geometry-helper divergence is possible — hit-test and render both call `remote_mount_rows(inner, recents_count)[4]` and then the same clamp helper.

The branch's own `recents_never_overlap_the_buttons_at_a_clamped_popup_height` (`src/ui/remote_mount.rs:329-378`) is a real test: it asserts the button rows do not hit-test as recents *and* that the last visible row is still selectable, so a degenerate "always return None" fix would fail it.

### M2 (major) — lying highlight / first `Down` skips index 0 → **RESOLVED**

`MenuListState` is gone from this path. `RemoteMountState.recents_highlighted: Option<usize>` (`src/app/state.rs:649`); `highlight_next_recent` maps `None → 0` (`:661-664`); `highlight_prev_recent` maps `None → 0` and saturates (`:674-677`). `RemoteMountState` derives `Default` (`:637`) and `open_remote_mount_dialog` (`src/app/remote_mount.rs:45`) installs a fresh `default()`, so re-opening the dialog always resets to "nothing picked".

Render honors it: `let highlighted = remote_mount.recents_highlighted == Some(idx)` (`src/ui/remote_mount.rs:201`) — nothing is accented on a fresh open.

The test that *codified* the old defect (`down_key_navigates_recents_and_fills_the_input`, asserting `"host-a"` at index 1) is deleted and replaced by `a_freshly_opened_dialog_highlights_nothing`, `down_key_selects_the_most_recent_target_first`, and `up_key_clamps_at_the_first_recent`. All pass.

### M3 (major) — full config reload with unrelated live side effects → **RESOLVED**

`save_recent_remote_mount_targets` (`src/app/config_io.rs:141`) now calls only `update_config_file`; the `apply_config_from_disk(false)` is gone. No `agent_panel_scroll` reset, no `clear_selection()`, no `config_reloaded_from_disk` → no second server-side `reload_server_config`.

The "write even when nothing changed" half is fixed too, at `src/app/remote_mount.rs:100-103`:

```rust
if targets == self.state.recent_remote_mount_targets {
    return;
}
```

Covered by `record_successful_remote_mount_target_skips_the_write_when_nothing_changed` (`src/app/mod.rs:3563-3592`), which rewinds the file between records and asserts the key does not reappear.

### Resync-index purge leak → **RESOLVED, no over-removal**

`src/app/api/workspaces.rs:553` adds `purge_remote_resync_pane_index_for_workspaces(&closing_ids)` to the remote-initiated teardown, alongside the two existing purges and — correctly — **before** `self.state.close_selected_workspace()` at `:556`, so the workspaces are still present when the helper walks them.

Over-removal check on `src/app/creation.rs:882-895`: `closing_pane_ids` is built only from workspaces whose `id` is in `closing_ids`, and `closing_ids` comes from `close_indices_for(idx)` for this mount's own federation space key (`:526-530`). `retain` drops an entry only when its *value* (`local_pane_id`) is in that set; `PaneId::alloc()` is globally unique, so a still-live workspace's panes cannot alias. The new regression test seeds an unrelated live mapping and asserts it survives with `len() == 1` — a blanket `.clear()` would fail it.

### Minors

| id | status |
|---|---|
| m1 duplicated cap constant | **resolved** — `src/ui/remote_mount.rs:26` now aliases `crate::app::state::RECENT_REMOTE_MOUNT_TARGETS_CAP` directly |
| m2 doc claims hover support | **resolved** — `src/app/state.rs:642-643` now says "driven by Up/Down keys and clicks" |
| m4 config key missing from reference | **resolved** — `docs/next/website/src/data/config-reference.json:694-700`; `config_reference_check.py` is back to the 6 pre-existing keys |
| m3 recents live in `config.toml`, no clear affordance | carried (design decision) — now at least documented as hand-editable in the config reference |
| m5 test gaps | partly carried — see n4 |
| m6 unrelated `assert_eq!` reflow | carried — `src/app/mod.rs:2351-2355` still in the diff; `cargo fmt --check` passes either way, so it is stable but unrelated noise |
| m7 navigation live while `submitting` | carried — `src/app/remote_mount.rs:114/125` and `src/app/input/mouse.rs:293` still mutate `name_input` mid-submission |

---

## New observations (all minor, none blocking)

### n1 — the "recent" heading can land on the button row at 6–7 terminal rows

`src/ui/remote_mount.rs:184`:

```rust
if remote_mount_recents_rows(recents_count) > 0 && list_rect.height > 0 {
```

The heading is gated on `list_rect.height > 0`, not on `visible_rows > 0`. Swept case: screen height 6 (or 7) with ≥1 recent → `inner` h2, `list_rect` h1 at `y3`, `visible_rows == 0`, but the heading still paints at `y3` — which is exactly `inner.y + inner.height - 1`, the row `centered_button_row` pins the mount/cancel buttons to.

Impact is cosmetic only: the buttons render *after* the recents block (`src/ui/remote_mount.rs:233-254`) and overdraw the centered ~26 columns, so only the left-aligned word `recent` remains visible beside them. The hit-test is unaffected — `remote_mount_recent_at` returns `None` when `visible_rows == 0` (`:107-109`), so the mount button stays clickable. At 6–7 rows every description row of this dialog is already collapsed. Cheap fix: gate the heading on `visible_rows > 0`.

### n2 — the no-op write skip can leave a hand-edited list un-normalized on disk

`src/app/remote_mount.rs:100-103` compares against the *in-memory* list, which `dedup_capped_recent_remote_mount_targets` (`src/app/mod.rs:334`) already normalized at load. So a hand-edited config with 9 entries stays 9 entries on disk while memory holds 5, until some other target forces a write. Harmless — the list is re-normalized on every load and the extra entries are never surfaced — but worth knowing.

### n3 — `recents_highlighted` is not cleared when the user edits the input afterwards

Picking a recent then typing (`selecting_a_recent_still_lets_the_user_edit_before_submitting`, `src/app/remote_mount.rs:614-628`) leaves the highlight on a row whose text no longer equals `name_input`. Arguably useful provenance rather than a lie, and strictly milder than the M2 case it replaced; flagging only for a deliberate call.

### n4 — remaining test gaps from m5

- Still no test that a **failed** or conflict-rejected mount does not record. `handle_federation_mount_failed` (`src/app/api/workspaces.rs:401`) has 8 test call sites, none of which assert `recent_remote_mount_targets.is_empty()`. The headline correctness property is still guaranteed only by statement placement.
- `dedup_capped_recent_remote_mount_targets` (`src/app/mod.rs:334`) — the whole defense against a hand-edited config — still has no direct test.
- No test for the mouse-click selection path, nor for `select_recent_remote_mount_target` with an out-of-range index (the documented stale-click no-op at `src/app/state.rs:745`).

### n5 — Windows dead-code path not compilable here

`#[allow(dead_code)]` on `record_successful_remote_mount_target` (`src/app/remote_mount.rs:92`) makes it a live dead-code root, which also keeps its callee `save_recent_remote_mount_targets` reachable, so no cascade is expected on `x86_64-pc-windows-msvc`. Not verified — the fork's Windows build is independently broken (10 pre-existing cfg-mismatch clippy errors on master), so it is not a gate for this diff.

---

## Unresolved questions

1. n1: gate the heading on `visible_rows > 0`, or accept the 6–7-row cosmetic overlap?
2. m3 carried: keep recents in `[ui] config.toml`, or move to `state_dir()`? The config-reference entry now documents hand-editing as the clear affordance — is that the intended final answer?
3. m6: is the `src/app/mod.rs:2351-2355` `assert_eq!` reflow intentional? It is still unrelated to this change.
4. n4: are the three remaining test gaps (failure-does-not-record, dedup helper, mouse-click path) accepted for this ship, or in scope?
