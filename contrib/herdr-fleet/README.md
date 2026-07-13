# herdr-fleet

One local sidebar for agents running across a **local + multiple remote** herdr servers.

## Why

herdr has no built-in multi-server federation. Each server — your local one and
every box you reach with `herdr --remote <ssh>` — is a standalone instance (like
tmux). `herdr --remote` is a *full-screen attach* to one server at a time, so
there is no native way to see local + remote agents in a single sidebar.

`herdr-fleet` works around that with two moves:

1. **`list`** — runs `herdr agent list` on every configured host (`ssh <host> herdr agent list`
   for remotes, direct for local) and merges the JSON into one table.
2. **`pull`** — opens a **local** herdr pane whose command is
   `ssh -t <host> herdr agent attach <target> --takeover`. Because
   `herdr agent start <name> -- <argv>` labels that pane as an agent named
   `<host>:<target>`, the remote agent **appears in your local sidebar** and is
   fully interactive (keystrokes forward over the attach, output streams back).

## Fidelity caveat

A pulled pane is a *labelled live attach*, not a native local agent:

- ✅ Shows in the local sidebar / `agent list`; fully interactive.
- ⚠️ Rich status (working/idle/blocked) and session-resume live on the **remote**
  server, not local. Locally the label comes from `agent start --name` (screen
  detection may refine it to `claude`/`codex`/… once the TUI renders).
- ⚠️ A local server restart won't natively re-attach it — re-run `pull`.

## Requirements

- `herdr` on PATH locally **and** on each remote host.
- `jq`. Optional: `fzf` (interactive target picker for `pull`).
- Passwordless SSH to each remote (key auth). Remotes are reached exactly as
  `ssh <alias> …`, so your `~/.ssh/config` aliases work.

## Install

```sh
# from this repo
ln -s "$PWD/contrib/herdr-fleet/herdr-fleet" ~/.local/bin/herdr-fleet   # or anywhere on PATH
```

## Hosts file

`~/.config/herdr-fleet/hosts` (override with `$HERDR_FLEET_HOSTS`), one ssh alias
per line. The literal word `local` (or `localhost`) means this machine's server
with no ssh hop. `#` starts a comment.

```
local
gpu-ml
build-box
```

## Usage

```sh
herdr-fleet list                 # aggregate agents across all configured hosts
herdr-fleet list gpu-ml          # just one host
herdr-fleet pull gpu-ml          # pick a remote agent (needs fzf), attach into a local pane
herdr-fleet pull gpu-ml w1:p2    # attach a specific remote target
herdr-fleet board                # pull EVERY agent from all hosts into one new workspace
herdr-fleet board gpu-ml build-box --label remotes
herdr-fleet hosts                # show configured hosts
```

Options for `pull` / `board`: `-w/--workspace <id>`, `--split right|down`,
`--focus`, `--label <text>` (board workspace name), `-n/--dry-run`.

`board` is the closest thing to the original goal: it creates one workspace and
drops a live pane for every agent on every machine, giving you a single sidebar
that lists them all.

## How targets are resolved

A `<target>` is what `herdr agent <cmd> <target>` accepts on the host it runs on:
a terminal id, a legacy pane id (`w1:p2`), or a unique agent name/label. `list`
prints the pane id in the `TARGET` column — copy that into `pull`.

## Environment guard

The remote command is wrapped in `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH`
so a local server's socket env can never misdirect the remote CLI if your SSH
config forwards env vars.

## Status

Mechanism validated end-to-end against the local transport (spawn → sidebar
label → bidirectional live attach → cleanup). The `ssh -t` hop is standard and
the only layer not exercised in that local test; validate it once against a real
remote with `herdr-fleet pull <realhost> <target>`.
