# Orca Update + Relay Enable — VM100 — Outcome

Date: 2026-07-16 | Host: `appn-ltu-vm-100` (131.172.248.161, gpu-ml, x86_64)
Supersedes plan: `orca-update-and-relay-setup-260716-1629-vm100-vm105-report.md` (4 factual errors — see below)

## Result: DONE

| Item | Before | After |
|---|---|---|
| Orca version | 1.4.141 | **1.4.143** (latest) |
| `orca-serve` | enabled, **inactive** (crash-looping) | **active + enabled** |
| `orca-xvfb` | active, **disabled** (no reboot survival) | **active + enabled** |
| Runtime | `stale_bootstrap`, unreachable | `ready`, **reachable** |
| `~/.local/bin/orca` | missing | installed |
| Remote reach from Mac | — | **proven** (`ws://131.172.248.161:45511`) |

## Root Cause (plan missed this)

`orca-serve` was already `enabled` — it exited on every start:

> `[single-instance] Another Orca instance is already running for this userData profile`

An **orphaned** `orca serve` tree (PID 338981/338982, PPID=1, from an interactive debug one-liner at ~02:43) squatted the Electron single-instance lock on `~/.config/orca` while **not** serving: port 6768 dead, `runtimeState: stale_bootstrap`, `runtimeReachable: false`. It bound only 45511.

Killing the orphan + clearing the stale daemon socket released the lock; the unit then started clean.

## Verification (evidence, not assumption)

- **Checksum**: downloaded asset sha512 == `latest-linux.yml` expected value; size 200617774 exact.
- **Version**: `package.json` inside AppImage -> `1.4.143`.
- **End-to-end**: `orca status --environment APPN-LTU-VM-100` from Mac returns
  `runtimeId: 2ab16a0c-1843-4d44-9399-110fc7dc27a0` — **identical** to the runtimeId observed locally on VM100 => remote traffic lands on the new 1.4.143 process.
- **Transport**: `nc 131.172.248.161 45511` succeeds from Mac.
- **Command exec**: `orca repo list --environment APPN-LTU-VM-100` -> "No repos found" (valid response; VM100 has no repos registered).
- **Preserved**: 5 `.orca-remote/relay-*` processes left untouched (separate remote-relay sessions, not `orca serve`).

## Plan Corrections

1. **Download URL fabricated.** `github.com/orca-sh/orca` -> **404, repo does not exist**. Real feed is `stablyai/orca` (source: local `Orca.app/Contents/Resources/app-update.yml`). Correct asset: `orca-linux.AppImage` (x86_64), verified against `latest-linux.yml`.
2. **"Enable orca-serve" was not the fix** — already enabled; the profile lock was the blocker.
3. **`orca-xvfb` was `disabled`** — active only until reboot, despite `orca-serve` depending on it. Plan didn't catch this. Now enabled.
4. **`--port 6768` is a no-op.** ws-transport binds `0.0.0.0:45511` regardless. Saved env already targets 45511 — which is why the existing pairing code still works (token is persisted, not regenerated per restart; confirmed by user).

## Rollback

```bash
sudo systemctl stop orca-serve
sudo cp -a /opt/orca/orca-linux.AppImage.bak-1.4.141 /opt/orca/orca-linux.AppImage
sudo systemctl start orca-serve
```

## Known Non-Blocking Noise

- `[claude-rate-limits] ... status: 401 Invalid authentication credentials` — Claude creds on VM100 stale/expired. Does not affect serve/relay. Fix separately if agent runs on VM100 need Claude.
- `ERROR:dbus/bus.cc` spam — expected on headless Xvfb, harmless.

## Unresolved Questions

1. **VM105 not touched** — task scoped to VM100 only. VM105 is registered at `ws://131.172.248.163:33155` (plan's "IP TBD" answered); its version/serve state unverified. Want the same update+enable pass there?
2. **Stale `--port 6768`** left in the unit file — misleading but harmless. Remove it, or set it to the real 45511 for clarity?
3. **Orphan origin** — the debug one-liner that squatted the lock came from an earlier interactive session. If that pattern repeats it will re-break the unit. Worth avoiding manual `orca serve` runs on a host where the systemd unit owns the profile.
4. **Claude 401 on VM100** — fix now or out of scope?
