# Discussion draft — Remote Workspace Federation

> Post yourself under your own account, Discussions → Ideas, on `ogulcancelik/herdr`.
> Category "Ideas" form has 3 fields (idea/problem, requested change, why you want this)
> instead of one free body — mapped below.

---

## Title

[idea] Remote workspace federation — mount a headless remote herdr as a local workspace over SSH

## idea / problem

*what do you want to do, or what feels awkward today?*

I run coding agents across a few always-on boxes plus my laptop. `--remote ssh` today gives
a full-screen attach to one pane at a time, so watching several remote hosts means separate
SSH sessions or separate herdr windows per host — no unified view.

## requested change

*what would you like herdr to support?*

Mount a headless remote herdr session as a *new workspace* inside a local herdr, so local
and remote workspaces sit in one sidebar with native agent-status detection and
cold-resume. This federates a whole remote workspace (multiple panes/tabs, agent detection,
resize, clipboard) alongside local ones, via a dedicated versioned protocol over the
existing SSH bridge to a new remote `federation-serve` subcommand — version/capability
mismatch falls back to today's attach behavior. Raw PTY byte channels (not the
rendered-ANSI attach path), fenced per mount so a remote restart can't leak stale traffic
in. Remote agent status relays into the existing detection UI with an origin badge. Trust
model: I control both binaries, SSH is the auth boundary, ingestion is still
sanitized/bounded.

Scope right now: single remote mount, no warm handoff (cold-resume only), no kitty-graphics
over federation, no local-echo prediction.

## why you want this

*how would this help you?*

One herdr window shows all my hosts instead of juggling separate SSH sessions or windows
per box. I've been running this daily against real remote boxes, fully opt-in, existing
`--remote ssh` path untouched.

Does this fit where herdr is headed? If there's interest I'd like to land it in
scope-sized pieces (protocol, remote server, client, UI) rather than one huge PR, once
there's an accepted issue to attach the work to.
