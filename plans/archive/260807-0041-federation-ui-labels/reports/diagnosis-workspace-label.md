# Diagnosis: remote/mounted workspace doesn't show folder name like local

Branch: fix/federation-ui-labels @ a021433a. Read-only investigation, no edits made.

## Symptom (as understood)
Local sidebar rows: 2 lines — folder/workspace name, then dim `branch` line.
Remote/mounted rows: 1 line — `☁ <badge><truncated-text>`, no dim second line,
and the visible text looks like the SSH/mount target, not the remote folder.

## Confirmed root causes (2, both proven by source)

### A. Missing second (branch) line — INTENTIONAL, not a data-loss bug
`workspace_branch_for_display()` explicitly returns `None` for any workspace
classified as federated:

- `src/ui/sidebar.rs:329-334`
  ```rust
  pub(crate) fn workspace_branch_for_display(ws: &Workspace) -> Option<String> {
      if workspace_federation_origin(ws).is_some() { return None; }
      ws.branch()
  }
  ```
- Comment at `src/ui/sidebar.rs:323-328` states the reasoning: a federated
  workspace's `identity_cwd` is local-machine garbage (meaningless remotely),
  and nothing populates `cached_git_branch` for it — this guard just makes
  the suppression explicit/defensive.
- `workspace_row_height`/`render_workspace_list` feed this `None` into
  `space_rows()` (`src/ui/sidebar/tokens.rs:100-137`); the config's row 2 is
  `[Branch, GitStatus]` (default at `src/config/sidebar.rs:415-424`), and
  `SpaceSidebarToken::Branch => context.branch.map(...)` filters the whole
  row to empty when `branch` is `None`, so the row is dropped, not rendered
  blank — hence one line instead of two.
- Test documenting the intent: `federated_workspace_suppresses_local_git_branch_display`
  (`src/ui/sidebar.rs:2432-2441`).

**Why the data doesn't exist at all, upstream:** the federation wire schema
that carries workspace metadata (`WorkspaceInfo`, `src/api/schema/workspaces.rs:67-81`,
serialized inside `SessionSnapshot`/`MountSnapshot`,
`src/api/schema/session.rs:9-23`, `src/remote/federation/protocol/mod.rs:149-153`)
has **no branch field anywhere** — not on `WorkspaceInfo`, and not on the
nested `WorkspaceWorktreeInfo` (`src/api/schema/workspaces.rs:83-90`, which
only has `repo_key/repo_name/repo_root/checkout_path/is_linked_worktree`).
The remote host never sends a branch name over the wire at all; this isn't
"sent but dropped" or "stored but not rendered" — it is **never sent**.
So even without the `sidebar.rs:329` guard, there is nothing to show.

