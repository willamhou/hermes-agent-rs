#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=examples/lib/managed-run-helpers.sh
source "${SCRIPT_DIR}/lib/managed-run-helpers.sh"

usage() {
  cat <<'EOF'
Usage:
  bash examples/replay-managed-run.sh [--run <run-id> | --latest | --agent <name>] [--json] [--limit <n>]

Examples:
  bash examples/replay-managed-run.sh --latest
  bash examples/replay-managed-run.sh --agent code-reviewer
  bash examples/replay-managed-run.sh --run run_123 --json

Notes:
  - The script resolves the Hermes CLI from `HERMES_CLI_BIN`, `target/release/hermes`, or
    `cargo run --release -p hermes-cli -- ...`.
  - `--latest` is the default when no selector is provided.
  - The default stdout output is the new replayed run id for easy shell capture.
EOF
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
setup_hermes_cli

if [[ "${MODE}" != "run" ]]; then
  RUN_ID="$(resolve_managed_run_id "${MODE}" "${RUN_ID}" "${AGENT_NAME}" "${LIMIT}")"
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

echo "Selected source run: ${RUN_ID}" >&2

replay_json="$(hermes_cli runs replay "${RUN_ID}" --json)"
replayed_run_id="$(printf '%s' "${replay_json}" | extract_run_id_from_envelope)"

if [[ -z "${replayed_run_id}" ]]; then
  echo "Failed to extract replayed run id from response." >&2
  exit 1
fi

echo "Replayed run: ${replayed_run_id}" >&2

if [[ "${OUTPUT_JSON}" -eq 1 ]]; then
  printf '%s\n' "${replay_json}"
else
  printf '%s\n' "${replayed_run_id}"
fi
