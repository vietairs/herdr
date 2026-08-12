# Codex gpt-5.6-sol adversarial review — P9.2b option (b) own-in-proc session design

Reviewer: codex-cli 0.144.1, `gpt-5.6-sol`, xhigh, read-only, workdir `~/Projects/herdr`. Date 260714.
Target: `phase-09b-option-b-own-in-proc-session.md` (verified against source). No files changed.

## Verdict: UNSOUND (but tractable — feasible model, correctness defects, not an architectural dead-end)

Option (b)'s local in-process model is feasible and DOES remove option-(a)'s mutex / deferred-dispatch
/ render-path failures. But the live flip remains blocked by four critical correctness defects,
including a deeper remote-runtime ownership flaw.

## Findings

1. **[CRITICAL] Federation mounts a transient DUPLICATE App, not the live remote session.**
   The federation dial launches a separate `federation-serve` process (unix.rs:353-379, 301-338);
   `AppFederationHost::boot` constructs a NEW persistence-enabled `App` instead of connecting to the
   existing server (serve.rs:476-500); restore creates fresh shells (persist/restore.rs:64-92,
   564-612). EOF ends `federation-serve` (serve.rs:252-285) whose local runtimes kill children on
   drop (pane.rs:1226-1238, 2640-2654). Neither reflects live remote panes NOR satisfies "disconnects,
   never kills" (plan.md:141-148). [This is the pre-existing CARRIED GAP #1 surfacing as a blocker.]
   **Fix:** remote server owns the federation endpoint/runtime; `federation-serve` must PROXY to that
   server, never restore a second `App`. → architecture decision.

2. **[CRITICAL] Tunnel failure is invisible; panes freeze indefinitely.** Plan spawns the driver then
   awaits only `app.run()` (phase-09b:68-72). Driver returns on EOF/gap/reset/error (client.rs:390-431);
   writer silently exits on write error (client.rs:444-460); pane readers deliberately emit no
   disconnect event (pane_source.rs:106-114); `App::run` has no federation completion branch
   (app/mod.rs:1060-1077). **Fix:** supervise App+reader+writer+child together; any transport
   completion must terminate or explicitly disconnect the App.

3. **[CRITICAL] Post-start classic fallback is unsafe with current stdin ownership.** `App::run`
   starts a detached input thread that locks+reads stdin forever, no cancellation/join
   (app/mod.rs:873-876, raw_input.rs:441-474). Classic fallback then launches another process
   inheriting that stdin (unix.rs:2166-2185); restoring ratatui does not stop the competing reader.
   **Fix:** restrict classic fallback to failures BEFORE `App::run` (exit to shell on later
   disconnect), OR make raw-input ownership cancellable+joinable. RAII terminal guard before any
   fallback.

4. **[CRITICAL] Named sessions still target the default session.** Federation command explicitly omits
   `--session` (unix.rs:186-193) while the mount is namespaced with the requested session name
   (unix.rs:321-336) → default-session data under a named-session identity, input routed to wrong
   processes. **Fix:** `exec <herdr> --session <quoted-name> federation-serve`. [small, concrete]

5. **[MAJOR] The dedicated App is not put into a valid federated UI mode.** Fresh
   `App::new(...,true,...)` starts `active=None`, `Mode::Navigate` (app/mod.rs:377-386, 489-497);
   materialization pushes workspaces but never activates one (creation.rs:600-627); empty/unrepresentable
   mounts return success, after which `App::run` creates a LOCAL workspace (creation.rs:933-949,
   app/mod.rs:1132-1145); normal App actions can also create local workspaces/tabs (app/mod.rs:917-943).
   **Fix:** explicit federated-session mode — require nonempty materialization, activate a returned
   workspace, enter terminal mode, disable local creation/default-workspace behavior.

6. **[MAJOR] "Decide route, then dial once" cannot use the current route API.** `Federated` requires
   an already-successful mount (unix.rs:264-280); `run_remote` mounts before deciding (unix.rs:381-397);
   no cheap capability probe — negotiation+snapshot are one `connect_and_mount` (client.rs:134-218).
   **Fix:** branch on the opt-in, perform ONE live dial/mount, match that result directly. Don't route
   first and redial.

