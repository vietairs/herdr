# Adversarial review — mount-remote recents (change 1)

- Worktree: `/Users/hvnguyen/Projects/herdr/.claude/worktrees/mount-recents-perf`
- Branch: `feat/mount-recents-and-perf` (base `origin/master` @ `14ef6b3b`), uncommitted diff
- Scope reviewed: 9 modified files, +483/-21. No untracked new source files (`git status --short` shows M only).
- Date: 2026-08-02

## Verification performed

| Check | Result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo check --tests` (ZIG 0.15.2 + xcrun shim, shared target dir) | pass, no new warnings |
| `cargo test --bin herdr -- --test-threads=1 remote_mount recent` | 34 passed / 0 failed |
| `cargo clippy --all-targets -- -D warnings` | 12 errors, **all pre-existing** (`cli.rs`, `workspace.rs`, `api/client.rs`, `pane/osc.rs`, `federation/id.rs`, `pane_source.rs`, plus test-module lines in `api/workspaces.rs:1827/1852` and `input/mouse.rs:2412` — none inside the diff hunks at `workspaces.rs:285` / `mouse.rs:289`) |
| `python3 scripts/config_reference_check.py` | fail — 7 keys missing, 6 pre-existing + 1 new from this diff |
| Empirical TOML round-trip of target strings (scratch crate, `toml 0.9`) | corruption confirmed, see B1 |
| Empirical ratatui 0.30 layout at clamped popup heights (scratch crate) | overflow confirmed, see M1 |

## What is correct

- Success-only recording is right: `record_successful_remote_mount_target` sits *after* the `materialize_federation_mount` `Err` early-return and after the `begin_federation_mount` conflict early-return, so neither failure path records (`src/app/api/workspaces.rs:285`). `handle_federation_mount_failed` never records. Verified by reading both early-return branches.
- Per-target recording is right: `FederationMountReady` is emitted once per target by the spawn loop (`src/app/api/workspaces.rs:106-152`), and it is the only emitter in the tree, so a 2-target submission records two entries.
- Ordering/dedup/cap logic (`record_recent_remote_mount_target`, `src/app/state.rs:706`) is pure, list-only, free of I/O, and directly covered by three `AppState`-level tests.
- No protocol/wire/API-schema change leaked in: `PROTOCOL_VERSION`, `src/api/schema/`, and the socket messages are untouched. Recents never crosses the JSON API.
- `render()` stays pure — the new recents block only reads `&AppState`; the geometry helper `remote_mount_rows` is shared by render and hit-test so they cannot drift *at unclamped sizes*.
- Corrupt/hand-edited config cannot panic: no `unwrap`/`expect` in the new production paths; a wrong-typed value fails deserialization into a diagnostic (`Config::load` → defaults; `load_live_config` → "keeping current config"), never a panic. `dedup_capped_recent_remote_mount_targets` defends against a hand-written duplicate/over-cap list on both the `App::new` and reload paths.
- Existing dialog flows are semantically untouched: `Esc`, `Backspace`, `Char`, `Enter`/submit, the re-entrancy guard, the error-over-`submitting` render precedence, and the abandoned-mount correlation are all byte-identical apart from row-index renumbering `rows[4] → rows[5]`.
- Both modal drain sites stay consistent: the mouse handler recomputes `inner` with the same `recents_count` the renderer uses, and the single `AppEvent::FederationMountReady` drain site (`src/app/api.rs:158`) is unchanged.

---

## Findings

### B1 — blocker — unescaped TOML interpolation of an arbitrary target corrupts the entire config

`src/app/config_io.rs:130-146` (value built at `:135`)

```rust
.map(|target| format!("\"{target}\""))
```

The doc comment asserts *"targets are `user@host` strings that never contain a `\"`"*. Nothing enforces that. The only validation on a target is `validate_remote_target` (`src/remote/unix.rs:285`), which rejects **only** empty strings and a leading `-`. The string originates from free text in the dialog (`parse_mount_targets` splits on whitespace and keeps everything else, including pasted bytes) or from any JSON-API caller of `workspace.mount_remote`. The cited precedent (`save_theme`) writes a name chosen from an enumerated UI list, so it does not transfer.

