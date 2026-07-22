# Orca Update & Relay (Serve) Setup — VM100 / VM105

Date: 2026-07-16 (revised 17:05 after live execution on VM100 + VM105) | Branch: feat/remote-workspace-federation

> **Revised.** The original draft of this report was written from assumption, not evidence, and was wrong on 4 points
> (fabricated download URL, wrong root cause, missed reboot gap, no-op `--port`). It has been rewritten against
> live, verified runs on BOTH VMs. Outcome + evidence: `orca-update-relay-enable-260716-1650-vm100-outcome-report.md`
> (VM100); VM105 outcome folded into this report (§ below) since its run matched the corrected runbook exactly.

## Current State (verified 2026-07-16 17:00 — both VMs done)

| | VM100 (`appn-ltu-vm-100`) | VM105 (`appn-ltu-vm-105`) |
|---|---|---|
| Address | `131.172.248.161` (x86_64, host `gpu-ml`) | `131.172.248.163` (x86_64, host `bio-1-ubuntu`) |
| Orca version | ✅ **1.4.143** (latest) | ✅ **1.4.143** (latest) |
| Orca CLI (`~/.local/bin/orca`) | ✅ installed (copy of `orca-ide` shim) | ✅ present |
| Orca-IDE shim | ✅ `~/.local/bin/orca-ide` | ✅ present |
| `orca-serve` | ✅ active + enabled | ✅ active + enabled (was already healthy pre-update — no lock-squat here) |
| `orca-xvfb` (`:99`) | ✅ active + enabled | N/A — **no such unit on VM105**; its `orca-serve` has no `DISPLAY=` env and doesn't need Xvfb |
| Saved environment | `ws://131.172.248.161:45511` | `ws://131.172.248.163:33155` |
| GNOME Orca `/usr/bin/orca` | screen reader — **ignore** | same |
| `gh` CLI on host | ✅ authed | ❌ not installed — see § VM105 below |

## Key Facts (evidence-based)

- **No `orca upgrade` command.** Orca has no CLI self-update. Update = replace the AppImage manually.
- **Release feed is `stablyai/orca`** — NOT `orca-sh/orca` (that repo does not exist; returns 404).
  Source of truth: local `Orca.app/Contents/Resources/app-update.yml` → `owner: stablyai, repo: orca, provider: github`.
  Requires `gh` auth. VM100 has it (authed as `vietairs`); VM105 does not — download+verify elsewhere and `scp` over
  (see § VM105 below) rather than installing `gh` just for one download.
- **"Relay" = `orca serve`.** No `orca relay` subcommand exists.
- **`--port` is a no-op.** `orca serve --port 6768` does NOT produce a 6768 listener; ws-transport binds
  `0.0.0.0:45511` regardless. Clients connect to **45511** on VM100. Do not trust `--port` in the unit file.
- **Pairing token is persisted**, not regenerated per restart — an existing pairing code survives updates and
  restarts (confirmed live). You do NOT need to re-pair after an update.
- **Pairing banner reaching the journal is inconsistent.** On VM100 it never appeared (Node stdout buffering when
  not a TTY). On VM105 it appeared directly: `journalctl -u orca-serve` showed `Pairing URL: orca://pair?code=...`
  right after `Orca server ready: ws://0.0.0.0:33155`. Don't rely on either behavior — check `orca environment list`
  first; it already holds a working code and existing codes survive updates/restarts (confirmed on both VMs).

## ⚠️ The Single-Instance Trap (this is what actually breaks serve)

Orca is Electron and enforces **one instance per userData profile** (`~/.config/orca`). If ANY other Orca process
owns that profile, the systemd unit starts and immediately exits:

```
[single-instance] Another Orca instance is already running for this userData profile
```

The unit reports `enabled` but stays `inactive` — which reads like "needs enabling" but is not. **A manual
`orca serve` run on a host where the systemd unit owns the profile will silently break the service**, and can
orphan (PPID=1) while holding the lock *without actually serving* (`runtimeState: stale_bootstrap`, port dead).

**Rule: never run `orca serve` by hand on VM100/VM105. Use `systemctl`.**

Diagnose before assuming the unit is at fault:

```bash
systemctl is-active orca-serve; systemctl is-enabled orca-serve
pgrep -af "orca-linux.AppImage|\.mount_orca-" | grep -v grep    # any squatter?
~/.local/bin/orca status                                        # runtimeReachable?
```

