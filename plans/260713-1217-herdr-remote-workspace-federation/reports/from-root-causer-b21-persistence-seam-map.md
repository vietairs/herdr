# b2.1 SessionPersistencePolicy — persistence choke-point seam map

Repo: `~/Projects/herdr`, branch `feat/remote-workspace-federation`. READ-ONLY scout, no build run.
Scope: locate every current `no_session`-gated persistence choke point so an immutable
`SessionPersistencePolicy::Disabled` can be designed to intercept the same set (or fewer, more
centralized, seams) without changing classic (`no_session=false`) behavior.

---

## 1. `no_session` field: definition, construction, every read site

Definition — `src/app/mod.rs:103`:
```rust
pub(crate) no_session: bool,
```
Plain `bool` field on `App`, not an enum/policy type today.

Set at construction — threaded as a **constructor parameter**, `src/app/mod.rs:353-359`:
```rust
pub fn new(
    config: &Config,
    no_session: bool,
    config_diagnostic: Option<String>,
    api_rx: tokio::sync::mpsc::UnboundedReceiver<crate::api::ApiRequestMessage>,
    event_hub: crate::api::EventHub,
) -> Self {
```
Also force-mutated post-construction in two flows:
- `src/app/mod.rs:776` (`App::new_from_handoff`) — `app.no_session = false;` after calling
  `Self::new(config, true, ...)` internally (see item 6).
- `src/server/headless.rs:1193` — `self.app.no_session = true;` (headless server flips a live App
  into no-session mode at some lifecycle point — worth checking before assuming `no_session` is
  write-once after construction).
- Test-only mutations: `src/app/mod.rs:4131,4204,4223,4235,4242` (`app.no_session = true/false`)
  — test harnesses reaching into the field directly (only possible because it's `bool`, not an
  encapsulated policy type — an enum type change should keep these compiling via test-only setters
  or keep the field crate-visible).

Every non-test read site (`grep -rn "no_session" src/`):
| Site | What it gates |
|---|---|
| `src/main.rs:706,710` | CLI `--no-session` flag parse; skips server/client autodetect |
| `src/main.rs:796` | monolithic mode literal `true` passed into `App::new` |
| `src/app/session.rs:15,61,91` | schedule/start/save-now gates (item 4/7 below) |
| `src/app/runtime.rs:466,486` | background update-check enable gates |
| `src/app/mod.rs:203-212` (`auto_updates_enabled`, `background_update_check_enabled`, `load_plugin_registry`) | derived gates, not persistence but same flag |
| `src/app/mod.rs:355,377,514,651,676,678,737` | constructor: threading + restore-gate (item 2) + `detach_exits: no_session` + plugin registry load gate |
| `src/app/mod.rs:776,778,785` | `new_from_handoff` override + update-check re-gate |
| `src/app/mod.rs:1108` | exit-save gate (item 3) |
| `src/app/mod.rs:1438-1440,1460,1472` | config-reload history-clear gate (item 5) + update-check re-gate |
| `src/app/api/plugins/mod.rs:586` | plugin-registry-related gate (not session persistence, adjacent) |
| `src/server/headless.rs:682,1193,4114,4125` | headless server save-on-exit gate + forced no_session=true + explicit `false` comment "Server always does session persistence" |

**Observation:** `no_session` is not one seam — it's a single `bool` fanned out to ~6 independent
gate sites (restore, save-schedule, save-now, exit-save, history-clear, background-update-check,
plugin-registry-load). A `SessionPersistencePolicy` that only replaces the bool 1:1 at each site
carries the same fan-out risk; a real improvement is a policy method (`policy.disabled()`) called
identically at each site, OR collapsing the persistence-only subset (restore/save/history/clear —
NOT update-check/plugin-registry, which are unrelated concerns riding the same flag) into fewer
choke points (see recommendation).

---

## 2. RESTORE gate — `src/app/mod.rs:366-405`

