# Implementation notes — federation UI labels

Active log for this task. Supersedes `plans/260806-2311-merge-upstream-v0.8.0/implementation-notes.md`
as the append target from 2026-08-07 00:41 onward; that file stays frozen as the merge's record.

## Decisions / deviations (append-only)

### 1. Branch stacked on the merge branch, not master
- **What:** `fix/federation-ui-labels` created from `merge/upstream-v0.8.0` (a021433a), not `master`.
- **Why:** upstream v0.8.0 rewrote `src/ui/sidebar.rs` and the agent-list render path — the exact
  code these two bugs live in. A fix authored against `master` would target code PR #10 replaces,
  then conflict on merge and need re-deriving against unfamiliar upstream structure.
- **Evidence:** `src/ui/sidebar.rs` was one of the 40 conflicted files in the v0.8.0 merge; the
  merge's own review flagged `src/ui/panes.rs:172-176` and sidebar-adjacent regions as substantially
  restructured by upstream.
- **Reversibility:** moderate. If PR #10 is reworked, this branch rebases onto whatever replaces it.
  Consequence to remember: **this PR must merge AFTER PR #10.**

### 2. Diagnose before fixing, despite the bugs looking cosmetic
- **What:** spent an evidence pass (2 root-cause agents) before writing any fix.
- **Why:** both symptoms are display-level, but the cause could sit at any of three layers — the
  federation wire never carrying the metadata, local state not storing it for mounted workspaces,
  or the renderer skipping it for remote rows. The fix's size, risk, and whether it needs a
  `FEDERATION_PROTOCOL_VERSION` bump differ completely by layer. Guessing "it's just UI" and
  patching the renderer would silently paper over a missing-data bug.
- **Reversibility:** n/a — this is a process decision, no code changed.

### 3. Both symptoms fixed TUI-only; the branch line is NOT restored over the wire
- **What:** shorten the remote badge to the bare `☁` glyph, move the host string to the dim
  second line (where local rows show their branch), and apply the same badge in the agent-panel
  projection. No federation-wire field added, no `FEDERATION_PROTOCOL_VERSION` bump.
- **Why:** diagnosis proved the folder name IS sent (`WorkspaceInfo.label`) and IS stored
  (`custom_name`) — it was being truncated away because `federation_origin_badge` prepended
  `☁ user@ip#session` into the SAME flexible-width token, and `truncate_end` keeps the prefix
  and drops the suffix. The badge ate the whole budget; `▲ appn-ltu-vm…` is the badge, not a name.
  A remote branch name genuinely does not exist on the wire (no field on `WorkspaceInfo` or
  `WorkspaceWorktreeInfo`), so restoring a literal branch would need a server-side change on the
  REMOTE host. The host origin is the more useful secondary datum anyway and is already local.
- **Evidence:** `src/ui/text.rs:11-24` (`truncate_end` prefix-keep); `src/remote/federation/id.rs:27`
  (`HostKey` = `user@ip#session`); `src/app/creation.rs:639-641` (label → `custom_name`);
  `src/ui/sidebar.rs:329-334` (branch suppressed for federated).
- **Reversibility:** high — pure presentation, no persisted state, no protocol surface.

### 4. `workspace_branch_for_display` kept; a separate secondary-line fn added
- **What:** did NOT overload `workspace_branch_for_display` to return the host.
- **Why:** its return value is also fed to `grouped_child_display_label`, which REPLACES a
  grouped child's visible label with the branch. Overloading it would make an indented remote
  workspace render its host string as its name — reintroducing the exact bug being fixed.
- **Evidence:** `src/ui/sidebar.rs:336-350` + call sites at `:196-208` and `:1306-1314`.
- **Reversibility:** high.

### 5. PR #10's "CLAUDE.md restored to the fork's version" is FALSE — measured a symlink
- **What:** `CLAUDE.md` is a symlink to `AGENTS.md` (9-byte blob). The merge's verification ran
  `git diff master HEAD -- CLAUDE.md`, which compared the symlink target string, found it
  unchanged, and reported the user's "restore the fork's version" decision as applied. The real
  instruction document is `AGENTS.md`, and PR #10 took **upstream's** version of it
  (39 insertions / 25 deletions vs fork master).
- **Why not reverted anyway:** the decision was made on a false premise. Most of upstream's
  AGENTS.md delta is factual description of code this merge brought in — `docs/preview/`,
  `docs/versions/`, the snapshot-based release-docs flow, the agent-detection throwaway-repro
  skill. Reverting wholesale would leave the fork with docs describing a layout that no longer
  exists. Kept upstream's version; surfacing to the user rather than silently deciding either way.
- **Evidence:** `ls -la CLAUDE.md` → `CLAUDE.md -> AGENTS.md`; `git show master:CLAUDE.md | md5`
  == `git show HEAD:CLAUDE.md | md5` (both the string "AGENTS.md");
  `git diff --stat master HEAD -- AGENTS.md` → 39/25.
- **Reversibility:** trivial — `git checkout master -- AGENTS.md` if the user still wants the
  fork's wording.
- **Verification lesson:** a symlinked path makes `git diff <path>` answer a question about the
  link, not the document. Same failure shape as the earlier `git grep` reading the index.
