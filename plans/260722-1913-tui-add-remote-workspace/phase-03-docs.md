# Phase 03 — Docs (unreleased)

Status: pending · Depends on: 01, 02 · Owns:
`docs/next/website/src/content/docs/persistence-remote.mdx`,
`docs/next/website/src/content/docs/keyboard.mdx`,
`docs/next/CHANGELOG.md`

Do **not** touch `website/src/content/docs/**` (stable), root `README.md`, root `CHANGELOG.md`,
or `website/latest.json`.

## Steps

1. Read `docs/next/website/src/content/docs/persistence-remote.mdx` first; find the section that
   documents `herdr --remote <target>`. Add a short subsection: mounting a remote workspace from
   inside a running session via the global menu → `mount remote` → type one or more
   `user@host[:port]` targets separated by spaces → enter.
2. State the constraints that are real, in one short list:
   - `localhost` is not accepted (use a local workspace).
   - Targets must not start with `-`.
   - Key-based SSH auth is required: the dial does not run in batch mode and an interactive
     password/passphrase prompt would be written to the terminal herdr is drawing on
     (`src/remote/unix.rs:422` inherits stderr). No prompt is surfaced in the dialog.
   - The dialog closes as soon as the request is accepted; the mount itself completes in the
     background (~25s dial budget, plus an unbounded remote-preparation step). There is no cancel.
     A successful mount appears as new workspaces; a failure surfaces through the configured
     notification channel.
3. Troubleshooting note (same file or `troubleshooting.mdx` if that is where remote issues live —
   read it first): if `toast.delivery` is `terminal` or `system` and local terminal notifications
   are disabled, a failed mount is only written to the log; check the herdr server log for
   `federation mount failed`.
4. `keyboard.mdx`: only if the file documents the global menu entries — add `mount remote` to
   that list. Do not invent a keybinding; this feature ships with no dedicated binding.
5. `docs/next/CHANGELOG.md`: one lowercase line under the unreleased section.

## Acceptance

- Docs describe only shipped behavior from phases 01/02.
- No stable docs, root README, or root CHANGELOG modified.
- No claim of a keybinding, cancel affordance, or progress indicator that does not exist.

## Rollback

Revert the docs commit; no code impact.