Empirically confirmed against `toml 0.9`:

```
target "DOMAIN\\user@host" -> PARSE ERROR: too few unicode value digits (line 2, col 41)
target "we\"ird@host"      -> PARSE ERROR: missing comma between array elements
target "ok@host"           -> parses OK
```

Failure scenario: a user mounts an ssh destination whose user component contains a backslash (`ssh DOMAIN\user@host`, a legal OpenSSH form) or whose `~/.ssh/config` `Host` alias contains a `"` or `\`. The mount succeeds; herdr writes `recent_remote_mount_targets = ["DOMAIN\user@host"]` into `~/.config/herdr/config.toml`. From that moment:

1. the immediate `apply_config_from_disk` fails to parse and raises a config diagnostic;
2. **on the next herdr start, `Config::load` hits the top-level parse error and returns `Config::default()` for the whole file** (`src/config/io.rs:145-152`) — theme, keybinds, sidebar, `[remote]`, `[experimental]`, everything silently reverts to defaults;
3. it does not self-heal: `update_config_file` is line-based and never parses, so each later mount just rewrites the same still-broken line. Recovery requires the user to find and hand-edit the file.

Silent, total loss of an unrelated user-owned file, triggered by legal input, from a one-line formatting shortcut. Escape the value (or refuse to persist a target that would need escaping) before merge.

---

### M1 — major — recents rows are painted and hit-tested past the layout rect when the popup is height-clamped

`src/ui/remote_mount.rs:183-197` (render) and `src/ui/remote_mount.rs:86-101` (`remote_mount_recent_at`)

Both loops derive their row count from `remote_mount_recents_rows(recents_count)` — i.e. from state — and index off `list_rect.y + 1 + idx`, never from `list_rect.height`. `centered_popup_rect` (`src/ui/widgets.rs:41`) clamps `popup_h` to `area.height - 2`, so on a short terminal `inner` is smaller than the fixed `Length` constraints require and ratatui shrinks `rows[4]` while the two loops keep assuming the full-size list.

Measured with the branch's exact geometry (5 recents, ratatui 0.30):

| terminal rows | popup | inner h | `list_rect` | error row | button row | rows actually drawn |
|---|---|---|---|---|---|---|
| 12 | y1 h10 | 8 | y6 h**2** | y8 | y9 | y7..y**11** |
| 14 | y1 h12 | 10 | y6 h**4** | y10 | y11 | y7..y**11** |
| 16+ | — | 12+ | y6 h6 | y12 | y13 | y7..y11 (fits) |

Failure scenario, terminal 12 rows tall with 5 stored recents: recent #3 is painted directly on top of the mount/cancel button row (y9), recent #4 paints over the popup's bottom border (y10), recent #5 paints *outside the popup* onto the dimmed background (y11). Worse, `remote_mount_recent_at` is consulted **before** `remote_mount_button_rects` in `src/app/input/mouse.rs:293-300` and returns `Some(2)` for a click at y9 — so clicking "mount" selects a recent instead of submitting, and **the dialog's primary action becomes unreachable by mouse**. Same at 14 rows (click on the y11 button row → `Some(4)`).

herdr has no minimum-terminal-size gate for rendering (`MIN_ROWS = 24` in `src/server/headless.rs:227` is only the no-client fallback size), and this is exactly the steady state after a week of use (5 recents). Clamp both loops to `list_rect.height.saturating_sub(1)`.

---

### M2 — major — the highlighted recent lies, and the first `Down` skips the most-recent entry

`src/app/state.rs:654` (`MenuListState::new(0)`), `src/app/remote_mount.rs:119-131`, render at `src/ui/remote_mount.rs:191`

`MenuListState` has no "nothing highlighted" state (`highlighted: usize`, `src/app/state.rs:1184`). Two consequences:

1. The moment the dialog opens, `recents[0]` renders with the accent background — it *looks* selected — while `name_input` is empty. Pressing Enter on that visibly-selected row produces the inline error "enter at least one target".
2. `move_next` computes `min(0 + 1, len - 1)`, so the first `Down` lands on **index 1**, silently skipping the most-recent target. The branch's own test codifies the defect as expected behavior: `down_key_navigates_recents_and_fills_the_input` (`src/app/remote_mount.rs:541-559`) seeds `["host-b", "host-a"]` and asserts the first `Down` yields `"host-a"` at index 1. Reaching the top entry requires `Down` then `Up`.

`Up` is not symmetric (`move_prev` saturates at 0, so the first `Up` correctly picks index 0), which makes the asymmetry a pure bug rather than a deliberate convention. Fix by tracking "not yet navigated" (e.g. `Option<usize>` or a bool) so the initial render highlights nothing and the first `Down` selects index 0.

---

### M3 — major — a successful mount triggers a full config reload with unrelated live side effects

`src/app/api/workspaces.rs:285-292` → `src/app/remote_mount.rs:95-99` → `src/app/config_io.rs:130-153`

`record_successful_remote_mount_target` already mutates `self.state.recent_remote_mount_targets` in memory before persisting, so the trailing `apply_config_from_disk(false)` (copied from the `save_theme` shape) is not needed to make the new value live. It is not free:

- `apply_live_config` resets `self.state.agent_panel_scroll = 0` (`src/app/mod.rs:1596`) — the sidebar agent panel jumps to the top every time a federated mount lands, while the user may be scrolled elsewhere;
- it re-clamps `sidebar_width` and, when `ui.copy_on_select = false`, calls `clear_selection()` / `stop_selection_autoscroll_state()` (`src/app/mod.rs:1563-1571`) — a mount completing while the user holds a selection wipes it;
- it sets `config_reloaded_from_disk`, which `src/server/headless.rs:2810` consumes on the *next* client input to run a **second** full `reload_server_config` including `apply_keybindings`;
- it performs a blocking config read + write + full re-parse on the event-loop tick, and does so even when nothing changed: re-mounting a target already at index 0 rewrites a byte-identical list and pays the whole cycle again.

Failure scenario: user scrolls the agent panel down, mounts a remote workspace, and on mount completion the panel snaps to the top for no visible reason. Suggested fix: skip the write when the list is unchanged, and drop the `apply_config_from_disk` call (persist only).

---

### m1 — minor — duplicated cap constant with no code link

`src/ui/remote_mount.rs:26` defines `REMOTE_MOUNT_RECENTS_MAX_ROWS = 5` and only *comments* that it tracks `state::RECENT_REMOTE_MOUNT_TARGETS_CAP` (`src/app/state.rs:697`). If the state cap is raised to 10, the popup silently keeps showing 5 rows while `move_next` (bounded by `recent_remote_mount_targets.len()`, `src/app/remote_mount.rs:122`) navigates to index 9 — highlight invisible, input filled from an off-screen entry. Reference the state constant directly.

### m2 — minor — doc comment describes hover support that does not exist

`src/app/state.rs:642-644`: *"driven by Up/Down keys and mouse hover — reuses the same primitive as the global launcher menu"*. Only click is wired (`src/app/input/mouse.rs:293`); the `MouseEventKind::Moved` path calls `global_menu.hover(...)` / `menu.list.hover(...)` for the menus it claims to mirror but has no arm for `Mode::MountRemoteWorkspace`. Either implement hover (it is the pattern being reused, and the project rule is to reuse existing UI interaction language) or fix the comment.

### m3 — minor — ssh identities are persisted into the hand-edited config file, with no clear/disable affordance

`src/config/model.rs:832`, `src/app/config_io.rs:130`. Usernames and internal hostnames are history/state, not configuration, yet they are written into `~/.config/herdr/config.toml` — the file users hand-edit and very commonly commit to a public dotfiles repo. herdr already has `state_dir()` (`src/config/io.rs:35`) for machine-written state. There is also no UI to clear the list and no setting to turn recording off. Worth a deliberate decision before this ships.

### m4 — minor — new config key missing from the config reference

`src/config/model.rs:832`. `python3 scripts/config_reference_check.py` reports `ui.recent_remote_mount_targets: in src/config but missing from docs/next/website/src/data/config-reference.json`. The check is not part of `just check` (only `just release-docs-check`), and it already fails on this fork for 6 pre-existing keys — so this is added debt, not a new gate break, but it blocks release-docs finalization.

### m5 — minor — test gaps on the properties the change is judged by

- No test asserts that a **failed** or conflict-rejected mount does not record. That is the headline correctness requirement, and it is currently guaranteed only by statement placement inside a `#[cfg(unix)]` handler. A test driving `handle_federation_mount_failed` and asserting `recent_remote_mount_targets.is_empty()` would be cheap and directly on-point.
- `dedup_capped_recent_remote_mount_targets` (`src/app/mod.rs:334`), the entire defense against a hand-edited config, has no test.
- No geometry test at a clamped popup height — that gap is precisely why M1 survives. `remote_mount_popup_grows_only_when_recents_are_present` only exercises a 40-row screen.
- No test for the mouse-click selection path or for `select_recent_remote_mount_target` with an out-of-range index (the documented stale-click no-op).

