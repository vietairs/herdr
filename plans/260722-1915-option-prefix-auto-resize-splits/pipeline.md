# Pipeline — option+b prefix (local) + auto-resize splits in pane context menu

Task: (A) change herdr prefix hotkey ctrl+b -> option+b in LOCAL config only;
(B) add auto-resize of split panes to the right-click pane context menu, as BOTH a
one-shot manual "balance splits" action AND a persistent auto-resize toggle.

Task source: free text (user request, 2026-07-22)

Created: 2026-07-22 19:15 Australia/Melbourne

## ROUTE CARD (confirmed by user, verbatim)

```
Risk:        medium — pane geometry is a server-owned runtime fact per the repo's
                      runtime/client guardrail; a PERSISTENT toggle needs server state +
                      protocol exposure, and this fork mirrors pane changes to mounted
                      remote clients (commit b5cb8ce8). No auto-resize code exists to
                      pattern-match — net-new layout math. Config edit itself is low.
Familiarity: medium — prior fork work on pane mirroring/federation; zero prior art for
                      split balancing.
Scope:       feature — 2 independent units:
                      (A) local config one-liner + empirical Option-key verification
                      (B) balance action + persistent toggle across state/menu/dispatch/protocol
Payoff:      medium — user is the direct consumer; daily-driver ergonomics on a tool run
                      constantly. Evidence: this request.

Route:
  0. Verify option+b reaches herdr           — main-loop (irreducible: needs user keypress)
  1. /hvn-worktree                           — agent:git-manager
  2. /hvn:blindspot                          — agent:hvn-scout x3 parallel
  3. /hvn-predict                            — agent:hvn-root-causer
  4. /hvn-plan --tdd                         — agent:planner
  5. plan validate + direction confirm       — main-loop (irreducible: confirm)
  6. /hvn:impl-notes init                    — agent:hvn-scout
  7. /hvn-cook --auto --parallel             — agent:fullstack-developer (per-unit fan-out)
  8. /hvn:impl-notes review                  — main-loop (irreducible: distillation)
  9. /hvn-code-review  ||  /hvn-security-scan — agent:code-reviewer || general-purpose
 10. /hvn:ship-gate                          — main-loop (irreducible: attestation)

Skips: /hvn-brainstorm — scope is concrete, no design space to debate (R5 row)
       red-team — overkill at medium risk (R5 row)
       codex adversarial gates — R7-only

Teardown: after PR merge -> git pull --ff-only on base -> remove local worktree ->
          /hvn:plan-gc archive this dir. Pushed remote branch is KEPT (never deleted).
```

## User decisions (from confirm round)

1. Prefix change target: **local ~/.config/herdr/config.toml only** — NOT the repo default.
   Repo default `ctrl+b` at src/config/model.rs:923 stays untouched.
2. Auto-resize semantics: **both** — one-shot manual balance action AND persistent toggle.
3. macOS Option key: **verify first, then decide** — Stage 0 gates Unit A.

## Autonomy mode

default (interactive) — all gates stop and ask.

## Scouted evidence (pre-classification)

- Prefix default: `src/config/model.rs:923`; mirrored at `src/main.rs:162-170` (config
  template comment), `src/cli.rs:944` (CLI help), `src/ui/onboarding.rs:15`
  (ONBOARDING_PREFIX_LABEL), `src/app/mod.rs:2099` (comment), `src/config/io.rs:719` +
  `src/config/keybinds.rs:1908` (tests). Unit A touches NONE of these — local config only.
- Modifier parsing already accepts `alt` | `option` | `meta` -> KeyModifiers::ALT at
  `src/config/keybinds.rs:1160`. No parser work needed.
- Pane context menu items: `src/app/state.rs:1266-1313`, 4 variants keyed on
  `has_manual_label` / `source_pane_id`. Current items: Rename pane / Clear pane name /
  Swap with focused pane / Split right / Split down / Zoom / Close pane.
- Menu action dispatch: `src/app/input/modal.rs:693+` (`apply_context_menu_action`).
- Menu render: `src/ui/menus.rs:286` (`render_context_menu`).
- NO existing auto-resize / equalize / balance / even-split code anywhere in src/.
  Unit B is net-new layout logic.
- Terminal: Ghostty 1.3.2-HEAD-+bb30526, `macos-option-as-alt` UNSET (no
  ~/.config/ghostty/config present). Terminal.app `useOptionAsMetaKey = 0` (not the
  terminal in use). Effective Option behavior undetermined -> Stage 0 verifies empirically.
