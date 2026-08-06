# Security scan — upstream v0.8.0 merge

Scope: uncommitted merge on `merge/upstream-v0.8.0`. Six trust boundaries: federation wire,
remote pane output, OSC 52 clipboard, clipboard staging dir, SSH mount construction, PTY/sockets.
Method: `git diff master -- <path>` (fork→merged) + targeted reads. Read-only, no build.

Agent was read-only (no Write tool); findings transcribed by orchestrator. Claims F1 and F2
independently re-verified by orchestrator before transcription — see "Orchestrator verification".

## Result: no merge-introduced security regression on the audited boundaries

| # | Finding | Class | Severity |
|---|---------|-------|----------|
| F1 | SSH `-oProxyCommand` / flag-injection guard still closed | PRE-EXISTING, unmodified | verification |
| F2 | Federation core byte-for-byte unchanged by merge | — | informational |
| F3 | Remote-origin OSC 52 clipboard policy gate intact | — | informational |
| F4 | Hand-rolled OSC52 parser replaced by vendored libghostty-vt extraction | MERGE-INTRODUCED refactor | informational (trust shift) |
| F5 | Federation protocol 4→5 is hardening, not weakening | MERGE-INTRODUCED | informational |
| F6 | No secrets in merge diff (count: 0) | — | n/a |
| F7 | Clipboard staging symlink weakness untouched, NOT widened | PRE-EXISTING | out of merge scope |
| F8 | `socket_paths.rs` diff is dead-code removal, no behavior change | MERGE-INTRODUCED | informational |

## Detail

**F1** `src/remote/unix.rs:289` — `validate_remote_target()` rejects any target starting with `-`,
blocking `-oProxyCommand=...`. Test at `:3090`. The merge's only change to this file is a 10-line
PATH-discovery retry via `/bin/sh` for non-POSIX login shells (~`:1402-1412`); validator and all
call sites untouched.

**F2** Zero diff vs fork master across `session.rs`, `pane_source.rs`, `serve.rs`, `loopback.rs`,
`mod.rs`, `protocol/codec.rs`, `protocol/negotiate.rs`, `id.rs`, `tee.rs`, `reducer.rs`,
`file_staging.rs`, `sanitize.rs`, `server/federation_accept.rs`. That is the whole federation
attack surface — frame deserialization, remote-origin clipboard gating, file-staging path handling.
Only `client.rs` (12 lines, mechanical `AtomicBool`→`RenderSignal`) and `protocol/mod.rs`
(version constant + comment) changed.

**F3** `src/app/api.rs:104-118` still gates on `accept_remote_clipboard_writes` before
`selection::write_osc52_bytes`, never a server-side `platform::write_clipboard`. This is the exact
shape of the previously-shipped fix for silently-dropped remote clipboard writes. Appears as
unchanged context in the diff. `src/app/api/workspaces.rs:203-249` still routes the mirror's
outbound clipboard through `AppEvent::ClipboardWrite { origin: Some(host) }`.

**F4** `Osc52Forwarder` + `parse_osc52_clipboard_write` deleted; replaced by
`src/ghostty/mod.rs:608` push into vendored callback state → `take_clipboard_writes()` (`:983-985`)
→ `src/pane/terminal.rs:1202` → `src/pane.rs:2067/2318/2496` → the F3 path. New regression test
`terminal.rs:3598 seeded_history_clipboard_write_does_not_leak_into_live_output`.
**Trust shift:** a security-relevant parser in this repo is now upstream vendor C/Zig code that
this pass did not audit.

**F5** Bump documented as reject-on-decode-mismatch for `EventKind::WorkspaceReordered`.
Handshake still hard-rejects on version mismatch (`negotiate.rs` zero-diff per F2).

**F6** Secret-pattern grep over `git diff master -- src/`: 0 matches. No values printed.

**F7** `src/server/clipboard_image.rs`, `src/image_path.rs`: zero diff. Known pre-existing symlink
weakness (follows symlink then sweeps target; Linux shared `/tmp`) is unchanged — not widened.

**F8** `src/server/socket_paths.rs`: removed branch body was textually identical to the
fallthrough. No socket-path or permission change.

## Orchestrator verification (independent of agent report)

- F2 re-run: `git diff --stat master -- src/remote/` → exactly 3 files, 27 insertions / 7 deletions
  (`client.rs` 12, `protocol/mod.rs` 12, `unix.rs` 10). Federation-core file list: empty diff. CONFIRMED.
- F1 re-run: `grep -n "starts_with('-')" src/remote/unix.rs` → `289`. CONFIRMED.

## Coverage NOT provided (stated deviations)

- ~150 files / ~21k lines of non-federation v0.8.0 diff not audited line-by-line
  (notably `protocol/wire.rs`, `server/headless.rs` 1277-line diff, `server/client_transport.rs`).
  That is a full-repo STRIDE pass, a separate scope.
- Vendored libghostty-vt clipboard-write extraction that F4 now depends on — not audited.
- No build, no test run (excluded by constraints).

## Unresolved questions

1. Does the vendored libghostty-vt clipboard path (now load-bearing per F4) warrant its own audit,
   or is upstream's use of it sufficient assurance for this fork?
2. Should the non-federation v0.8.0 diff get a separate STRIDE pass before release, or is riding
   upstream's own review acceptable for a fork?
