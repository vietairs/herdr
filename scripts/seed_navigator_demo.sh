#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
usage: scripts/seed_navigator_demo.sh [--allow-main]

Seeds a running herdr dev server with navigator demo workspaces, tabs, panes,
and fake agent states. Most panes are intentionally unnamed.

Environment:
  HERDR_NAV_SOCKET_PATH  API socket to target. Defaults to $HOME/.config/herdr-dev/herdr.sock.
  HERDR_NAV_CWD          Workspace cwd for created panes. Defaults to the repo root.
USAGE
}

allow_main=0
while (($#)); do
  case "$1" in
    --allow-main)
      allow_main=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "$script_dir/.." && pwd)"
workspace_cwd="${HERDR_NAV_CWD:-$repo_dir}"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
dev_socket="$config_home/herdr-dev/herdr.sock"
main_socket="$config_home/herdr/herdr.sock"
export HERDR_SOCKET_PATH="${HERDR_NAV_SOCKET_PATH:-$dev_socket}"

if [[ "$allow_main" != 1 && "$HERDR_SOCKET_PATH" == "$main_socket" ]]; then
  echo "refusing to seed main herdr session: $HERDR_SOCKET_PATH" >&2
  echo "use HERDR_NAV_SOCKET_PATH for a dev socket, or pass --allow-main intentionally" >&2
  exit 1
fi

if [[ ! -S "$HERDR_SOCKET_PATH" ]]; then
  echo "herdr socket not found: $HERDR_SOCKET_PATH" >&2
  echo "start a dev server first, or set HERDR_NAV_SOCKET_PATH" >&2
  exit 1
fi

cd "$repo_dir"

run() { cargo run --quiet -- "$@"; }

mkws() {
  local label="$1"
  run workspace create --label "$label" --cwd "$workspace_cwd" --no-focus \
    | jq -r '.result.workspace.workspace_id + " " + .result.root_pane.pane_id + " " + .result.tab.tab_id'
}

mktab() {
  local ws="$1" label="$2"
  run tab create --workspace "$ws" --label "$label" --cwd "$workspace_cwd" --no-focus \
    | jq -r '.result.tab.tab_id + " " + .result.root_pane.pane_id'
}

split() {
  local pane="$1" direction="$2"
  run pane split "$pane" --direction "$direction" --no-focus \
    | jq -r '.result.pane.pane_id'
}

rename_sparse() {
  local pane="$1" label="$2"
  run pane rename "$pane" "$label" >/dev/null
}

report() {
  local pane="$1" agent="$2" state="$3" status="$4" seq="$5"
  run pane report-agent "$pane" \
    --source nav-seed \
    --agent "$agent" \
    --state "$state" \
    --custom-status "$status" \
    --seq "$seq" >/dev/null
}

stamp="$(date +%H%M%S)"
done_panes=()

read WS1 P1 T1 < <(mkws "nav-${stamp}-claude-review")
P2="$(split "$P1" down)"
P3="$(split "$P1" right)"
rename_sparse "$P2" "approval needed"
report "$P1" claude blocked blocked 1
report "$P2" claude working working 1
# P3 stays shell/unknown.

read WS2 P4 T2 < <(mkws "nav-${stamp}-codex-build")
P5="$(split "$P4" right)"
read T2B P6 < <(mktab "$WS2" tests)
P7="$(split "$P6" down)"
report "$P4" codex working working 1
report "$P5" codex working working 1
report "$P6" codex idle idle 1
report "$P7" codex working working 1
done_panes+=("$P7:codex")

read WS3 P8 T3 < <(mkws "nav-${stamp}-finished")
P9="$(split "$P8" down)"
P10="$(split "$P8" right)"
rename_sparse "$P10" "release notes"
report "$P8" claude working working 1
report "$P9" claude working working 1
report "$P10" codex idle idle 1
done_panes+=("$P8:claude" "$P9:claude")

read WS4 P11 T4 < <(mkws "nav-${stamp}-quiet")
read T4B P12 < <(mktab "$WS4" notes)
P13="$(split "$P12" right)"
report "$P12" pi idle idle 1
# P11 and P13 stay shell/unknown.

read WS5 P14 T5 < <(mkws "nav-${stamp}-mixed")
P15="$(split "$P14" down)"
P16="$(split "$P14" right)"
read T5B P17 < <(mktab "$WS5" agents)
P18="$(split "$P17" down)"
rename_sparse "$P14" "prod decision"
report "$P14" claude blocked blocked 1
report "$P15" codex blocked blocked 1
report "$P16" claude working working 1
report "$P17" codex working working 1
report "$P18" claude idle idle 1

read WS6 P19 T6 < <(mkws "nav-${stamp}-dense")
P20="$(split "$P19" down)"
P21="$(split "$P19" right)"
read T6B P22 < <(mktab "$WS6" long-run)
P23="$(split "$P22" down)"
P24="$(split "$P22" right)"
report "$P19" codex working working 1
report "$P20" claude idle idle 1
report "$P21" codex blocked blocked 1
report "$P22" claude working working 1
report "$P23" codex working working 1
report "$P24" claude working working 1
done_panes+=("$P24:claude")

run workspace focus "$WS1" >/dev/null
seq=2
for item in "${done_panes[@]}"; do
  pane="${item%%:*}"
  agent="${item##*:}"
  report "$pane" "$agent" idle done "$seq"
  seq=$((seq + 1))
done

cat <<EOF
Seeded navigator demo data via $HERDR_SOCKET_PATH

Workspaces:
  $WS1 nav-${stamp}-claude-review  claude blocked + claude working + shell
  $WS2 nav-${stamp}-codex-build    codex working x2 + idle + done, tab $T2B
  $WS3 nav-${stamp}-finished       claude done x2 + codex idle
  $WS4 nav-${stamp}-quiet          shell x2 + pi idle, tab $T4B
  $WS5 nav-${stamp}-mixed          blocked x2 + working x2 + idle, tab $T5B
  $WS6 nav-${stamp}-dense          blocked + working x4 + idle + done, tab $T6B

Sparse naming is intentional. Most panes have no manual names.
Test in navigator: / then b, w, i, d, a, or normal text.
EOF