## Workflow

### 1. Update Orca (manual AppImage replace, checksum-verified)

Run on the VM (it has `gh` auth). Never install an unverified 200MB binary.

```bash
LATEST=$(gh release view --repo stablyai/orca --json tagName -q .tagName)   # e.g. v1.4.143

# Expected sha512 + size for the x86_64 asset:
gh release download "$LATEST" --repo stablyai/orca --pattern "latest-linux.yml" --output - | head -6

# Download (use orca-linux-arm64.AppImage on aarch64):
gh release download "$LATEST" --repo stablyai/orca --pattern "orca-linux.AppImage" \
  --output /tmp/orca-linux.AppImage.new --clobber

# VERIFY before installing — must match latest-linux.yml exactly:
openssl dgst -sha512 -binary /tmp/orca-linux.AppImage.new | openssl base64 -A
stat -c%s /tmp/orca-linux.AppImage.new
```

Only if both match:

```bash
sudo systemctl stop orca-serve
sudo cp -a /opt/orca/orca-linux.AppImage /opt/orca/orca-linux.AppImage.bak-<oldver>   # rollback point
sudo install -m 0755 -o root -g root /tmp/orca-linux.AppImage.new /opt/orca/orca-linux.AppImage
```

Verify version (`--version` prints CLI help, not a version — use the package.json instead):

```bash
ELECTRON_RUN_AS_NODE=1 /opt/orca/orca-linux.AppImage \
  -e 'console.log(require(process.env.APPDIR+"/resources/app.asar/package.json").version)'
```

Install the CLI if missing (the `orca-ide` shim is a generic dispatcher — copying it is valid):

```bash
cp ~/.local/bin/orca-ide ~/.local/bin/orca && chmod +x ~/.local/bin/orca
```

### 2. Clear any squatter, then start (VM100)

```bash
sudo systemctl stop orca-serve
pkill -TERM -f "orca-linux.AppImage serve"; sleep 3
pkill -KILL -f "\.mount_orca-"; sleep 2
rm -f ~/.config/orca/daemon/daemon-v21.{sock,pid}
rm -f ~/.config/orca/{SingletonLock,SingletonSocket,SingletonCookie}

sudo systemctl enable --now orca-xvfb     # MUST be enabled — serve depends on it
sudo systemctl daemon-reload
sudo systemctl enable --now orca-serve
```

Do **not** kill `~/.orca-remote/relay-*` processes — those are separate remote-relay sessions, not `orca serve`.

VM100 unit (`/etc/systemd/system/orca-serve.service`), as deployed:

```ini
[Unit]
Description=Orca runtime server
After=network-online.target orca-xvfb.service
Wants=network-online.target orca-xvfb.service

[Service]
Type=simple
User=hvnguyen
WorkingDirectory=/home/hvnguyen
Environment=DISPLAY=:99
Environment=LIBGL_ALWAYS_SOFTWARE=1
ExecStart=/opt/orca/orca-linux.AppImage serve --port 6768 --no-sandbox --disable-gpu --disable-gpu-sandbox --pairing-address 131.172.248.161
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`--port 6768` is inert (see Key Facts) — harmless, but do not expect a 6768 listener.

### 3. VM105 — done (2026-07-16 17:00)

VM105 differed from VM100 in three ways that changed the procedure:

- **`orca-serve` was already healthy** (active, enabled, genuinely serving on :33155) — no single-instance lock-squat
  outage here. This was a clean version-only update, not an outage recovery.
- **No `orca-xvfb` unit exists on VM105 at all.** Its `orca-serve.service` has no `Environment=DISPLAY=...` line and
  doesn't need one. Do not invent an Xvfb unit for VM105 — nothing is broken there.
- **No `gh` CLI on the host**, and `stablyai/orca` is a private repo, so a direct `curl`/`wget` on VM105 won't
  authenticate. Downloaded + sha512-verified on a machine that already has `gh` authed (in this case, the local Mac
  client), then `scp`'d the verified binary to VM105 and **re-verified the sha512 post-transfer** before installing
  (catches transfer corruption, not just a bad source). Avoided installing `gh` on VM105 just for one download
  (YAGNI) — reuse the already-verified artifact instead.

Procedure used:

```bash
# On a gh-authed machine:
LATEST=$(gh release view --repo stablyai/orca --json tagName -q .tagName)
gh release download "$LATEST" --repo stablyai/orca --pattern "orca-linux.AppImage" --output ./orca-linux.AppImage.new
gh release download "$LATEST" --repo stablyai/orca --pattern "latest-linux.yml" --output ./latest-linux.yml
# verify sha512+size against latest-linux.yml (see step 1) before transferring

