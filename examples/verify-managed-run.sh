#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  bash examples/verify-managed-run.sh [--run <run-id> | --latest | --agent <name>] [--json] [--limit <n>]

Examples:
  bash examples/verify-managed-run.sh --latest
  bash examples/verify-managed-run.sh --agent code-reviewer
  bash examples/verify-managed-run.sh --run run_123 --json

Notes:
  - The script resolves the Hermes CLI from `cargo run --release -p hermes-cli -- ...`.
  - `--latest` is the default when no selector is provided.
  - Verification runs with `--strict`, so it exits non-zero if receipts are missing or invalid.
  - Signet receipts are only recorded when the managed run actually executes at least one tool call.
EOF
}

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

hermes_cli() {
  (
    cd "${REPO_ROOT}"
    "${CARGO_BIN}" run --release -p hermes-cli -- "$@"
  )
}

select_run_id() {
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

MODE="latest"
RUN_ID=""
AGENT_NAME=""
LIMIT=100
OUTPUT_JSON=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --run)
      RUN_ID="${2:-}"
      MODE="run"
      shift 2
      ;;
    --agent)
      AGENT_NAME="${2:-}"
      MODE="agent"
      shift 2
      ;;
    --latest)
      MODE="latest"
      shift
      ;;
    --json)
      OUTPUT_JSON=1
      shift
      ;;
    --limit)
      LIMIT="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ "${MODE}" == "run" && -z "${RUN_ID}" ]]; then
  echo "--run requires a run id." >&2
  exit 1
fi

if [[ "${MODE}" == "agent" && -z "${AGENT_NAME}" ]]; then
  echo "--agent requires an agent name." >&2
  exit 1
fi

if ! [[ "${LIMIT}" =~ ^[0-9]+$ ]] || [[ "${LIMIT}" -lt 1 ]]; then
  echo "--limit must be a positive integer." >&2
  exit 1
fi

require_command python3
CARGO_BIN="$(resolve_cargo_bin)"

if [[ "${MODE}" != "run" ]]; then
  runs_json="$(hermes_cli runs list --limit "${LIMIT}" --json)"
  selector=""
  if [[ "${MODE}" == "agent" ]]; then
    selector="${AGENT_NAME}"
  fi
  RUN_ID="$(printf '%s' "${runs_json}" | select_run_id "${selector}")"
fi

if [[ -z "${RUN_ID}" ]]; then
  case "${MODE}" in
    latest)
      echo "No managed runs found." >&2
      ;;
    agent)
      echo "No managed runs found for agent '${AGENT_NAME}' in the latest ${LIMIT} entries." >&2
      ;;
    *)
      echo "Failed to resolve a managed run id." >&2
      ;;
  esac
  exit 1
fi

echo "Selected run: ${RUN_ID}" >&2

if [[ "${OUTPUT_JSON}" -eq 1 ]]; then
  hermes_cli runs verify "${RUN_ID}" --json --strict
else
  hermes_cli runs verify "${RUN_ID}" --quiet --strict
  echo "Signet verification OK for run ${RUN_ID}" >&2
fi
