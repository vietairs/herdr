# Federated workspace close — regression tests

Branch `feat/federation-multi-tab-workspace`, worktree
`.claude/worktrees/federation-multi-tab-workspace`. Tests only; production
behavior untouched (fix reviewed and left as-is).

File touched: `src/app/api/workspaces.rs` (test module only).

## Fixture change

`test_federation_mirror_with_workspace(target, generation)` now delegates to a
new `test_federation_mirror_with_workspaces(target, generation, count)` that
builds a snapshot with N remote workspaces, one tab and one pane each
(`w<n>` / `w<n>-tab` / `w<n>-p1`, terminal `t<n>`). Unique per-workspace ids are
what makes the resync-index scoping test observable. Existing callers keep the
1-workspace shape; only the single pane's remote id changed (`p1` → `w1-p1`),
which nothing asserted on.

Mirrored entities land in the indexes under mount-namespaced ids
(`r:<host_key>:<raw>`), so the tests derive keys with a local `ns()` helper
rather than hardcoding.

`AppState::ensure_test_terminals()` is called before the close in every new
test — `Workspace::test_new` attaches a terminal it never registers, so without
it `assert_invariants_for_test()` panics on the seeded local workspace
regardless of the code under test.

## Tests added

All in `src/app/api/workspaces.rs`'s `mod tests`. The three federation ones are
`#[cfg(unix)] #[tokio::test]` (the close path's federation branch is unix-gated);
the worktree-group one is a plain `#[test]`.

1. `closing_one_of_several_federated_workspaces_keeps_siblings_and_mount`
   — mount with 3 mirrored workspaces plus 1 local; closes the middle mirrored
   one. Asserts: exactly one workspace removed (len 4 → 3), both named siblings
   still present by id, both survivors still carry the
   `federation:<host_key>` worktree-space membership, `state.remote_mirrors`
   still contains the `HostKey`, `state.mount_drive_tasks` still contains it
   (link not torn down), and `state.assert_invariants_for_test()`.

2. `closing_the_final_federated_workspace_of_a_mount_ends_it`
   — mount with 2 mirrored workspaces. After the first close the mount must
   still be registered; after the second, `remote_mirrors` and
   `mount_drive_tasks` are both empty, no workspace carries a federation
   worktree space, and only the local workspace remains. Guards the behavior
   the original code was written for. Ends with the state invariants.

3. `closing_one_federated_workspace_purges_only_its_own_resync_entries`
   — mount with 2 mirrored workspaces; seeds one
   `remote_resync_workspace_index` entry per mirrored workspace (that index only
   holds host-announced-but-unmaterialized workspaces, so mount-time does not
   populate it). Closes the first. Asserts `w1`'s entries are gone and `w2`'s
   survive in all three of `remote_resync_pane_index`,
   `remote_resync_tab_index`, `remote_resync_workspace_index`. Plus invariants.

4. `api_workspace_close_still_closes_a_whole_shared_worktree_space_group`
   — two workspaces sharing a non-linked `WorktreeSpaceMembership`
   (`key: "repo-key"`, `is_linked_worktree: false`) plus one unrelated
   workspace. Closing one member must still close both (group semantics of
   `close_selected_workspace`), leaving only the unrelated workspace. Guards
   against the fix over-narrowing. This one passes before and after the fix by
   design — it is the negative control, not the regression proof.

## Proof: tests 1–3 fail against the pre-fix behavior

The three fix points were temporarily reverted in place
(`close_indices_for(index)`-based `closing_ids`, unconditional
`end_federation_mount(host_key)`, and `state.close_selected_workspace()` with
no `close_single_workspace_at` branch), the suite re-run, then the fixed file
restored verbatim from a backup copy. Verbatim failure output:

```
running 4 tests
test app::api::workspaces::tests::api_workspace_close_still_closes_a_whole_shared_worktree_space_group ... ok
test app::api::workspaces::tests::closing_one_federated_workspace_purges_only_its_own_resync_entries ... FAILED
test app::api::workspaces::tests::closing_one_of_several_federated_workspaces_keeps_siblings_and_mount ... FAILED
test app::api::workspaces::tests::closing_the_final_federated_workspace_of_a_mount_ends_it ... FAILED

failures:

---- app::api::workspaces::tests::closing_one_federated_workspace_purges_only_its_own_resync_entries stdout ----

thread '...closing_one_federated_workspace_purges_only_its_own_resync_entries' (43883315) panicked at src/app/api/workspaces.rs:2838:9:
a sibling workspace's pane index entry must survive

---- app::api::workspaces::tests::closing_one_of_several_federated_workspaces_keeps_siblings_and_mount stdout ----

thread '...closing_one_of_several_federated_workspaces_keeps_siblings_and_mount' (43883427) panicked at src/app/api/workspaces.rs:2678:9:
assertion `left == right` failed: closing one mirrored workspace must remove exactly one, not the whole mount group
  left: 1
 right: 3

---- app::api::workspaces::tests::closing_the_final_federated_workspace_of_a_mount_ends_it stdout ----

thread '...closing_the_final_federated_workspace_of_a_mount_ends_it' (43883738) panicked at src/app/api/workspaces.rs:2745:9:
one mirrored workspace still remains, so the mount must stay live

test result: FAILED. 1 passed; 3 failed; 0 ignored; 0 measured; 3416 filtered out
```

Reading of the failures: test 1's `left: 1` is the live bug exactly — all three
mirrored workspaces were destroyed by closing one, leaving only the local
workspace. Test 3 shows the group-wide purge wiping the surviving sibling's
pane index. Test 2 failed on its *intermediate* assertion (the mount was ended
while a sibling remained); its final last-workspace assertions were never
reached, and they pass post-fix — so it covers both directions.

After restoring the fix, all four pass.

## Verification

- `cargo test --bin herdr -- --test-threads=2`: 3419 passed, 1 failed —
  `api::server::pane_graphics_stream::tests::inactive_owner_cancels_idle_stream_and_dispatches_close`,
  the documented load flake; re-run in isolation: passes. Baseline was 3416, so
  the 4 new tests are all present and green (3420 total, 3419 + 1 flake).
- `cargo fmt --check`: clean.
- `cargo clippy --all-targets`: 0 warnings.

No production code changed; `git diff src/app/api/workspaces.rs` still carries
the original fix hunks plus the test additions. Nothing committed.

## Unresolved questions

- None. The production fix looks correct as written: `siblings_remain` is
  computed against the live `remote_mirrors` registry via
  `federation_host_key_for_workspace`, which is the same key materialization
  writes, and `close_single_workspace_at` clears `worktree_space` before
  delegating so the group path cannot re-widen.
