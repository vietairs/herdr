# Blindspot synthesis — multi-remote federated workspace launch

Goal: `herdr --remote localhost 131.172.248.163 131.172.248.161 --remote-workspace` → 1 local + N remote workspaces in one TUI.

## Top blind spots (ranked)

1. **"Local coexistence" is the real feature, not multi-mount.** P9.2b option-b deliberately renders the
   federated workspace ALONE (no local workspaces in the same App). The request needs local + remote
   coexisting in one chrome — this reverses a recorded design decision, not just lifts a guard.
   Evidence: run_federated_session builds its own App; main.rs 706–716 local autodetect path is mutually
   exclusive with --remote.
2. **`localhost` target semantics.** No special-casing exists — `--remote localhost` would literally SSH to
   localhost (needs sshd + herdr installed there). Decision needed: treat `localhost` as "the local
   workspace" (skip SSH, use local server) vs. honest SSH target. User intent = local workspace.
3. **Single-mount registry shape.** AppState::remote_mirror is Option<RemoteMirror> (state.rs:1494) with
   typed guard begin_federation_mount → AlreadyMounted (1522–1531). Multi-mount = Option → keyed
   collection (HostKey → RemoteMirror), plus end_federation_mount/double_attach_conflict updates.
   Comments confirm data model (HostKey/server_instance_id/mount_generation) is multi-remote ready.
4. **Partial-failure semantics undefined.** FederationRoute has no partial-success shape. With N targets:
   per-target fallback vs all-or-nothing must be decided (predict input).
5. **CLI parse: singular target enforced twice** (unix.rs 113–114, 124–125) + Windows parity (src/remote.rs
   67, 78) + test `extract_remote_args_rejects_duplicate_values` (2808–2818) enforces the old contract.
6. **Per-mount lifecycle.** Teardown (end_federation_mount sets None), reconnect, and the P9.3 FSM all
   assume one mount; each mount needs independent tunnel + teardown without killing siblings.
7. **Sidebar already multi-host ready** (badge derived from workspace id r:<host_key>:...) — low risk.

## Better-prompt (input to predict + plan)
Extend herdr so one command starts a combined session: the normal LOCAL server/workspaces plus N remote
federated workspaces (one per --remote target), rendered together in one TUI with per-host sidebar groups.
`localhost` in the target list means the local workspace (no SSH). Each remote target gets its own tunnel,
mount, HostKey, and independent lifecycle; a failed target degrades to a notice (others proceed) — exact
fallback policy to be decided at plan validation. Keep classic single-target `--remote` behavior unchanged
when --remote-workspace is absent. Windows arg-parse parity required; multi-value `--remote` accepts
space-separated targets terminated by the next flag.

Scout reports: blindspot-scout-cli-parse-lifecycle-surface.md (this dir) + single-mount enumeration (12 sites, inline above).
