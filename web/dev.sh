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

printf 'Building the runtime...\n'
(cd "$repo_root" && cargo build --quiet --bin omar)

if [[ ! -d "$script_dir/node_modules" ]]; then
  printf 'Installing web dependencies...\n'
  (cd "$script_dir" && npm install --silent)
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
