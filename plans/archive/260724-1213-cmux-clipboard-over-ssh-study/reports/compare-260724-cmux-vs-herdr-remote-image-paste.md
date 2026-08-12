# Feature Comparison: clipboard image → remote agent session

## Source: manaflow-ai/cmux — Local: herdr (fork, federation remote-paste feature + fix worktree)

Evidence base: reports/xia-recon-260724-cmux-clipboard-image-remote-mechanism.md (file:line traces from cmux tree) + plans/260724-1034-remote-paste-live-failure-diagnosis/ reports.

## Head-to-Head

| Aspect | cmux | herdr (feature + fix worktree) | Verdict |
| --- | --- | --- | --- |
| Capture | GUI clipboard API (NSPasteboard via Electron RPC `terminal.paste_image`), base64 | Server-side osascript: PNGf + furl fallback (fix worktree); bracketed-paste temp-path detection | herdr covers MORE clipboard shapes (Finder file copies) |
| Transport | base64 over RPC → local temp file → scp, 45s fixed timeout | Native federation channel (Channel::FileStaging, 24MiB frames), payload-proportional timeout | herdr more efficient (no base64 inflation), adaptive timeout |
| Remote landing | `/tmp/cmux-drop-{uuid}.{ext}` | Staging dir with 8 ordered validation guards, sweep + lifetime cleanup | herdr stricter |
| Agent ingestion | **File path injected into the pty as text — NOT [Image #N]** | Same: staged remote path pasted into pty | IDENTICAL approach — cmux does not achieve attachment-style ingestion either |
| Size cap | 10 MB | 16 MiB client precheck + server cap | comparable |
| Security | ext whitelist, traversal block | ext whitelist, traversal block, reject-don't-strip, TOCTOU-closed canonical read, capability gate | herdr stricter |

## Recommendation (arbiter-checked, no contradictions between recon and prior herdr reports)

1. ADOPT: nothing structural — cmux's mechanism (local temp → scp → paste path) is architecturally the same as herdr's shipped design B, and herdr's implementation is stricter on every compared axis. The interim `herdr-img2vm` script is literally the cmux pattern.
2. The user's desired `[Image #N]` rendering is NOT achieved by cmux — its agents also receive a path as text. Attachment-style ingestion would require the agent CLI reading a live clipboard on the remote host (headless → virtual-clipboard shim) or an agent-side attachment API; out of scope for both tools today.
3. herdr's remaining defect is not mechanism but the LIVE fall-through in the TUI paste path (tracked in plans/260724-1034-remote-paste-live-failure-diagnosis/, debug-traced binary deployed, awaiting one user paste for the trace).
4. Minor idea worth keeping in mind (not implemented): cmux supports multiple images as space-separated paths on one line; herdr's bracketed-paste detector currently matches a single path only. Logged as a possible follow-up, not a defect.

## Unresolved questions
- Whether any agent CLI (Claude Code included) exposes a supported non-clipboard attachment channel a terminal manager could target — would unlock true [Image #N]; needs a separate research task if wanted.