7. **[MAJOR] M10 is only PARTLY a limitation; close/gap/overflow are correctness FAILURES.** Event
   frames carry no delta, application only advances a cursor (protocol/mod.rs:86-103, reducer.rs:223-258).
   Worse: remote output closure sends no terminal `Close` (serve.rs:417-452); gaps stop all terminal
   demux (client.rs:404-411); queue overflow silently drops stateful VT bytes (client.rs:341-357). New
   panes/renames MAY be documented as omitted; silent ghost panes, frozen streams, corrupted emulators
   MAY NOT. **Fix:** v1 minimum = terminal-close/disconnect propagation + fail-fast gap handling;
   overflow must mark desync and reopen/resnapshot, not continue silently.

8. **[MAJOR] M9's clipboard non-goal is not type-correct as written.** Materialization requires
   `UnboundedSender<ClipboardMessage>` (creation.rs:521-527); driver requires bounded `Sender` (client.rs:390-397).
   One `clipboard_tx` cannot serve both; an undrained unbounded receiver risks accumulation. **Fix:**
   separate inbound-bounded-discard and local-emulator-OSC52-discard sinks; explicitly drop receivers
   if relay disabled.

9. **[MAJOR] C5 teardown needs exception-safe OWNERSHIP, not a tail sequence.** Current mount calls
   `start_kill()` without reaping (unix.rs:338-350); pane runtimes retain writer senders
   (creation.rs:713-727). **`CancellationToken` is NOT available — only Tokio is declared
   (Cargo.toml:38).** **Fix:** guards from child creation onward, kill-on-drop as last resort, drop App
   first, cancel-and-await reader, abort-and-await writer, kill-and-wait SSH, restore TTY, then shut
   down runtime. Pipe/drain SSH stderr so warnings can't write into the ratatui screen.

10. **[MINOR] C2 defensible, but a comment/debug-assert is insufficient fencing.** Fresh client =
    generation 1 (client.rs:115-126, 204-209) matching server constant; routing ignores carried
    generation (client.rs:341-365). **Fix:** enforce fresh-client/single-dial structurally + a
    RELEASE-mode equality check before routing.

## Does (b) escape option-(a)'s criticals? — Partially

- C1 deadlock: YES (materialization eager-opens every pane; App retains only receiver+out_tx, never
  re-touches router/mirror; moving both into one reader task is sound).
- C3 (SSH stdio): foreground SSH/config/TTY solved; named-session correctness is NOT (finding #4).
- C4 (deferred dispatch): GONE.
- M11 (render path): GONE — App/ratatui renders local remote-backed runtimes directly.
- C5: simpler ownership, still critical (failure supervision + stdin lifetime + exception-safe cleanup absent).
- M8: shared-session collision gone initially, but normal App still permits local-pane creation unless restricted (finding #5).
- SSH child pipes + generic protocol types are workable: owned reader/mirror/router move into an
  `async move` task with Send+'static (spawn_mount_writer already requires those bounds, client.rs:444-452).

## V1 scoping judgment

- M9 agent-status/clipboard: defensible behind a flag IF both flows explicitly+safely discarded.
- M10 topology: additions/renames/exact layout = OK as documented non-goals. Pane exit/close, link
  failure, gaps, byte-stream desync = correctness bugs, NOT documentation issues.
- C2 reconnect/re-fence: defensible with one fresh client, one connection, no reconnect, + a real
  release fence.
- No local-pane coexistence: defensible ONLY if enforced by the dedicated App mode (finding #5).

## What the plan got right (verified)

- `run_remote` runs before normal server/client or monolithic startup — no inherent nested-ratatui
  conflict (main.rs:692-700).
- Foreground multi-thread runtime + inline `App::run` is viable; App needs no thin-client renderer.
- Eager materialization genuinely avoids (a)'s router mutex deadlock.
- Fresh no-session App makes local partial-state rollback feasible (dropping it releases earlier
  remote runtimes, no persisted half-session) — still needs the cleanup guards.
- Single-dial achievable by replacing the whole route/mount block, not adding to the `Federated` arm.

## Unresolved decisions (need user)

- Must federation PROXY the existing remote daemon, or should `federation-serve` become a separately
  durable remote runtime? (finding #1 — the deepest issue)
- After interactive startup, should transport failure EXIT cleanly to the shell, or is classic
  fallback important enough to justify cancellable stdin ownership? (finding #3)
