# Live two-host validation — federation close-forwarding

## First pass — `def4c90d` (2026-08-12)

Client: this mac (worktree release build of `def4c90d`).
Host: `appn-ltu-vm-105` (same commit, built from `git archive HEAD`).
Isolation: dedicated `--session fedtest` / `fedold` / `fedmix` servers on both
ends. The live `default`-session servers on both VMs were never restarted, so
their running panes (incl. Claude Code on vm-100) were untouched.

### T1 — workspace close forwards, closes EXACTLY one. PASS

Host baseline `w1`(3 tabs) `w2` `w3`; all three mirrored on the client with
correct `tab_count` (3/1/1 — so tab identity survives resync, not 1 tab with
3 splits).

`workspace.close_remote` on `r:appn-ltu-vm-105#fedtest:w2` returned
`remote_close_pending`, then:

- client: `w1`(3) `w3`(1) — mirror gone
- host:   `w1`(3) `w3`(1) — real workspace gone

Siblings survive on BOTH sides. This is the exact regression class the earlier
live pass caught (closing one mirror destroyed all four); it is closed.

### T2 — non-last tab close forwards. PASS

`tab.close_remote` on `...w1:t2` (of t1/t2/t3): client tabs `t1`, `t3`; host
tabs `w1:t1`, `w1:t3`. Exactly the named tab, on both sides.

### T3/T4 — mixed-version behavior. VERIFIED, and the prior note was WRONG

Both directions were run for real:

- OLD client (mac `~/.local/bin/herdr`, Aug 8 = master) -> NEW host: 0 mirrors.
- NEW client -> OLD host (vm-105 binary temporarily reverted, then restored):
  0 mirrors.

Both fail identically and loudly:

```
federation mount failed target=appn-ltu-vm-105
reason=remote herdr server on appn-ltu-vm-105 is running v0.8.0;
       run from an interactive terminal to approve stopping it for the update
```

#### Correction to the record

The pipeline record and memory note said deploy-together is "the recommendation,
not a correctness requirement", reasoning about the send-side
`Capability::WORKSPACE_TAB_CLOSE` gate returning `CapabilityNotAgreed`.

That reasoning is correct **only for the close-forwarding commit in isolation**,
i.e. when both ends already speak federation protocol 6. It does not describe
this BRANCH, because `FEDERATION_PROTOCOL_VERSION` is **5 on master and 6 on the
branch** — bumped by the earlier multi-tab-workspace phases, not by `def4c90d`.
The handshake hard-rejects a mismatch before capability intersection is ever
reached, so the capability gate is never the operative mechanism across a
version boundary.

**Deploy-together is therefore REQUIRED for this branch, not recommended.** The
failure mode is a refused mount with a clear message — not a hang and not silent
divergence — so it is safe, just not optional.

---

## Second pass — post-review build `5bbb1972` (2026-08-12)

Re-run after the review fixes landed. Both ends on `5bbb1972`, isolated
`--session fedtest` servers, `default` servers untouched on every host.

### Deploy state going in

| host | binary | default server |
|---|---|---|
| mac | `5bbb1972` | OLD (protocol 19) — **hosts the Claude Code session**, deliberately not restarted |
| vm-100 | `5bbb1972` | `5bbb1972`, protocol 20 (restarted) |
| vm-105 | `5bbb1972` | `5bbb1972`, protocol 20 (restarted) |

### Results

Mount mac -> vm-105: 3 mirrors, `tab_count` 3/1/1 — tab identity survives resync.

| test | result |
|---|---|
| `workspace.close_remote` w2 | **`"result":{"type":"workspace_close_requested",...}`** — SUCCESS envelope, no longer `encode_error`. Client `w1(3) w3(1)`, host `w1(3) w3(1)`: exactly one closed, both sides |
| `tab.close_remote` w1:t2 | `"result":{"type":"tab_close_requested",...}`. Client and host both `t1, t3` |
| CLI `herdr workspace close-remote` | **`EXIT=0`** — was exit 1 on every successful call before the fix |
| final state | client `w1`, host `w1` — every close forwarded exactly, nothing orphaned |

The `EXIT=0` line is the point of review finding D: the verbs always succeeded
but reported success through an error envelope, so every scripted caller saw a
failure. Confirmed fixed end-to-end, not just in the envelope shape.

### CI

Run 31558893010 on `5bbb1972`: **success on all five jobs**, including
`check (windows-latest)` and `Windows ConPTY package`. The Windows break was a
real bug — `raise_remote_close_failed_toast` gated `cfg(unix)` with two callers
on the platform-neutral TUI close path — not inherited fork breakage.

### Teardown

All `fedtest` servers killed and session dirs removed on both ends. Surviving:
one `default` server + TUI client per host. VM snapshot dirs `~/src/herdr-w1`
and `~/src/herdr-w2` removed from vm-105.

## Owed follow-up (operational, not a code defect)

The mac's `default` server still runs the OLD image. It hosts the active Claude
Code session (`HERDR_ENV=1`, `HERDR_PANE_ID`), so restarting it ends that
session — the user's call, deliberately not done here. Until it restarts, real
federation from the mac's default session hits the version refusal.

## Ops notes worth reusing

- `workspace.mount_remote` takes `targets` (array), not `target`.
- `--version` is `0.8.0` on every build; use `herdr status server` and read
  `protocol:` / `compatible:` to tell which image a server is actually running.
- On macOS `pgrep -x herdr` can return empty while the server is running; use
  `ps aux | grep '[h]erdr'` and `lsof` on the socket.
