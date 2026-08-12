# Auto-decisions — federation multi-tab / multi-workspace

Run mode: `--auto` (user-passed). Gates auto-adjudicated with conservative bias; recorded here for later audit.

## D1 — Route

**What:** Treated as a code-changing medium/high-risk route: isolated worktree -> root-cause -> phased implementation -> review -> tests. No `/ak-brainstorm` stage.
**Why:** Root cause was provable from source (two read-only investigation agents), so there was no design debate to hold. Scope was concrete once proven.
**Risk:** Low. Touches federation state identity, which is refactor-risk per CLAUDE.md; mitigated by requiring characterization tests.
**Alternatives rejected:** Brainstorm gate (nothing to debate — the bug has one correct fix).
**Reversibility:** Full — worktree is isolated, nothing pushed.

## D2 — Phase 1 carries no wire version bump

**What:** The tab-collapse fix is client-side only; `FEDERATION_PROTOCOL_VERSION` and `PROTOCOL_VERSION` stay unchanged in Phase 1.
**Why:** `PaneInfo.tab_id` is already on the wire (`src/api/schema/panes.rs:402`); the defect is a local in-process struct dropping an already-available field. Bumping would falsely signal a wire-incompatible change and force needless peer-version churn.
**Risk:** Low. Implementer instructed to report BLOCKED rather than bump if a wire change turns out to be needed.
**Alternatives rejected:** Bump defensively — would break mixed-version mounts for no reason.
**Reversibility:** Full.

## D3 — Phase 2 workspace-create reply payload

**What:** `WorkspaceCreateResponse::Created` carries only the new ids; the serving host's own `workspace.create` defaults decide label/cwd. Client may send an optional label hint.
**Why:** Flagged unresolved by the design scout (product call, no repo fact decides it). Smallest viable surface; matches `SplitPaneRequest`'s existing shape. Label/cwd negotiation can be added later without a second wire break, since the field is optional.
**Risk:** Low — cosmetic only; a wrong default is a rename away.
**Alternatives rejected:** Full label/cwd negotiation in v1 (YAGNI, wider wire surface).
**Reversibility:** High — additive optional field.

## D4 — No per-connection cap on remotely-created workspaces

**What:** Phase 2 adds no rate limit or count cap on `WorkspaceCreateRequest`.
**Why:** Parity with the existing `SplitPaneRequest`/`ClosePaneRequest` handlers, which have no cap either (`src/server/federation_accept.rs`). The peer already holds the single-controller lease over an authenticated SSH tunnel and can already spawn unbounded panes, so a cap here would not change the threat model.
**Risk:** Low-medium. A buggy or hostile controller could create many workspaces. Not a new capability — the same actor can already create unbounded panes.
**Alternatives rejected:** Add a cap now — would be the only capped request handler, inconsistent, and does not close the actual resource path.
**Reversibility:** High — a cap is additive later.

## D5 — Multi-workspace is inside one mount, not repeated mounts

**What:** `HostKey`, the "already mounted" rejection, and the mount dialog's target parsing are left untouched. N remote workspaces come from one tunnel.
**Why:** One tunnel per host is the correct scoping; `materialize_federation_mount` already creates one local workspace per remote workspace, so the gap is the missing create command, not mount identity.
**Risk:** Low.
**Alternatives rejected:** Allow repeated mounts of the same host — would multiply SSH tunnels and fight the single-controller lease.
**Reversibility:** Full.

## Outstanding — needs human judgment before merge

- No live validation against `appn-ltu-vm-105` was possible in-session (read-only, no VM access). The defect is structurally proven from source, but the exact trigger timing for the reported incident is unconfirmed. A real two-tab mount test against the VM is owed before merge.
