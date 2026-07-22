# Phase 08 — `--remote` federated CLI path + sidebar host labeling + capability fallback

**Goal:** the behavior switch. A feature-flagged flag/subcommand makes the local server create a federated
workspace (P4 mount + P5 panes) and the sidebar labels/groups it by host. Capability negotiation: if the remote
lacks federation, **fall back to today's full-screen attach** (no silent break of a public command). Owns the
unspoofable origin badge (RT-F8). **Depends on:** P5, P6, P7 (P7 green is the default-flip gate). **Blocks:** P9.
**Shippable:** yes (feature-flagged).

## Context
- Verified: today `main.rs:683` → `run_remote()` spawns a standalone client child; guarded mutually exclusive at
  `main.rs:453-464`. `extract_remote_args` at `src/remote/unix.rs:61`.
- Verified: `AppState.workspaces: Vec<Workspace>` (`src/app/state.rs:1323`); sidebar groups by
  `worktree_space().key` (`src/ui/sidebar.rs:318`) — reuse for a per-host remote-origin group;
  `agent_panel_entries_with_runtimes` walks by index (`src/ui/sidebar.rs:113`). Local git metadata
  (`cached_git_branch`, `src/workspace.rs:217-219`) is meaningless for remote — must not shell out locally.
- Verified: capability negotiation exists in P1/P4 (`FederationUnsupported` status) — this phase consumes it for
  fallback.
- Scenario S4.1/S8.1 (focus barrier Blocker), S8.2 (double-attach), S10.1 (single-mount v1), S11.4 (origin badge).
- RT-F8: badge owned HERE (not P7). RT-F11: double-attach detection keyed on `HostKey`.

## Requirements
1. **No silent break + capability fallback (RT-F4):** classic `herdr --remote ssh user@ip` full-screen path is
   unchanged and remains the default. Federation is opt-in via `HERDR_REMOTE_FEDERATION=1` / config / a distinct
   flag (e.g. `--remote-workspace`). When federation is requested but the remote returns `FederationUnsupported`
   (old binary / version skew), **fall back to the classic attach path** with a clear notice — never a hard
   failure of a public command.
2. When federation is enabled + supported, instead of `run_client_process`, the running local server triggers a
   P4 mount (or, if no local server, start/attach one first — reuse session/server autodetect).
3. **Origin badge (RT-F8 / S11.4):** federated workspaces render in a per-host group (reuse `worktree_space`
   grouping) with an unambiguous remote-origin badge/color, using the origin tag P7 guarantees. A crafted
   `custom_name` cannot masquerade as a trusted local workspace. **Badge is owned entirely by this phase.**
   Suppress local git shellouts for federated workspaces.
4. **Focus barrier (S4.1/S8.1 Blocker):** input routing keyed strictly on the namespaced pane id; a focus switch
   is a hard barrier — no in-flight keystroke leaks across the local/remote boundary.
5. **Single-mount enforcement (S10.1):** reject a second concurrent federation mount with a clear message (data
   model is multi-remote-ready; v1 allows one).
6. **Double-attach guard (RT-F11 / S8.2):** detect a classic `--remote` attach to an already-federated host,
   keyed on `HostKey` in the local mount registry; warn/block. (If detection proves costly, downgrade to
   documented-not-enforced for v1 — state the choice.)
7. **Keybindings (S13.2):** federated panes use local-style keybindings (they coexist in one chrome); document.
8. **Clipboard regression note (RT-F7):** document that federated-pane clipboard/image-paste is plumbed (P5/P7)
   — user-facing confirmation it is NOT dropped.

## Files
- **Modify** `src/main.rs` — parse the federation flag; branch `run_remote` vs. federation-mount; keep the
  `453-464` exclusivity semantics (coordinate with P3's `federation-serve` dispatch — different arms).
- **Modify** `src/remote/unix.rs` (`extract_remote_args`, `run_remote`) — federation branch calling P4
  `FederationClient`; capability-fallback to `run_client_process` on `FederationUnsupported`.
- **Modify** `src/ui/sidebar.rs` — per-host remote group + origin badge; skip local git for federated ws.
- **Modify** `src/app/state.rs` / input routing — namespaced-pane-id focus barrier + single-mount + local mount
  registry (host-key keyed) for the double-attach guard (coordinate with P4's mirror field on the same file).
- **Create** config/flag surface for `HERDR_REMOTE_FEDERATION`.

## TDD test plan (tests FIRST)
Run `cargo test -p herdr remote:: sidebar:: app::` + a CLI-parse test:
1. **Classic path unchanged:** flag OFF ⇒ `extract_remote_args` + `run_remote` identical to today (existing
   remote tests green; explicit "flag off = classic" assertion).
2. **Flag ON + supported routes to federation:** parse the flag → federation mount invoked, NOT
   `run_client_process` (mock the mount).
3. **Capability fallback (RT-F4):** flag ON but remote returns `FederationUnsupported` → falls back to
   `run_client_process` (classic attach) with a notice; no hard error.
4. **Focus barrier (S8.1 Blocker):** rapid focus switch between a local and a remote pane with buffered input →
   each keystroke lands on the pane focused at send time; zero cross-boundary leak.
5. **Sidebar grouping/badge (RT-F8):** a federated workspace renders under a per-host group with a remote-origin
   marker distinct from local worktree groups; a spoofed `custom_name` still shows the remote badge; no local git
   shellout invoked.
6. **Single-mount (S10.1):** second concurrent federation mount → rejected with a typed error.
7. **Double-attach guard (RT-F11/S8.2):** classic `--remote` to an already-federated host (same `HostKey`) →
   warned/blocked.

## Implementation steps
1. Write failing tests (1-7).
2. Add the flag/config surface + `main.rs`/`extract_remote_args` branch (default = classic) + capability fallback.
3. Wire the federation-mount trigger to P4; add sidebar host group + badge; suppress local git for federated ws.
4. Implement the focus barrier + single-mount + double-attach guards. Full suite green. **Requires P7 green.**

## Risks + rollback
- **Risk (top-5):** input misrouted to the wrong shell. Mitigation: namespaced-id barrier + test 4. **Risk:**
  silent break of a public command. Mitigation: default OFF, classic path untouched, capability fallback, tests
  1/3. **Rollback:** flip `HERDR_REMOTE_FEDERATION` off (runtime) or revert this commit (classic path never
  modified breakingly).

## File ownership
Shared: `src/main.rs` (with P3), `src/remote/unix.rs`, `src/ui/sidebar.rs`, `src/app/state.rs` (with P4 — P4 adds
the mirror field, P8 adds routing + registry; sequence P8 after P4). Owns the badge (RT-F8). New: flag/config
surface.
