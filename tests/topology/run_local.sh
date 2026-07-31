#!/usr/bin/env bash
set -uo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
source_dir="$script_dir/src"
results_dir=${OMAR_TEST_RESULTS_DIR:-"$repo_root/target/topology-tests"}
case_timeout=${OMAR_TEST_CASE_TIMEOUT_SECONDS:-900}
invocation_timeout=${OMAR_TEST_INVOCATION_TIMEOUT_SECONDS:-300}
case_filter=${OMAR_TEST_CASE:-}
fixture="$script_dir/README.md"
omar="$repo_root/target/debug/omar"
summary="$results_dir/results.tsv"

passed_cases=0
failed_cases=0
passed_checks=0
failed_checks=0
skipped_checks=0
started_at=$SECONDS

mkdir -p "$results_dir"
printf 'status\tcase\tseconds\tpassed\tfailed\tskipped\treason\tlog\n' >"$summary"

verdict() {
  local status=$1
  local elapsed=$((SECONDS - started_at))

  printf '\nTopology integration verdict: %s\n' "$status"
  printf 'Cases: %d passed, %d failed, %d total\n' \
    "$passed_cases" "$failed_cases" "$((passed_cases + failed_cases))"
  printf 'Checks: %d passed, %d failed, %d skipped\n' \
    "$passed_checks" "$failed_checks" "$skipped_checks"
  printf 'Elapsed: %ds\n' "$elapsed"
  printf 'Metrics: %s\n' "$summary"
}

build_prerequisites() {
  printf 'Building OMAR compiler and runtime...\n'
  if ! (cd "$repo_root/lang" && lake build omarc); then
    printf 'FAIL prerequisite: Lean compiler build\n' >&2
    failed_cases=$((failed_cases + 1))
    failed_checks=$((failed_checks + 1))
    printf 'FAIL\tprerequisites\t%d\t0\t1\t0\tLean compiler build\t-\n' \
      "$SECONDS" >>"$summary"
    verdict FAIL
    exit 1
  fi
  if ! (cd "$repo_root" && cargo build --quiet --bin omar); then
    printf 'FAIL prerequisite: Rust runtime build\n' >&2
    failed_cases=$((failed_cases + 1))
    failed_checks=$((failed_checks + 1))
    printf 'FAIL\tprerequisites\t%d\t0\t1\t0\tRust runtime build\t-\n' \
      "$SECONDS" >>"$summary"
    verdict FAIL
    exit 1
  fi
}

run_with_timeout() {
  local timeout=$1
  local log=$2
  local case_name=$3
  shift 3

  python3 - "$timeout" "$log" "$case_name" "$@" <<'PY'
import subprocess
import json
import os
import re
import sys
import time

timeout = int(sys.argv[1])
log_path = sys.argv[2]
case_name = sys.argv[3]
command = sys.argv[4:]
peek_agents = os.environ.get("OMAR_TEST_PEEK_AGENTS", "").lower() in {
    "1", "true", "yes"
}
agent_names = set()
if peek_agents:
    try:
        program_path = command[command.index("run") + 1]
        if program_path.endswith(".omar"):
            with open(program_path, encoding="utf-8") as source_file:
                source = source_file.read()
            agent_names = {
                match.group(1)
                for match in re.finditer(
                    r"\b([A-Za-z_][A-Za-z0-9_]*)\s*:\s*"
                    r"(?:Codex|Claude|ClaudeCode|OpenCode|Cursor|Agy)\b",
                    source,
                    re.IGNORECASE,
                )
            }
        else:
            with open(program_path, encoding="utf-8") as bytecode_file:
                bytecode = json.load(bytecode_file)
            agent_names = {
                instruction["name"]
                for instruction in bytecode["instructions"]
                if instruction["op"] == "spawn_agent"
            }
    except (OSError, ValueError, KeyError, json.JSONDecodeError):
        pass


def capture_agent_panes(elapsed):
    sessions = subprocess.run(
        ["tmux", "list-sessions", "-F", "#{session_name}"],
        capture_output=True,
        text=True,
    )
    if sessions.returncode != 0:
        print(f"[{case_name}] no tmux agent sessions found", flush=True)
        return

    names = [
        name for name in sessions.stdout.splitlines()
        if name.startswith("omar-agent-")
        and any(name.endswith(f"-{agent}") for agent in agent_names)
    ]
    group = os.environ.get("GITHUB_ACTIONS") == "true"
    if group:
        print(f"::group::{case_name} agent panes at {elapsed}s", flush=True)
    else:
        print(f"[{case_name}] agent panes at {elapsed}s", flush=True)

    ansi = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
    for name in names:
        pane = subprocess.run(
            ["tmux", "capture-pane", "-p", "-t", name, "-S", "-20"],
            capture_output=True,
            text=True,
        )
        lines = [
            ansi.sub("", line).rstrip()
            for line in pane.stdout.splitlines()
            if line.strip()
        ][-20:]
        print(f"--- {name} ---", flush=True)
        print("\n".join(lines) if lines else "(empty pane)", flush=True)

    if group:
        print("::endgroup::", flush=True)

with open(log_path, "ab") as log:
    process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
    started = time.monotonic()
    next_heartbeat = 30
    next_snapshot = 60
    while process.poll() is None:
        elapsed = int(time.monotonic() - started)
        if elapsed >= timeout:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            log.write(f"\nCASE TIMEOUT after {timeout}s\n".encode())
            raise SystemExit(124)
        if elapsed >= next_heartbeat:
            print(f"[{case_name}] still running ({elapsed}s elapsed)", flush=True)
            next_heartbeat += 30
        if peek_agents and elapsed >= next_snapshot:
            capture_agent_panes(elapsed)
            next_snapshot += 60
        time.sleep(1)
status = process.returncode
if status != 0 and peek_agents:
    elapsed = int(time.monotonic() - started)
    capture_agent_panes(elapsed)
raise SystemExit(status)
PY
}