```rust
// Try to restore previous session
let mut restored_terminals = std::collections::HashMap::new();
let mut restored_terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
let (
    workspaces, active, selected, sidebar_width, sidebar_width_source,
    sidebar_section_split, collapsed_space_keys,
) = if no_session {
    (Vec::new(), None, 0, config.ui.sidebar_width,
     state::SidebarWidthSource::ConfigDefault, 0.5_f32, std::collections::HashSet::new())
} else if let Some(snap) = crate::persist::load() {
    let history = config.experimental.pane_history.then(crate::persist::load_history).flatten();
    let (ws, terminals, terminal_runtimes) = crate::persist::restore(&snap, history.as_ref(), ...);
    ...
```
Gating: direct `if no_session { <empty defaults> } else if let Some(snap) = persist::load() { restore }`
— a plain `if/else` branch inside `App::new`, evaluated once at construction. `persist::load()` is
never even called when `no_session` is true (short-circuited, not just discarded).

---

## 3. EXIT-SAVE gate — `src/app/mod.rs:1107-1110`

```rust
// Save session on exit (skip in --no-session mode)
if !self.no_session {
    self.save_session_now();
}
```
Gating: single `if !self.no_session` guard around the terminal `App::run` event-loop's post-loop
call to `save_session_now()` (item 7's entry point).

---

## 4. SAVE-SCHEDULING on materialization — `src/app/creation.rs:681-684`

```rust
for ws_idx in &created_ws_idxs {
    self.emit_workspace_open_events(*ws_idx);
    self.schedule_session_save();
}
```
This is inside `materialize_federation_mount` (`src/app/creation.rs:521`), the fn that turns a
federation mount's remote workspaces into local `App` state — i.e. the exact call site a future
federated in-proc session (b2/b3) will run through. It calls `self.schedule_session_save()`
unconditionally at the call site — **but** `schedule_session_save()` itself (`src/app/session.rs:
14-18`) already re-checks `self.no_session` internally:
```rust
pub(super) fn schedule_session_save(&mut self) {
    if !self.no_session {
        self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
    }
}
```
So today, a federation-mount `App` constructed with `no_session: true` already gets a no-op here.
Confirmed by the two current federation test harnesses that construct exactly this way:
`src/server/federation_accept.rs:1075` and `src/server/federation_actor.rs:281`, both:
```rust
crate::app::App::new(&config, true, None, api_rx, crate::api::EventHub::default())
```
No production (non-test) federated-App construction exists yet on this branch — `b2`/`b3`
(`run_federated_session`, live flip) are still dormant per
`plans/260713-1217-herdr-remote-workspace-federation/phase-09b-option-b-own-in-proc-session.md:127-158`.

---

## 5. HISTORY CLEAR — two distinct call sites, correcting the assumed `src/session.rs:39-105` location

`src/session.rs` (top-level, 39-105) is the **multi-session CLI name-resolution** module
(`configure_from_args`, `apply_explicit_name` etc. — session *naming*, not persistence state). The
actual "clear" functions live in `src/persist/io.rs`:

```rust
// src/persist/io.rs:96-111
pub fn clear() {
    let path = session_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
        return;
    }
    clear_history();
    crate::logging::session_cleared(&path);
}

pub fn clear_history() {
    let path = session_history_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
    }
}
```
Callers:
- `crate::persist::clear()` — only called from `run_session_save_job` (`src/app/session.rs:103`,
  the `SessionSaveJob::Clear` arm), itself only reached via `save_session_now` /
  `start_background_session_save`, both already `no_session`-gated (item 7).
- `crate::persist::clear_history()` — called directly (bypassing the save-job path) from
  `src/app/mod.rs:1441`, inside `App::update_config` on a live config reload:
```rust
// Only touch the shared on-disk history for a session that actually
// persists. A no-session App (monolithic mode, or a federated mount
// displaying a remote workspace) must never mutate the classic saved
// snapshot — this was the one persistence write path not already
// gated by `no_session` (codex C3), so a config reload here could
// clear the local session history out from under a real session.
if !self.persist_pane_history && !self.no_session {
    crate::persist::clear_history();
}
```
This comment (already present on this branch) is the direct ancestor of the "clear" requirement in
`SessionPersistencePolicy` — it documents that this exact call site was found and gate-patched as
part of the codex adversarial review C3 finding referenced in the phase plan
(`phase-09b-option-b-own-in-proc-session.md:130-134`). This is currently a **second, independent**
`no_session` check, not reachable from the save/restore code path.

---

## 6. App constructor(s) — `App::new` / `App::new_from_handoff`

`grep -rn "fn new" src/app/` → two real constructors on `App` (plus unrelated `state.rs` value types):
- `src/app/mod.rs:353` `pub fn new(config: &Config, no_session: bool, config_diagnostic: Option<String>, api_rx: ..., event_hub: ...) -> Self` — `no_session` is param #2, threaded straight into the restore branch (item 2) and stored as the field (item 1).
- `src/app/mod.rs:752` `pub fn new_from_handoff(config: &Config, config_diagnostic: Option<String>, api_rx: ..., event_hub: ..., snapshot: &SessionSnapshot, imports: &mut HashMap<...>) -> io::Result<Self>`:
```rust
let mut app = Self::new(config, true, config_diagnostic, api_rx, event_hub);
let (workspaces, terminals, runtimes) = crate::persist::restore_handoff(snapshot, ...)?;
...
app.no_session = false;
```
Handoff always constructs internally with `no_session=true` (to skip the normal disk-restore
branch — it supplies its own `snapshot` argument instead) and then flips the field to `false`
post-construction to re-enable normal persistence for the rest of the App's life (this is the
**exact pattern** — construct-disabled-then-flip — that a `SessionPersistencePolicy` field should
either preserve via a builder/setter, or (better) avoid needing by giving `new_from_handoff` its
own explicit policy argument up front instead of relying on a post-construction mutation.

**Choke point for the new field:** the natural insertion point is `App::new`'s parameter #2 (today
`no_session: bool`) — swap to `policy: SessionPersistencePolicy` (or add alongside, deprecate the
bool) since every one of items 2-5's gates is reachable from a value threaded through this single
constructor param.

