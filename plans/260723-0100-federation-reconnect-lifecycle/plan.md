# Federation reconnect & lifecycle (P9.3, re-scoped)

Status: NOT STARTED — scope only, no implementation, no route card yet.

Carved out of `plans/260713-1217-herdr-remote-workspace-federation` (closed 2026-07-23). That
epic's P9.3 was written before six later pipelines shipped, and those ate part of its original
scope. This file records only what survives, so the work can be re-planned against today's code
rather than resumed against a stale spec.

## Already delivered elsewhere — do NOT re-plan

- Post-mount pane mirroring — `plans/260722-1327-post-mount-pane-mirroring`
- Link-close teardown + toast visibility — `plans/260721-1830-federation-link-cleanup-toast-visibility`
- Agent identity relay — `plans/260721-2353-federation-agents-sidebar-remote-detection`
- Repaint on geometry change + mount size ownership — `plans/260722-1638-remote-workspace-resize-rerender` (merged, PR #2)

## Surviving scope

1. **Reconnect after tunnel fault** — the ssh child dies or the socket drops; today the mount is
   gone and the user re-mounts by hand.
2. **Epoch re-fence on reconnect** — a reconnect must not let a pre-fault connection's in-flight
   frames land against the new mount.
3. **Cold resume** — re-attaching to a serving host whose session outlived the client.
4. **Disconnect UX** — what a mounted pane shows while the link is down, and how it recovers.
5. **Observability gap** — federated mode writes no log file, so a fault leaves nothing to read.

## Known adjacent debt (decide whether to fold in when planning)

- Inbound `ClipboardStageRequest`, `SplitPaneRequest` and `SnapshotRequest` carry no
  `(epoch, connid)` fence. `SplitPane` is the worst: the actor executes it with no
  `is_mounted_controller` check, so a revoked connection can mutate the App. One uniform fence
  would close all three and overlaps directly with item 2 above.
- Direct attach vs mounted controller: after the mount releases, an attach made during the mount
  is stranded at the mount's size until its window resizes or it detaches.

## Open questions

- Is reconnect worth it before the runtime/client protocol split lands, or does that split change
  the seam enough to make this work throwaway?