Ranked hypothesis outcome (task's candidate list, item 3):
- ✅ "metadata is never SENT over the federation wire" — CONFIRMED (no wire field).
- ✅ "render path skips it for remote rows via an explicit guard" — CONFIRMED,
  and it's belt-and-suspenders on top of the wire gap, not the primary cause.
- ❌ "stored but not rendered" — not applicable, nothing is stored either.
- ❌ "git-discovery only runs for local paths" — not literally true;
  `discover_workspace_git_identity`/`git_branch()` (`src/workspace.rs:276,284`)
  would run against whatever `identity_cwd` a materialized remote workspace
  gets, but that `identity_cwd` is set from the remote pane's reported `cwd`
  string (`src/app/creation.rs:664`, `first_pane.cwd`), which is a *remote*
  filesystem path that doesn't exist locally, so any such discovery attempt
  would just silently fail to find a `.git` — this is a secondary/theoretical
  path, not what actually happens today.

### B. Top-line text is the mount target string, truncated — a real display bug
The remote workspace's real folder name **is** sent over the wire and **is**
stored locally, but the render path prepends a badge string in front of it
and then truncates the *combined* string from the right, so on a narrow
sidebar the folder name is exactly what gets cut off.

Evidence chain:

1. **Folder label IS transmitted.** `WorkspaceInfo.label` (`src/api/schema/workspaces.rs:70`)
   is populated server-side from `ws.display_name_from(...)` (folder-basename/
   custom-name derived) at `src/app/creation.rs:525`, and this is the exact
   struct serialized into the `SessionSnapshot` sent as the federation
   `MountSnapshot` (`src/app/api/session.rs:34,45`).
2. **Folder label IS stored locally.** The live mount path
   `App::handle_federation_mount_ready` → `App::materialize_federation_mount`
   (`src/app/api/workspaces.rs:249`, `src/app/creation.rs:570-684`) sets
   `Workspace::from_existing_pane(Some(ws_info.label.clone()), ...)`
   (`src/app/creation.rs:661-662`), which becomes `custom_name`
   (`src/workspace.rs:279`). `display_name_from_terminals()` returns
   `custom_name` verbatim when set (`src/workspace.rs:1173-1180`) — so the
   real remote folder name is present in local state, contradicting the
   "not stored" hypothesis.
3. **The badge is concatenated as a PREFIX onto that same label, as one string,
   before truncation ever sees it.**
   `workspace_display_label_with_origin_badge()` (`src/ui/sidebar.rs:313-321`):
   ```rust
   match federation_origin_badge(ws) {
       Some(badge) => format!("{badge} {label}"),
       None => label,
   }
   ```
   `federation_origin_badge()` (`src/ui/sidebar.rs:305-307`) builds the badge
   as `"☁ {host_key}"`, and `host_key` (`HostKey`, `src/remote/federation/id.rs:20-37`)
   is `format!("{user_at_ip}#{session_discriminator}")` where `user_at_ip` is
   literally the **raw mount target string the user typed/picked**
   (`HostKey::new(target, &session_name)` at `src/app/api/workspaces.rs:105`,
   `target` from the `workspace.mount_remote` request). For an SSH alias like
   `appn-ltu-vm`, the badge becomes `"☁ appn-ltu-vm#<session>"`.
4. **Truncation keeps the prefix, drops the suffix.** This whole
   `"☁ <target>#<session> <folder-label>"` string is passed as a single
   `ResolvedTokenKind::Workspace` token (`src/ui/sidebar.rs:1166-1171`,
   `workspace_row_height` at `src/ui/sidebar.rs:196-224` and
   `render_workspace_list`), and rendered with
   `truncate_end(text, budgets[index])`. `truncate_end`
   (`src/ui/text.rs:11-24`) keeps the **left/prefix** up to the width budget
   and appends `…`, discarding everything after. On a normal/narrow sidebar
   width, `"☁ appn-ltu-vm#<session-disc> "` alone can already consume most or
   all of the available budget, so the real folder name (the suffix) never
   renders — exactly reproducing `▲ appn-ltu-vm…` (the `▲`/triangle is very
   likely a terminal-font fallback glyph for `☁`, U+2601).

This is confirmed as a genuine bug, not intended behavior: nothing in the
badge/label design says the folder name should be silently dropped; the
badge-prepend was added for RT-F8/S11.4 "unspoofable remote-origin badge"
(comment at `src/ui/sidebar.rs:286-293`) as a security/trust concern, without
apparently accounting for what happens to the (equally single, equally
truncated) label suffix when a real host_key/target string is long.

Ranked hypothesis outcome (task's candidate list, item 4/5):
- Top line is NOT displaying "the mount target instead of the folder name"
  by design — the folder name is fetched correctly and stored correctly.
  It is being truncated away because it shares one flexible-width token with
  a long badge prefix that always renders first.
- Truncation is **not** because the stored string is short — `custom_name`
  holds the full remote label. It's `budgets`-driven width truncation
  (`src/ui/sidebar.rs:1044,1124-1142`) of the concatenated
  badge+label string, confirmed by `truncate_end`'s prefix-keep/suffix-drop
  semantics (`src/ui/text.rs:11-24`).

## Where the fix belongs (per repo's runtime/client boundary guardrail)

- **Cause A (branch line):** the missing datum (branch name) is a
  shared runtime/session fact today it's not exposed over the wire at all
  (`WorkspaceInfo`/`WorkspaceWorktreeInfo` have no branch field). Adding it
  is a **server/API + federation-wire** change (new field on `WorkspaceInfo`
  or `WorkspaceWorktreeInfo`, populated server-side from the remote host's
  own git branch and forwarded through `SessionSnapshot`/`MountSnapshot`).
  The suppression guard at `sidebar.rs:329-334` is TUI-only and would stay in
  some form (or be relaxed to show the new remote-origin branch instead of
  the local `cached_git_branch`).
- **Cause B (badge/label truncation):** purely a **TUI/sidebar rendering**
  issue — badge composition (`workspace_display_label_with_origin_badge`) and
  width-budget allocation (`resolved_token_spans`) both live in
  `src/ui/sidebar.rs`/`src/ui/text.rs`. No wire or server-state change needed.

## Protocol version note
`FEDERATION_PROTOCOL_VERSION` currently at 5. A fix for Cause A would add a
new **field** to an existing struct (`WorkspaceInfo` or
`WorkspaceWorktreeInfo`), not a new message variant — per this repo's stated
convention ("a new wire FIELD is additive and usually does NOT need one, a
new message variant does"), this should NOT require a protocol version bump,
as long as the field is `#[serde(default, skip_serializing_if = ...)]`
(matching the existing pattern already used on every optional field in that
struct, e.g. `worktree` at `src/api/schema/workspaces.rs:79-80`). Not
independently verified against the exact protocol-version bump policy in
`src/protocol/wire.rs` (task asked me not to; flagging as the one place to
double-check before shipping).

## Unresolved / open questions
1. Did not verify against a live mount (no build/run performed, per
   read-only constraint) that the rendered glyph is genuinely `☁` displaying
   as `▲` due to font fallback, vs. some other codepoint — inferred from
   `federation_origin_badge`'s `\u{2601}` literal (`src/ui/sidebar.rs:306`)
   and is highly likely but not pixel-confirmed.
2. Did not measure the actual sidebar column width vs. `HostKey` string
   length in the user's real session, so "badge alone consumes the whole
   budget" is argued from the code path and typical host_key lengths, not
   a captured `budgets[index]` value from a live repro.
3. Whether Cause A's fix should show the remote's branch (requires a new
   wire field, server-side git discovery on the *remote* host) or something
   cheaper (e.g. showing the remote's reported repo/worktree label from
   `WorkspaceWorktreeInfo.repo_name`, which IS already sent) is a product
   decision, not something this diagnosis resolves.
4. Whether the badge should instead be a separate, non-flex/fixed-width
   token (so it can never eat the label's truncation budget) or should be
   shortened (e.g. hostname only, drop `#session-discriminator` from display)
   is a design choice for the fix, out of scope here.

Status: DONE
Summary: Two independent, both-proven causes. (A) Missing branch line is by design + upstream data gap — no branch field exists anywhere in the federation wire schema (`WorkspaceInfo`/`WorkspaceWorktreeInfo`), and `sidebar.rs:329-334` explicitly suppresses local git-branch lookup for federated workspaces on top of that. (B) The garbled top-line name is a genuine TUI truncation bug: the real remote folder name IS sent and IS stored (`custom_name` from `WorkspaceInfo.label`), but `workspace_display_label_with_origin_badge` concatenates a `☁ <mount-target>#<session>` badge as a prefix onto it into one flexible-width token, and `truncate_end` (keeps prefix, drops suffix) chops off the folder name first when the badge alone is long. Fix A is federation-wire/server (new optional field); Fix B is pure TUI (sidebar.rs/text.rs).
Concerns: Protocol-version-bump policy for Fix A not independently re-verified against src/protocol/wire.rs; live glyph/width behavior not visually reproduced (read-only task).
