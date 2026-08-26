# herdr


<p align="center">
  <img src="assets/logo.png" alt="herdr" width="100" />
</p>

<p align="center">
  <a href="https://herdr.dev">herdr.dev</a> · <a href="#install-this-fork">install</a> · <a href="https://herdr.dev/docs/quick-start/">quick start</a> · <a href="https://herdr.dev/docs/">docs</a>
</p>

<p align="center">
  English · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-666666?labelColor=333333" alt="Apache 2.0 license" /></a>
  <a href="https://github.com/herdrdev/herdr/releases"><img src="https://img.shields.io/github/downloads/herdrdev/herdr/total?labelColor=333333&color=666666" alt="total GitHub release downloads" /></a>
  <a href="https://github.com/herdrdev/herdr/stargazers"><img src="https://img.shields.io/github/stars/herdrdev/herdr?labelColor=333333&color=666666&logo=github" alt="GitHub stars" /></a>
  <a href="https://github.com/herdrdev/herdr/releases/latest"><img src="https://img.shields.io/github/v/release/herdrdev/herdr?label=release&labelColor=333333&color=666666" alt="latest stable release" /></a>
  <a href="https://formulae.brew.sh/formula/herdr"><img src="https://img.shields.io/homebrew/v/herdr?label=homebrew&labelColor=333333&color=666666" alt="Homebrew version" /></a>
  <a href="https://x.com/herdrdev"><img src="https://img.shields.io/badge/follow-%40herdrdev-000000?logo=x&logoColor=white" alt="follow @herdrdev on X" /></a>
</p>

---

https://github.com/user-attachments/assets/043ec09f-4bdd-41d5-aee0-8fda6b83e267

**the runtime your coding agents live on.**