run_case() {
  local name=$1
  local team=$2
  local output_csv=$3
  shift 3

  if [[ -n $case_filter && $name != "$case_filter" ]]; then
    return
  fi

  local source="$source_dir/$name.omar"
  local log="$results_dir/$name.log"
  local case_started=$SECONDS
  local case_passed=0
  local case_failed=0
  local case_skipped=0
  local expected_outputs=()
  local total_checks
  local status=PASS
  local reason=-

  IFS=',' read -r -a expected_outputs <<<"$output_csv"
  total_checks=$((2 + ${#expected_outputs[@]}))
  : >"$log"

  printf '\n[%s] running with local agents\n' "$name"
  if run_with_timeout "$case_timeout" "$log" "$name" \
    "$omar" run "$source" --replace \
    --timeout-seconds "$invocation_timeout" "$@"; then
    case_passed=$((case_passed + 1))
  else
    local runtime_status=$?
    status=FAIL
    if ((runtime_status == 124)); then
      reason="case timed out"
    else
      reason="runtime exited $runtime_status"
    fi
    case_failed=$((case_failed + 1))
  fi

  if grep -Fq "Topology '$team' completed" "$log"; then
    case_passed=$((case_passed + 1))
  else
    status=FAIL
    [[ $reason == - ]] && reason="missing completion marker"
    case_failed=$((case_failed + 1))
  fi

  local output
  for output in "${expected_outputs[@]}"; do
    if grep -Fq "Output $output =" "$log"; then
      case_passed=$((case_passed + 1))
    else
      status=FAIL
      [[ $reason == - ]] && reason="missing output $output"
      case_failed=$((case_failed + 1))
    fi
  done

  local elapsed=$((SECONDS - case_started))
  passed_checks=$((passed_checks + case_passed))
  failed_checks=$((failed_checks + case_failed))
  skipped_checks=$((skipped_checks + case_skipped))

  if [[ $status == PASS ]]; then
    passed_cases=$((passed_cases + 1))
    printf 'PASS %-18s %2ds  checks %d/%d\n' \
      "$name" "$elapsed" "$case_passed" "$total_checks"
  else
    failed_cases=$((failed_cases + 1))
    printf 'FAIL %-18s %2ds  passed=%d failed=%d skipped=%d  reason=%s  log=%s\n' \
      "$name" "$elapsed" "$case_passed" "$case_failed" "$case_skipped" \
      "$reason" "$log"
  fi

  printf '%s\t%s\t%d\t%d\t%d\t%d\t%s\t%s\n' \
    "$status" "$name" "$elapsed" "$case_passed" "$case_failed" \
    "$case_skipped" "$reason" "$log" >>"$summary"
}

build_prerequisites

run_case AllTypes AllTypes \
  bool_out,int_out,float_out,string_out,path_out,bytes_out,list_out,option_out,nested_out \
  --input bool_in=true \
  --input int_in=7 \
  --input float_in=2.5 \
  --input string_in=hello \
  --input "path_in=$fixture" \
  --input bytes_in=SGVsbG8= \
  --input 'list_in=[1,2,3]' \
  --input 'option_in="present"' \
  --input 'nested_in=[1,null,3]'

run_case BusinessLoop BusinessLoop delivery \
  --input brief='Launch a reliable self-service analytics product for small retailers'
run_case EffectContracts EffectContracts accepted \
  --input candidate='a careful systems engineer'
run_case FanOutFanIn FanOutFanIn answer \
  --input request='design a resilient task queue'
run_case HR HR hired --input "resume=$fixture"
run_case HRCodex HR hired --input "resume=$fixture"
run_case OrTriggers OrTriggers observation \
  --input request='compare the fast and slow paths'
run_case OrderedWrites OrderedWrites result \
  --input request='declaration order'
run_case Recurrence Recurrence result --input start=0
# Instantiated by a main block, so the program is 'main' and every port is
# qualified. Only the seed is supplied; the other two inputs come off the ring.
run_case Ring main n1.done --input n1.token=0
run_case SameAgentSerial SameAgentSerial result \
  --input request='serialize these reactions'
run_case SuperdenseTime SuperdenseTime fixed_result,connected_result --input start=7

if ((passed_cases + failed_cases == 0)); then
  printf 'No topology case named %s\n' "$case_filter" >&2
  failed_cases=1
  failed_checks=1
  printf 'FAIL\t%s\t0\t0\t1\t0\tunknown case\t-\n' "$case_filter" >>"$summary"
fi

if ((failed_cases > 0)); then
  verdict FAIL
  exit 1
fi

verdict PASS