---

## 7. SAVE write path — from schedule-site to disk

Chain: `schedule_session_save()` (`src/app/session.rs:14`, sets a debounce deadline) →
`sync_session_save_schedule` (`:20`, called when `state.session_dirty`) → main loop timer fires →
`start_background_session_save()` (`:60-84`, gated `if self.no_session { return }` at `:61`) spawns
a thread running `run_session_save_job` → OR synchronous `save_session_now()` (`:86-98`, gated
`if self.no_session { return }` at `:91`) → both funnel into:
```rust
// src/app/session.rs:101-108
fn run_session_save_job(job: SessionSaveJob) {
    match job {
        SessionSaveJob::Clear => crate::persist::clear(),
        SessionSaveJob::Save { snapshot, history } => {
            crate::persist::save(&snapshot, history.as_ref());
        }
    }
}
```
→ `crate::persist::save` / `crate::persist::clear` (`src/persist/io.rs:86-111`) → both bottom out
in `save_to_path` / `clear_path` (`src/persist/io.rs:44-84`), which do the actual
`std::fs::write`/`std::fs::rename`/`std::fs::remove_file` against `session_path()` /
`session_history_path()` (`src/persist/io.rs:10-16`, both under `crate::session::data_dir()`).

**Single true disk choke point:** `src/persist/io.rs` — `save_to_path` (write) and `clear_path`
(remove), both `pub(super)`, only reachable through the four `pub fn` entry points `save`, `clear`,
`clear_history`, `load`/`load_history` at the top of that file. Every current in-process persistence
mutation (save, clear-on-empty, history-clear) funnels through these four functions and no other
path writes `session.json`/`session-history.json`.

---

## MINIMAL SEAM RECOMMENDATION

Two viable designs, ranked:

