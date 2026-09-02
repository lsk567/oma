#!/usr/bin/env bash
# Runs the daemon and Mission Control together, pointed at each other.
#
# Mode is chosen when Mission Control is launched rather than in the UI, so the
# two have to start in the right order with the right address between them.
# Doing that by hand means two terminals and a variable that is easy to forget;
# forgetting it is not an error, it is a UI that quietly cannot deploy.
#
# Both surfaces are loopback-only. Ctrl-C stops both.
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

serve_address=${OMAR_SERVE_ADDRESS:-127.0.0.1:7340}
web_port=${OMAR_WEB_PORT:-3000}
# Set to skip the browser: a smoke test wants the servers, not a window.
open_browser=${OMAR_DEV_OPEN:-1}
# How long each side gets to start answering before this gives up.
ready_timeout=${OMAR_DEV_READY_TIMEOUT:-120}

serve_pid=""
web_pid=""

# `npm run` is a shell wrapping node, so killing the pid this script holds
# leaves the dev server running and the port taken. Take the descendants too.
kill_tree() {
  local pid=$1 child
  for child in $(pgrep -P "$pid" 2>/dev/null); do
    kill_tree "$child"
  done
  kill "$pid" 2>/dev/null || true
}

cleanup() {
  trap - INT TERM EXIT
  for pid in "$web_pid" "$serve_pid"; do
    [[ -n $pid ]] && kill_tree "$pid"
  done
  wait 2>/dev/null || true
}
trap cleanup INT TERM EXIT

wait_for() {
  local url=$1 what=$2 pid=$3 waited=0
  until curl -fsS -o /dev/null "$url" 2>/dev/null; do
    if ! kill -0 "$pid" 2>/dev/null; then
      printf '%s exited before it answered %s\n' "$what" "$url" >&2
      return 1
    fi
    if ((waited >= ready_timeout)); then
      printf '%s did not answer %s within %ds\n' "$what" "$url" "$ready_timeout" >&2
      return 1
    fi
    sleep 1
    waited=$((waited + 1))
  done
}

open_url() {
  local url=$1
  if command -v open >/dev/null 2>&1; then
    open "$url" 2>/dev/null || true
  elif command -v xdg-open >/dev/null 2>&1; then
    xdg-open "$url" >/dev/null 2>&1 || true
  else
    printf 'Open %s\n' "$url"
  fi
}

# Checked here because nothing else checks it: npm ignores `engines` unless the
# project opts into engine-strict, so an old Node installs cleanly and then
# fails inside a dependency, as a SyntaxError about some export that release
# does not have. That trace says nothing about Node versions.
if ! command -v node >/dev/null 2>&1; then
  printf 'Node is not installed; Mission Control needs Node. See web/README.md.\n' >&2
  exit 1
fi
required_node=$(node -p "require('$script_dir/package.json').engines.node.replace(/[^0-9.]/g, '')")
current_node=$(node -p 'process.versions.node')
if [[ $(printf '%s\n%s\n' "$required_node" "$current_node" | sort -V | head -1) != "$required_node" ]]; then
  printf 'Node %s is too old; Mission Control needs >=%s. Yours: %s\n' \
    "$current_node" "$required_node" "$(command -v node)" >&2
  exit 1
fi

printf 'Building the runtime...\n'
(cd "$repo_root" && cargo build --quiet --bin omar)

# Reinstalled when the manifests are newer than the tree, not just when the
# tree is missing: a pull that adds a dependency leaves node_modules in place
# but incomplete, and the failure shows up as a module the dev server cannot
# resolve rather than as anything about installing.
needs_install=0
if [[ ! -d "$script_dir/node_modules" ]]; then
  needs_install=1
else
  for manifest in package.json package-lock.json; do
    if [[ "$script_dir/$manifest" -nt "$script_dir/node_modules" ]]; then
      needs_install=1
    fi
  done
fi

if ((needs_install)); then
  printf 'Installing web dependencies...\n'
  (cd "$script_dir" && npm install --silent)
  # npm only touches node_modules when it changes something, so stamp it to
  # keep this from reinstalling on every run.
  touch "$script_dir/node_modules"
fi

printf 'Starting omar serve on %s...\n' "$serve_address"
"$repo_root/target/debug/omar" serve --address "$serve_address" &
serve_pid=$!
wait_for "http://$serve_address/health" "omar serve" "$serve_pid"

printf 'Starting Mission Control on port %s...\n' "$web_port"
(cd "$script_dir" && OMAR_SERVE_URL="http://$serve_address" npm run dev -- --port "$web_port") &
web_pid=$!
# Named `localhost`, not `127.0.0.1`: the dev server binds the IPv6 loopback
# only, so the v4 address never answers and waiting on it would time out with
# the server up and running.
web_url="http://localhost:$web_port"
wait_for "$web_url/" "Mission Control" "$web_pid"

printf '\nMission Control: %s (live against http://%s)\n' "$web_url" "$serve_address"
printf 'Ctrl-C stops both.\n\n'

[[ $open_browser == 1 ]] && open_url "$web_url"

# Either side going down takes the pair with it: a Mission Control pointed at a
# dead daemon looks live until you try to deploy.
#
# Polled rather than `wait -n`, which bash 3.2 — still what macOS ships — does
# not have.
while kill -0 "$serve_pid" 2>/dev/null && kill -0 "$web_pid" 2>/dev/null; do
  sleep 1
done