- **always running** — herdr is a background server; the terminals live inside it. close the lid, drop the network, or restart the machine; agents keep working and sessions come back. reattach from any terminal, or over ssh.
- **never hunt for the stuck one** — every pane is marked working, blocked, or idle. when an agent stops and needs an answer, herdr says so.
- **agent-native** — agents drive herdr through the cli and socket api: they can spawn panes, prompt each other, and wait until another agent is genuinely blocked. [agent skill →](https://herdr.dev/docs/agent-skill/)
- **runs what you already run** — claude code, codex, cursor, opencode, grok and the rest. herdr doesn't wrap or replace them; it owns their terminals.
- **keyboard and mouse, both first-class** — tmux-style prefix keys *and* click, drag, split. pick per moment, not per tool.
- **plugins** — extend panes and workflows. [browse the marketplace →](https://herdr.dev/plugins/)
- **one rust binary, no electron** — runs in whatever terminal you already use.

---

> **this is a fork.** [`vietairs/herdr`](https://github.com/vietairs/herdr) tracks upstream
> [`herdrdev/herdr`](https://github.com/herdrdev/herdr) and adds remote workspace federation work
> that is not upstream yet. fork releases are tagged `v<upstream-version>-hvn.<n>` and ship their
> own binaries — see [install (this fork)](#install-this-fork). upstream's installer, homebrew
> formula and update channels always give you **upstream** herdr, not this fork.

## install (this fork)

fork releases publish four binaries: `herdr-linux-x86_64`, `herdr-linux-aarch64`,
`herdr-macos-x86_64`, `herdr-macos-aarch64`. no installer script, no homebrew, no windows build.

```bash
case "$(uname -s)" in Darwin) os=macos ;; *) os=linux ;; esac
case "$(uname -m)" in arm64|aarch64) arch=aarch64 ;; *) arch=x86_64 ;; esac
mkdir -p ~/.local/bin
curl -fsSL "https://github.com/vietairs/herdr/releases/latest/download/herdr-$os-$arch" -o ~/.local/bin/herdr
chmod +x ~/.local/bin/herdr
```

make sure `~/.local/bin` is on your `PATH`, then check what you got:

```bash
herdr --version
```

pick a specific release instead of the latest by swapping `latest/download` for
`download/<tag>` — [all fork releases](https://github.com/vietairs/herdr/releases).

`herdr update` and `herdr channel set …` point at upstream's update feed, so they will
**downgrade you off this fork**. update by re-running the download above.

### federation: same build on both ends

remote workspace federation speaks a fork-local protocol, and the version guard refuses a mount
between mismatched builds. install the **same fork release** on the local machine and every remote
you mount, and upgrade them together. the remote needs `herdr` on its `PATH` (or
`~/.local/bin`); to push a local build instead, set `HERDR_REMOTE_BINARY=path/to/herdr`.

### build from source

```bash
git clone https://github.com/vietairs/herdr.git
cd herdr
cargo build --release   # binary at target/release/herdr
```

rust comes from `rust-toolchain.toml` (1.96.1, installed automatically by rustup). the vendored
`libghostty-vt` needs **zig 0.15.2** specifically — if your `PATH` zig is a different version,
point the build at the right one with `ZIG=/path/to/zig-0.15.2/zig cargo build --release`.

## install (upstream herdr)

```bash
curl -fsSL https://herdr.dev/install.sh | sh
```

or `brew install herdr` · `mise use -g herdr` · windows: `powershell -ExecutionPolicy Bypass -c "irm https://herdr.dev/install.ps1 | iex"` · [endpoint-protected Windows](https://herdr.dev/docs/windows-beta/) · [binaries](https://github.com/herdrdev/herdr/releases)

then start it where the work lives:

```bash
herdr
```

run your agents, split panes, walk away. `ctrl+b q` detaches, `herdr` reattaches. [quick start →](https://herdr.dev/docs/quick-start/)

## remote workspace federation

mount a remote machine's herdr as a **new workspace in your local sidebar** — local and remote agents side by side, native status, cold-resume. the remote runs herdr headless as a federation server over the same ssh bridge; no extra ports, no daemon to babysit.

```bash
# mount a remote host as a federated workspace
herdr --remote ssh://you@server --remote-workspace
# equivalently, via env
HERDR_REMOTE_FEDERATION=1 herdr --remote server        # `server` = any ssh config host
```

repeat per host to pull several machines into one sidebar — each mounts under its own per-host group:

```bash
herdr --remote appn-ltu-vm-100 --remote-workspace
herdr --remote appn-ltu-vm-105 --remote-workspace
```

notes:

- **herdr must be installed on both ends** — the remote is driven via `herdr federation-serve`. put it on the remote `PATH` or `~/.local/bin`; to push a local build, set `HERDR_REMOTE_BINARY=path/to/herdr`.
- mount a **named** remote session with `--session <name>` (`herdr --remote server --remote-workspace --session agents`).
- `--remote-workspace` requires `--remote`. if the remote's herdr lacks federation (old binary / version skew), herdr **falls back to the classic full-screen `--remote` attach** with a notice — never a hard failure.
- plain thin-client attach (no sidebar merge, one host at a time) stays `herdr --remote <target>`. full details: [remote docs →](https://herdr.dev/docs/persistence-remote/).

## docs

everything lives at [herdr.dev/docs](https://herdr.dev/docs/): [quick start](https://herdr.dev/docs/quick-start/) · [concepts](https://herdr.dev/docs/concepts/) · [supported agents](https://herdr.dev/docs/agents/) · [keyboard](https://herdr.dev/docs/keyboard/) · [configuration](https://herdr.dev/docs/configuration/) · [session state](https://herdr.dev/docs/session-state/) · [remote](https://herdr.dev/docs/persistence-remote/) · [integrations](https://herdr.dev/docs/integrations/) · [plugins](https://herdr.dev/docs/plugins/) · [socket api](https://herdr.dev/docs/socket-api/)

## thanks

every past sponsor and backer is listed in [SPONSORS.md](./SPONSORS.md) — thank you 🐑

enterprise / partnership: hey@herdr.dev

## agent instructions

if you are an ai agent helping with this repository, read [`AGENTS.md`](./AGENTS.md) before making changes and read [`CONTRIBUTING.md`](./CONTRIBUTING.md) before opening issues or PRs.

## development

```bash
git clone https://github.com/herdrdev/herdr
cd herdr
cargo build --release

just test        # unit tests
just check       # formatting, tests, and maintenance checks
```

## license

Herdr is licensed under the [Apache License 2.0](LICENSE).