**Option A (recommended) — thread the policy through the App constructor, keep today's gate shape:**
Replace the `no_session: bool` constructor param / field with `SessionPersistencePolicy` (`enum {
Disabled, OnDisk }` or similar), and mechanically swap each existing `if no_session` /
`if !self.no_session` check to `if policy.is_disabled()` / `if !policy.is_disabled()` at the
**same five sites** already found:
1. `src/app/mod.rs:377` (restore branch condition)
2. `src/app/session.rs:15` (`schedule_session_save`)
3. `src/app/session.rs:61` (`start_background_session_save`)
4. `src/app/session.rs:91` (`save_session_now`)
5. `src/app/mod.rs:1108` (exit-save)
6. `src/app/mod.rs:1440` (history-clear on config reload — currently a *second, independent* check; must not be missed, since it doesn't route through any of 2-5)

This is the smallest diff (rename-and-typecheck), preserves exact classic-mode byte-for-byte
behavior (same branches, same order, just a differently-typed condition), and is what the
`no_session` field's own fan-out already proves is sufficient today — items 2-5 in this report ARE
that gate set, and (4) schedule-session-save + (7) the disk write path already collapse into (2)/(3)
transitively, so there is no additional undiscovered save path.

Do NOT try to intercept only at the single disk-write choke point (`persist/io.rs`'s `save`/
`clear`/`clear_history`) instead of the six sites above — `crate::persist` is a free-standing module
with no knowledge of any particular `App`/policy instance, so pushing the gate down there would
either require passing the policy into every `persist::*` call (churns a stable, App-agnostic
module) or a global/thread-local flag (worse: reintroduces exactly the kind of implicit global state
`no_session`-as-a-field was designed to avoid). Keep the gate at the `App`-owned call sites; the
`persist` module stays a dumb, policy-unaware I/O layer.

**Option B — do not thread through `App::new`'s bool param at all; give `App` a policy field set once
via a dedicated setter/builder method, decoupled from the `no_session: bool` constructor arg.** This
would avoid touching `App::new`'s signature (avoids a mechanical ripple through ~80 call sites,
mostly `#[cfg(test)]` harnesses per the earlier grep), but leaves two states in sync (`no_session`
bool AND the new policy) unless `no_session` is fully retired in favor of `policy.is_disabled()`
everywhere — more moving parts, more chance of drift between the two. **Rejected** in favor of A:
the ~80 call sites are almost entirely `App::new(&config, true, ...)` / `App::new(&config, false,
...)` test literals (`grep` above), which is a mechanical `true`→`SessionPersistencePolicy::Disabled`
/ `false`→`SessionPersistencePolicy::OnDisk` (or similar) find-and-replace, not meaningfully more
work than adding a parallel field.

`new_from_handoff`'s construct-with-true-then-flip-to-false pattern (`src/app/mod.rs:763,776`)
should be preserved as construct-with-`Disabled`-then-flip-to-`OnDisk` (or given its own explicit
policy arg) — it is intentional (skip the normal-restore branch, use the supplied snapshot instead)
and not a bug to fix in this seam map.

---

## Unresolved / to verify before implementing

- `src/server/headless.rs:1193` sets `self.app.no_session = true` on a **live, already-constructed**
  App outside `App::new`/`new_from_handoff` — need to read the surrounding function (not scouted in
  this pass) to confirm whether an immutable `SessionPersistencePolicy` (no runtime mutation) can
  actually replace this site, or whether it needs to stay a runtime-settable field for this one
  call, which would conflict with "immutable" framing in the b2.1 goal.
- `src/app/api/plugins/mod.rs:586` and the `load_plugin_registry`/`background_update_check_enabled`
  gates use `no_session` for unrelated concerns (plugin registry load, auto-update checks) — if
  `no_session` bool is fully retired in favor of the new policy type, these call sites need a
  decision: keep reading `policy.is_disabled()` (fine, just naming) or split into a second concern
  flag. Not a persistence bug, just a naming/scope question for the eventual full replace.
