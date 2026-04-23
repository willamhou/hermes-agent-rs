#!/usr/bin/env bash

MANAGED_RUN_HELPERS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${MANAGED_RUN_HELPERS_DIR}/../.." && pwd)"

HERMES_CLI_BIN_PATH="${HERMES_CLI_BIN_PATH:-}"
CARGO_BIN="${CARGO_BIN:-}"

require_command() {
  local name="$1"
  if ! command -v "${name}" >/dev/null 2>&1; then
    echo "${name} is required." >&2
    exit 1
  fi
}

resolve_cargo_bin() {
  if command -v cargo >/dev/null 2>&1; then
    command -v cargo
    return 0
  fi

  if [[ -x "${HOME}/.cargo/bin/cargo" ]]; then
    printf '%s\n' "${HOME}/.cargo/bin/cargo"
    return 0
  fi

  echo "cargo is required but was not found in PATH or ~/.cargo/bin/cargo." >&2
  exit 1
}

setup_hermes_cli() {
  if [[ -n "${HERMES_CLI_BIN:-}" ]]; then
    if [[ ! -x "${HERMES_CLI_BIN}" ]]; then
      echo "HERMES_CLI_BIN is set but not executable: ${HERMES_CLI_BIN}" >&2
      exit 1
    fi
    HERMES_CLI_BIN_PATH="${HERMES_CLI_BIN}"
    return 0
  fi

  if [[ -x "${REPO_ROOT}/target/release/hermes" ]]; then
    HERMES_CLI_BIN_PATH="${REPO_ROOT}/target/release/hermes"
    return 0
  fi

  CARGO_BIN="$(resolve_cargo_bin)"
}

hermes_cli() {
  (
    cd "${REPO_ROOT}"
    if [[ -n "${HERMES_CLI_BIN_PATH}" ]]; then
      "${HERMES_CLI_BIN_PATH}" "$@"
    else
      "${CARGO_BIN}" run --release -p hermes-cli -- "$@"
    fi
  )
}

select_run_id_from_list() {
  local agent_name="$1"
  python3 -c '
import json
import sys

agent_name = sys.argv[1]
payload = json.load(sys.stdin)

for item in payload.get("data", []):
    if agent_name and item.get("agent_name") != agent_name:
        continue
    run = item.get("run") or {}
    run_id = run.get("id")
    if run_id:
        print(run_id)
        break
' "${agent_name}"
}

resolve_managed_run_id() {
  local mode="$1"
  local requested_run_id="$2"
  local agent_name="$3"
  local limit="$4"
  local runs_json=""
  local selector=""

  case "${mode}" in
    run)
      printf '%s\n' "${requested_run_id}"
      return 0
      ;;
    latest)
      ;;
    agent)
      selector="${agent_name}"
      ;;
    *)
      echo "Unknown run selection mode: ${mode}" >&2
      return 1
      ;;
  esac

  runs_json="$(hermes_cli runs list --limit "${limit}" --json)"
  printf '%s' "${runs_json}" | select_run_id_from_list "${selector}"
}

extract_run_status() {
  python3 -c '
import json
import sys

payload = json.load(sys.stdin)
run = payload.get("run") or {}
status = run.get("status") or ""
if status:
    print(status)
'
}

extract_run_id_from_envelope() {
  python3 -c '
import json
import sys

payload = json.load(sys.stdin)
run = payload.get("run") or {}
run_id = run.get("id") or ""
if run_id:
    print(run_id)
'
}

wait_for_managed_run_terminal() {
  local run_id="$1"
  local timeout_secs="$2"
  local poll_ms="$3"
  local start_ts
  local now_ts
  local poll_secs
  local run_json=""
  local status=""

  start_ts="$(date +%s)"
  poll_secs="$(python3 -c 'import sys; print(max(int(sys.argv[1]), 1) / 1000)' "${poll_ms}")"

  while true; do
    run_json="$(hermes_cli runs get "${run_id}" --json)"
    status="$(printf '%s' "${run_json}" | extract_run_status)"
    case "${status}" in
      completed|failed|cancelled|timed_out)
        printf '%s\n' "${status}"
        return 0
        ;;
      pending|running)
        ;;
      "")
        echo "Failed to parse managed run status for ${run_id}." >&2
        return 1
        ;;
      *)
        echo "Unknown managed run status '${status}' for ${run_id}." >&2
        return 1
        ;;
    esac

    now_ts="$(date +%s)"
    if (( now_ts - start_ts >= timeout_secs )); then
      echo "Timed out after ${timeout_secs}s waiting for managed run ${run_id}." >&2
      return 1
    fi

    sleep "${poll_secs}"
  done
}