scp ./orca-linux.AppImage.new appn-ltu-vm-105:/tmp/orca-linux.AppImage.new

# On VM105 — re-verify post-transfer, then install:
ssh appn-ltu-vm-105
openssl dgst -sha512 -binary /tmp/orca-linux.AppImage.new | openssl base64 -A   # must match latest-linux.yml again
sudo systemctl stop orca-serve
sudo cp -a /opt/orca/orca-linux.AppImage /opt/orca/orca-linux.AppImage.bak-1.4.141
sudo install -m 0755 -o orca -g orca /tmp/orca-linux.AppImage.new /opt/orca/orca-linux.AppImage   # note: owner orca:orca, not root:root like VM100
rm -f ~/.config/orca/daemon/daemon-v21.{sock,pid} ~/.config/orca/{SingletonLock,SingletonSocket,SingletonCookie}
sudo systemctl daemon-reload
sudo systemctl start orca-serve
```

Result: 1.4.141 → **1.4.143**, `runtimeState: ready`, `runtimeReachable: true`, 0 restarts post-update.
Verified end-to-end from the Mac client: `runtimeId` from `orca status --environment APPN-LTU-VM-105` matched the
VM105-local runtimeId, and `orca repo list --environment APPN-LTU-VM-105` returned real registered repos (`herdr`,
`APPNltu_smartForm`) — stronger proof than VM100's empty repo list. Existing pairing code kept working; no re-pair
needed.

### 4. Verify (do not assume — prove it)

```bash
# On the VM:
systemctl is-active orca-serve; systemctl is-enabled orca-serve
~/.local/bin/orca status            # want: runtimeState: ready, runtimeReachable: true
ss -tlnp | grep -i orca             # expect 0.0.0.0:45511

# From the client (Mac) — the real test:
nc -z -w 8 131.172.248.161 45511
orca environment list
orca status --environment APPN-LTU-VM-100
```

**Strongest check:** the `runtimeId` returned remotely must equal the `runtimeId` seen locally on the VM. That
proves the client lands on the freshly-updated process rather than a stale one.

### 5. Connect from local machine

Existing environments already work after an update (token persists) — re-pair only when adding a new host:

```bash
orca environment list
orca environment add --name vm105 --pairing-code <code>   # only if not already registered
```

## Rollback

```bash
sudo systemctl stop orca-serve
sudo cp -a /opt/orca/orca-linux.AppImage.bak-1.4.141 /opt/orca/orca-linux.AppImage
sudo systemctl start orca-serve
```

## Known Non-Blocking Noise

- `[claude-rate-limits] ... status: 401 Invalid authentication credentials` — VM100 Claude creds stale.
  Does **not** affect serve/relay; only agent runs on VM100.
- `ERROR:dbus/bus.cc: Failed to connect to the bus` — expected under headless Xvfb, harmless.
- `orca status` over a remote env shows `appRunning: false / pid: none` — normal; `runtimeReachable` is what matters.

## Service Dependencies

1. `orca-xvfb.service` → provides `DISPLAY=:99` (must start first; **must be `enabled`** or serve dies after reboot)
2. `orca-serve.service` → depends on Xvfb + network

Both use `Restart=on-failure`.

## Unresolved Questions

1. **Stale `--port 6768`** in both unit files — remove, or set to the real ws port for clarity? (VM100: 45511,
   VM105: 33155 — each host keeps whatever port it first came up on; `--port` never actually selects it.)
2. **VM100 Claude 401** — `[claude-rate-limits] ... Invalid authentication credentials` seen on VM100 only, not
   VM105. In scope to fix?
3. **Is the ws port stable across versions?** Saved envs hardcode it (45511 for VM100, 33155 for VM105); if a
   future release changes how it's chosen, every saved environment breaks. Not yet confirmed whether Orca pins it
   or derives it from something host-specific (hostname hash, PID, etc. — both VMs got different ports despite an
   identical `--port 6768` request).
