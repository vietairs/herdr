Task: merge upstream v0.8.0 into fork master.

Shipped: PR #10 — https://github.com/vietairs/herdr/pull/10, merged
2026-08-06 as `584c5c97`. 3370 tests green at merge time.

Force-archived despite UNKNOWN classification and this session's active
IMPL-NOTES standing-rule pointer — the pointer is stale: PR #10 is merged,
and a later archived dir (260807-0041-federation-ui-labels) already
reconciled the two verification claims this merge originally got wrong
(fork-original symbol-loss count, CLAUDE.md/AGENTS.md restoration).

Carried-forward review items from this merge (now historical, not open
work): `Osc52Forwarder` removed in favor of upstream's
`OscStreamCollector`; `send_bytes_after` no-op for remote panes flagged as
an inferred gap; a per-keystroke `key.clone()` workaround for a latent move
bug; FEDERATION_PROTOCOL_VERSION 4->5 needing a live merged<->v4 handshake
proof (deferred, non-blocking); Windows cfg arms not covered by the mac
build (pre-existing, tracked separately).

Post-merge steps (`git replace -d ...`, worktree removal) were completed as
part of finishing PR #10/#11 — see the already-archived
260807-0041-federation-ui-labels ledger.

Archived-at-SHA: 4ccf09a812e5227d95b6e7a8a511d3be7a8cc565
