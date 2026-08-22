#!/usr/bin/env bash
set -euo pipefail

OMAR_BIN="${OMAR_BIN:-target/debug/omar}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux is required" >&2
  exit 1
fi

if [ ! -x "$OMAR_BIN" ]; then
  echo "OMAR binary not found or not executable: $OMAR_BIN" >&2
  exit 1
fi

server="omar-codex-yolo-${RANDOM}-$$"
home_dir="$(mktemp -d)"
state_file="$home_dir/.omar/fake-codex-fallback.log"
fake_codex="$home_dir/fake-codex"
codex_exec="$home_dir/codex"

cleanup() {
  tmux -L "$server" kill-server >/dev/null 2>&1 || true
  rm -rf "$home_dir"
}
trap cleanup EXIT

mkdir -p "$home_dir/.omar"
cat >"$fake_codex" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

state_file="__STATE_FILE__"
prev_attempt=$(cat "${state_file}.count" 2>/dev/null || echo 0)
attempt=$(( prev_attempt + 1 ))
printf '%s\n' "$*" >> "$state_file"
# Recorded so the test can find the per-pane config, when there is one.
printf 'CODEX_HOME=%s\n' "${CODEX_HOME:-}" >> "$state_file"
echo "$attempt" > "${state_file}.count"
printf '%s\n' "attempt ${attempt}" >> "$state_file"

# Always succeeds on first launch.
exec sleep 9999
EOF
awk -v sf="$state_file" '{gsub("__STATE_FILE__", sf); print}' "$fake_codex" > "${fake_codex}.tmp"
mv "${fake_codex}.tmp" "$fake_codex"
chmod +x "$fake_codex"
ln -sf "$fake_codex" "$codex_exec"

cat >"$home_dir/.omar/config.toml" <<EOF
[dashboard]
refresh_interval = 1
session_prefix = "omar-agent-"

[agent]
default_command = "$codex_exec --no-alt-screen --dangerously-bypass-approvals-and-sandbox"
default_workdir = "."
EOF

tmux_cmd() {
  HOME="$home_dir" OMAR_TMUX_SERVER="$server" tmux -L "$server" "$@"
}

wait_for_session() {
  local session="$1"
  for _ in $(seq 1 120); do
    if tmux_cmd has-session -t "$session" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  echo "Timed out waiting for tmux session: $session" >&2
  return 1
}

session_has_live_pane() {
  local session="$1"
  if ! tmux_cmd list-panes -t "$session" -F "#{pane_pid}" 2>/dev/null | tr -d '[:space:]' | grep -q .; then
    return 1
  fi
  return 0
}

wait_for_live_manager() {
  local session="$1"
  for _ in $(seq 1 120); do
    if session_has_live_pane "$session"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

wait_for_state_file() {
  for _ in $(seq 1 120); do
    if [ -s "$state_file" ]; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

wait_for_state_marker() {
  local pattern="$1"
  for _ in $(seq 1 300); do
    if grep -q "$pattern" "$state_file"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

capture_dashboard() {
  tmux_cmd capture-pane -pt omar-dashboard:0.0 -S -200 -p
}

fail() {
  local message="$1"
  printf 'FAIL: %s\n' "$message" >&2
  if [ -f "$state_file" ]; then
    echo "---- fake-codex log ----" >&2
    cat "$state_file" >&2 || true
  fi
  echo "---- dashboard ----" >&2
  capture_dashboard >&2 || true
  echo "---- sessions ----" >&2
  tmux_cmd list-sessions -F '#{session_name}|attached=#{session_attached}|pid=#{session_pid}|cmd=#{pane_current_command}' >&2 || true
  exit 1
}

tmux_cmd new-session -d -s omar-dashboard \
  "cd '$REPO_ROOT' && HOME='$home_dir' OMAR_TMUX_SERVER='$server' '$OMAR_BIN'"

wait_for_session omar-dashboard || fail "dashboard session did not start"
wait_for_session omar-agent-ea-0 || fail "initial manager session failed to appear"
wait_for_live_manager omar-agent-ea-0 || fail "manager session never became live"

if ! wait_for_state_file; then
  fail "fake codex script was never invoked"
fi

sleep 0.1

if ! wait_for_state_marker "attempt 1"; then
  fail "expected initial manager startup attempt"
fi

if ! grep -q "^attempt 1$" "$state_file"; then
  fail "failed to observe first startup attempt"
fi

# codex is configured one of two ways, and which one a machine takes is not a
# choice this test gets to make.
#
# Its TUI only attaches to an app-server when started with no `-c` overrides at
# all, so OMAR writes a per-pane `CODEX_HOME/config.toml` when it can. It
# cannot when the socket that home implies would exceed `sun_path` -- 96 bytes
# -- and that depends on where `mktemp` puts a directory: about 87 bytes on
# Linux, about 129 under macOS's `/var/folders`. Then it falls back to `-c`
# flags on the command line, which is why this passed on a mac and failed in
# CI while both were behaving correctly.
#
# So assert what codex was told, not which route carried it.
configured() {
  local flag_pattern="$1" toml_pattern="$2" home
  if grep -q "$flag_pattern" "$state_file"; then
    return 0
  fi
  home="$(grep -m1 '^CODEX_HOME=' "$state_file" 2>/dev/null | cut -d= -f2-)"
  [ -n "$home" ] && [ -f "$home/config.toml" ] &&
    grep -q "$toml_pattern" "$home/config.toml"
}

if ! configured "mcp_servers\\.omar\\.command" "\\[mcp_servers\\.omar\\]"; then
  fail "codex was not told how to launch omar's MCP server"
fi

if ! configured "mcp_servers\\.omar\\.args" "mcp-server"; then
  fail "codex was not told what arguments omar's MCP server takes"
fi

if ! configured "features.scheduled_tasks=false" "scheduled_tasks = false"; then
  fail "codex scheduled tasks were not disabled"
fi

if grep -q "^attempt 2$" "$state_file"; then
  fail "unexpected retry attempt"
fi

if ! grep -q -- "--dangerously-bypass-approvals-and-sandbox" "$state_file"; then
  fail "startup command did not include the codex bypass flag"
fi

if capture_dashboard | grep -q "failed to start"; then
  fail "dashboard reports failed manager startup"
fi

tmux_cmd kill-session -t omar-dashboard >/dev/null 2>&1 || true
tmux_cmd kill-session -t omar-agent-ea-0 >/dev/null 2>&1 || true

echo "PASS: codex manager startup keeps approval flag on single startup attempt"
