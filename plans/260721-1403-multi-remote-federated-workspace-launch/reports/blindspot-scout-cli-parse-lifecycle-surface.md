# Blindspot scout — CLI parse + launch lifecycle surface (multi-target --remote)

Source: hvn-scout, 2026-07-21 14:05

## 1. main.rs dispatch
- configure_from_args 436–443; extract_remote_args call 444–451
- Mutual-exclusivity guard 453–465 (remote + subcommand → error)
- Help: usage 528 (single target), options 625–627; --remote-workspace NOT in help / known-flags 651–661
- Dispatch 693–701: remote_launch → remote::run_remote, else local auto-detect

## 2. RemoteLaunch / extract_remote_args (src/remote/unix.rs)
- RemoteLaunch 61–71: `target: String` (SINGULAR), keybindings, live_handoff, federation_flag
- extract_remote_args 82–174; single-target guard 113–114 ("can only be specified once")
- --remote space form 112–121, equals form 123–129; --remote-workspace 107–111 (bool flag)
- federation_requested() 78–80 (flag OR HERDR_REMOTE_FEDERATION=1); validate_remote_target 176–184

## 3. Federation mount chain (unix.rs)
- run_remote 448–539 (single RemoteLaunch); prep 467–474; federation check 479–480
- attempt_federation_mount 293–378 (ssh -T <target> "herdr --session <name> federation-serve"; ~25s timeout at 347)
- decide_federation_route 489–492; Federated branch 494–523 → run_federated_session; ClassicFallback 524–527
- LOOP POINT: run_remote would iterate targets → attempt_federation_mount per target

## 4. Local workspace start
- main.rs 706–716: no --remote → server::autodetect::auto_detect_launch (autodetect.rs:291–307; spawn_server_daemon :300; run_client :306)
- NO "local + remote mounts alongside" logic exists. P9.2b option-b: federated workspace renders ALONE, no local coexistence.

## 5. Env/config
- HERDR_REMOTE_FEDERATION (unix.rs:58, read :480); HERDR_REMOTE_KEYBINDINGS :29; HERDR_REATTACH_COMMAND :27
- manage_ssh_config 462–465

## 6. Tests (unix.rs 2576–2825)
14 extract_remote_args tests; key: `extract_remote_args_rejects_duplicate_values` 2808–2818 ENFORCES single target — must invert/replace. Also 2653–2676 (--remote-workspace flag tests).

## 7. Capability fallback
- FederationMountFailure 226–247 (Unsupported/Failed → notice); FederationRoute 255–266 (Federated / ClassicFallback / ClassicUnchanged)
- No partial-success handling — per-target fallback strategy undefined for multi-target.

## Scout's unresolved questions
1. Multi-mount fallback: per-target vs all-or-nothing?
2. Session affinity: per-target session name vs shared active_name()?
3. Expose --remote-workspace in --help?
4. --remote-keybindings: global or per-target?

Status: DONE
