# Remote agents sidebar still missing after 6bbc829 — root cause

## Symptom (confirmed live)
- Local `agent.list` on Mac client (env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH herdr agent list) returns ONLY the local `wC:p1` agent. The remote pane `r:appn-ltu-vm-105#default:w3:p2` (real live Claude session, confirmed via `ssh appn-ltu-vm-105 ... agent.list` -> `agent_status:"working"`) is absent entirely, not just wrong status.
- `workspace.list` on Mac client shows `r:appn-ltu-vm-105#default:w3` and `r:appn-ltu-vm-100#default:w1` with `agent_status:"unknown"`.
- So this is not a stale-status bug — the remote pane never becomes an "agent terminal" client-side at all.

## Evidence chain

### 1. Status relay itself is wired (6bbc829's fix is intact, not the break)
- Serve side (`src/server/federation_accept.rs:836-867 poll_agent_statuses`, ticker every `OUTBOUND_POLL_INTERVAL*AGENT_STATUS_POLL_DIVISOR`=100ms) diffs against an empty `HashMap`, so the very first poll after mount always emits a frame for any known agent (H1 "only emits on change, never re-sends pre-mount state" is FALSE — first poll always fires because `last.get(...)` is `None`).
- Actor side confirms real data: `FederationCommand::AgentStatuses` (`src/server/federation_actor.rs:246-260`) calls the live `Method::AgentList` and does return real `working` status — verified live: `ssh appn-ltu-vm-105 ... agent.list` shows `term_65729e57aad531 agent_status:working`.
- Client applies it: `src/remote/federation/client.rs:509-523` on `FederationMessage::AgentStatus` calls `mirror.apply_agent_status(...)` then `router.route_agent_status(&status_msg.terminal_id, status_msg.status)`.
- `register_agent_status_sender` is wired at INITIAL MOUNT time too, not just remote-split: `src/app/creation.rs:747-748` inside `build_remote_pane` (called from `materialize_federation_mount`, which IS live-wired from `handle_workspace_mount_remote`, `src/app/api/workspaces.rs:164` — contrary to a stale doc comment at `creation.rs:516` calling it "dormant").
- The relayed value reaches the detection task: `src/pane.rs:680-707`, `Some(relayed_status) = relayed_status_recv` branch maps `AgentStatus`->`AgentState` and calls `publish_state_changed_event(..., agent_presence.current_agent(), mapped, ...)`.

**Verdict H2 (serve never emits in prod)**: FALSE, disproved by live VM probe + code path above.

### 2. THE ACTUAL BREAK: agent IDENTITY is never established for remote panes, so the pane is filtered out of the agent list entirely
- `collect_agent_infos()` (`src/app/agents.rs:7-21`) enumerates every pane but `agent_info()` (`src/app/agents.rs:408-441`) does `if !terminal.is_agent_terminal() { return None; }` — panes that aren't recognized as agent terminals are silently dropped, not shown with `unknown` status.
- `TerminalState::is_agent_terminal()` (`src/terminal/state.rs:1333-1337`): `self.agent_name.is_some() || self.effective_agent_label().is_some() || self.launch_argv.is_some()`. `launch_argv` is never set for remote-mirrored panes (`build_remote_pane`, `src/app/creation.rs`, greps clean of `launch_argv`), so this reduces to needing an identified `agent_name`/label.
- Agent IDENTITY (`Option<Agent>`, i.e. "this is claude/codex/etc") is set in the detection task **only** via the process-probe path: `src/pane.rs:728 let should_check_process = pid > 0 && {...}`. This gate is `pid > 0` — `pid` is `child_pid.load(...)`, the LOCAL OS process id. Remote-mirrored panes (`TerminalRuntime::spawn_remote`) have no local child process, so `pid` stays `0` and `should_check_process` is always `false`. The whole `if should_check_process { ... agent_presence.observe_process_probe(...) ... }` block (`src/pane.rs:747-828`, the only place that ever sets `agent_presence.current_agent()` = `Some(agent)`) never runs for a remote pane.
- The comment at `src/pane.rs:608-613` claims "screen-text detection... still supplies agent identity" for remote panes, but this is FALSE per source: `detect_agent_with_osc` (`src/detect/mod.rs:204-227`) takes `agent: Option<Agent>` as an INPUT, and its very first line (`src/detect/mod.rs:210-218`) short-circuits to `AgentState::Unknown` with no publish when `agent` is `None` — it refines *state* for an already-identified agent, it never *identifies* one from screen content. So screen-text detection cannot bootstrap identity either.
- The federation wire protocol itself cannot carry identity over the relay path: `AgentStatusMessage` (`src/remote/federation/protocol/mod.rs:219-223`) has only `terminal_id`, `mount_generation`, `status` — no agent label/name field. So even if the local screen-text path could theoretically identify an agent, the relay that "fixed" 6bbc829 structurally has nothing to give it, and the relayed-status consumer (`src/pane.rs:680-707`) only ever writes to `state`, never to `agent_presence`/`terminal.agent_name`.