Tests that do exist assert behavior rather than trivia, and `record_successful_remote_mount_target_persists_most_recent_first` correctly asserts state + on-disk content + absence of a config diagnostic. `up_down_keys_are_a_noop_with_no_recents` is good negative coverage.

### m6 — minor — unrelated reformatting in the diff

`src/app/mod.rs:2351-2355` reflows an existing `assert_eq!` in an unrelated clipboard test into three lines. `cargo fmt --check` accepts both forms, so this is pure diff noise in a change that otherwise touches nothing in that test.

### m7 — minor — recents navigation stays live while a submission is in flight

`src/app/remote_mount.rs:107-131` and `src/app/input/mouse.rs:293-297` mutate `name_input` even when `remote_mount.submitting` is true. A user who selects a different recent while "mounting…" is showing sees their pick silently discarded when the in-flight mount completes and `close_remote_mount_dialog` clears the input. Consider gating on `!submitting`, consistent with the mouse Cancel branch which already refuses to cancel a submitting worktree dialog.

---

## Behaviors flagged as defensible (no change requested)

- **Late success from a dismissed dialog is still recorded.** `record_successful_remote_mount_target` runs before the `claim_abandoned_remote_mount` correlation, so a dial the user walked away from still lands in recents. This is correct and well documented at `src/app/api/workspaces.rs:285-291`: the mount genuinely materialized and is live in `remote_mirrors`, so it *is* a successful target. The alternative (suppressing it) would hide a host the user is actually connected to.
- **Multi-target ordering follows completion order, not typed order.** Submitting `hostA hostB` records whichever mount finishes first at index 0. Inherent to per-target events; harmless.
- **CLI `--remote` mounts are never recorded** (they go through `run_federated_session`, not `workspace.mount_remote`). Recents therefore only reflects API/dialog-initiated mounts. Acceptable given the feature is scoped to the dialog, but worth knowing.
- **Pre-existing, unchanged:** the mouse Cancel branch at `src/app/input/mouse.rs:311-315` sets `self.remote_mount = None` directly instead of calling `close_remote_mount_dialog()`, so mouse-cancelled pending targets never reach `abandoned_remote_mounts`. Not introduced here; recents does not make it worse.

## Architecture verdict

State/runtime separation holds — the mutation logic is a free function over a `Vec<String>`, the selection helper is pure `impl AppState`, and only the `App`-level wrapper touches disk. Render stays pure. No wire/protocol/API surface added, and the field never leaves the TUI/server-render boundary, consistent with the theme/sound precedent. The one architectural wobble is m3: history persisted into the configuration file rather than the state directory.

## Unresolved questions

1. Should a mount the user explicitly walked away from (Esc / mouse cancel) still enter recents? Current behavior says yes and documents why; confirm that matches intent.
2. Should recents live in `state_dir()` rather than `config.toml` (m3), given users version-control their config?
3. Is there an intended "clear recents" affordance, or is hand-editing the config the expected escape hatch?
4. Was the `assert_eq!` reflow in `src/app/mod.rs` intentional, or leftover from an editor pass?
