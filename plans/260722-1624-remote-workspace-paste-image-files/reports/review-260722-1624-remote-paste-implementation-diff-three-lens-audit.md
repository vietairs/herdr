# Stage 7 review gate — implementation diff (2026-07-22/23)

Three independent lenses over 19 files / 2858 insertions + 2 new modules.
Correctness (opus) | Security (opus) | codex-cli 0.144.4 adversarial.

## Convergence
- bidi/Cf gap at the client returned-path boundary: found by ALL THREE independently.
- silent drop of a completed stage response under egress backpressure: found by TWO.

## Threat-model correction (bounds everything below)
The returned path is pasted into the REMOTE pane's PTY: try_send_paste -> try_send_bytes ->
pane_source forward task -> TerminalChannelMessage::Input over the wire. A hostile remote
injecting shell metacharacters is therefore attacking ITS OWN shell. What it genuinely gains
is (a) control of what the user sees rendered and (b) what the AGENT in that pane consumes.
Direction B is likewise defence-in-depth, not a privilege boundary: the federation listener is
a unix socket in the host user's runtime dir, so a 'hostile mounting client' is already
authenticated as the herdr user on the remote.

## Raw reports
- correctness + security: tasks/w3u5gybno.output
- codex: tasks/b0j3phlvs.output