**Net effect**: relayed `AgentStatus::Working` updates the detection task's local `state` variable (used for the `PaneAgentStatusChanged` event's *status* field), but `agent_presence.current_agent()` stays `None` forever for a remote-mirrored pane. `terminal.agent_name`/`effective_agent_label()` never get set. `is_agent_terminal()` is always `false`. `agent_info()` returns `None`. The pane never appears in `agent.list`, hence never appears in the sidebar agents panel. This is consistent with the live evidence: local `agent.list` omits the remote pane outright (not "unknown" — literally missing), and `workspace.list`'s aggregate `agent_status` for the remote workspace is `unknown` (aggregate over zero identified agent-terminals, `ws.aggregate_state`, `src/app/creation.rs:470-483`).

## Ranked hypotheses (final)
1. **PROVEN — identity never established for remote panes** (this report, section 2). High confidence, full file:line chain, live-probe-confirmed absence in `agent.list`.
2. H1 (serve only emits on transition, no re-send at mount) — disproved, first poll always emits (empty `last` map).
3. H2 (serve never emits AgentStatus in prod, only tests) — disproved, live VM `agent.list` via the actor path returns real status; wiring traced end to end.
4. H3 (sidebar ignores remote panes entirely, unrelated to status) — partially true as an *effect*: the sidebar/agent-list source (`collect_agent_infos`) does functionally ignore remote panes, but the cause is not "sidebar doesn't look at remote data," it's "remote panes never pass the `is_agent_terminal()` gate" — same underlying identity gap.
5. H4 (mount snapshot carries no status / TUI client attach drops it) — not the primary break; status IS delivered to the detection task (`src/pane.rs:680-707`), it's just discarded for list purposes since it never touches identity.

## Minimal fix shape (not applied — for the fixer to evaluate)
Two independent gaps to close, either one alone is insufficient:
- Server side: `AgentStatusMessage` needs an agent-identity field (e.g. `agent: Option<String>`), sourced from the same `Method::AgentList` response already used in `FederationCommand::AgentStatuses` (`src/server/federation_actor.rs:257`, which already has `agent.agent` from `AgentInfo`, just not threaded into the reply tuple/message).
- Client side: the `relayed_status_recv` branch (`src/pane.rs:680-707`) needs to also set `agent_presence`/`terminal.agent_name` (or an equivalent identity path bypassing the `pid>0` gate) when a relayed identity arrives, so `is_agent_terminal()` can go true for remote-mirrored panes.

## Unresolved / needs confirmation
- Whether `AgentStatus::Idle` (vs `Unknown`) alone, once identity is fixed, is sufficient for the sidebar filter, or whether `agent_status: Unknown` panes are also excluded downstream (secondary, not blocking).
- Have not traced the exact UI sidebar render call site (`src/ui/sidebar*.rs`) since `agent.list`'s server-side gate already fully explains the symptom before UI code is reached.
