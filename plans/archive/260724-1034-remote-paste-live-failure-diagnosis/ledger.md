Task: diagnose why remote-workspace clipboard-image paste wasn't firing.

Root cause (proven): H2 launch-route gap (--remote-workspace never set the
remote-client env, disabling the path-bridge mechanism) + H1 intercept only
matched raw ctrl+v KeyEvent, ignoring the bracketed-paste path-text case.
Live evidence round also found a stale running server masking the fix
during initial retest.

Shipped: 3 gaps fixed (G1 bracketed-paste image-path detection via shared
image_path module; G2 --remote-workspace remote-client marker, deliberately
left as single-owner; G3 furl-only clipboard fallback — the LIVE-CONFIRMED
primary cause, `read_clipboard_image` in platform/macos.rs had no furl
fallback, PNGf-only).

PR #6 — https://github.com/vietairs/herdr/pull/6, merged 2026-07-24 as
`46c4dd1e`. Code review: APPROVE_WITH_NITS (1 medium symlink-swap TOCTOU
sent back and fixed).

Force-archived despite UNKNOWN classification — only unchecked step was
ship-gate, and `gh pr view 6` confirms the PR merged.

Archived-at-SHA: 4ccf09a812e5227d95b6e7a8a511d3be7a8cc565
