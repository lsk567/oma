#!/usr/bin/env bash
# Smoke test for `make dev`.
#
# The script's whole job is that both sides come up pointed at each other and
# both go down together. That is not something the Node or Rust suites can see,
# and every failure it has had so far — a readiness probe on an address the dev
# server does not bind, `wait -n` on a bash that does not have it, a kill that
# left the dev server orphaned — looked fine until something ran it.
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
web_dir=$(cd "$script_dir/.." && pwd)

serve_address=${SMOKE_SERVE_ADDRESS:-127.0.0.1:7351}
web_port=${SMOKE_WEB_PORT:-3021}
web_url="http://localhost:$web_port"

dev_pid=""
log=$(mktemp)

finish() {
  [[ -n $dev_pid ]] && kill "$dev_pid" 2>/dev/null || true
  # Belt and braces: if the script leaked children, this test must not.
  pkill -f "vinext dev --port $web_port" 2>/dev/null || true
  pkill -f "omar serve --address $serve_address" 2>/dev/null || true
  rm -f "$log"
}
trap finish EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  printf -- '--- dev.sh output ---\n' >&2
  cat "$log" >&2
  exit 1
}

printf 'Starting dev.sh...\n'
OMAR_DEV_OPEN=0 \
  OMAR_SERVE_ADDRESS="$serve_address" \
  OMAR_WEB_PORT="$web_port" \
  "$web_dir/dev.sh" >"$log" 2>&1 &
dev_pid=$!

for _ in $(seq 1 300); do
  grep -q "Ctrl-C stops both" "$log" && break
  kill -0 "$dev_pid" 2>/dev/null || fail "dev.sh exited before it was ready"
  sleep 1
done
grep -q "Ctrl-C stops both" "$log" || fail "dev.sh never reported ready"

curl -fsS "http://$serve_address/health" | grep -q '"status":"ok"' \
  || fail "the daemon did not answer /health"
curl -fsS -o /dev/null "$web_url/" || fail "Mission Control did not answer"

# Live mode is the point: the page is rendered with the daemon's address, so a
# demo-mode shell here means the two started without being pointed at anything.
curl -fsS "$web_url/" | grep -q "$serve_address" \
  || fail "the page does not name the daemon, so it launched in demo mode"

printf 'Both answered. Stopping...\n'
kill "$dev_pid"
for _ in $(seq 1 30); do
  kill -0 "$dev_pid" 2>/dev/null || break
  sleep 1
done

# Nothing may outlive the script: an orphaned dev server holds the port and the
# next run fails with something that looks unrelated.
sleep 2
! curl -fsS -o /dev/null "$web_url/" 2>/dev/null \
  || fail "Mission Control outlived dev.sh"
! curl -fsS -o /dev/null "http://$serve_address/health" 2>/dev/null \
  || fail "the daemon outlived dev.sh"

printf 'PASS: both came up pointed at each other, and both went down.\n'
