#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${HERMES_GATEWAY_BASE_URL:-${GATEWAY_BASE_URL:-http://127.0.0.1:8080}}"
API_KEY="${HERMES_API_KEY:-}"
MODEL="${MANAGED_MODEL:-openai/gpt-4o-mini}"
AGENT_NAME="${MANAGED_AGENT_NAME:-code-reviewer-$(date +%s)}"
PROMPT="${MANAGED_USER_PROMPT:-Review this repository for the riskiest missing tests.}"

if [[ -z "${API_KEY}" ]]; then
  echo "HERMES_API_KEY is required." >&2
  echo "This script assumes the Hermes gateway is already running at ${BASE_URL}." >&2
  exit 1
fi

json() {
  curl -sS \
    -H "Authorization: Bearer ${API_KEY}" \
    -H "Content-Type: application/json" \
    "$@"
}

extract_json_string() {
  local key="$1"
  tr -d '\n' | sed -n "s/.*\"${key}\":\"\\([^\"]*\\)\".*/\\1/p"
}

echo
echo "== Health =="
curl -sS "${BASE_URL}/health"

echo
echo
echo "== Create agent: ${AGENT_NAME} =="
create_response="$(
json \
  -X POST "${BASE_URL}/v1/agents" \
  -d "{
    \"name\": \"${AGENT_NAME}\"
  }"
)"
printf '%s\n' "${create_response}"

AGENT_ID="$(printf '%s' "${create_response}" | extract_json_string id)"
if [[ -z "${AGENT_ID}" ]]; then
  echo "Failed to extract agent id from create response." >&2
  exit 1
fi

echo
echo
echo "== Publish version for ${AGENT_ID} =="
json \
  -X POST "${BASE_URL}/v1/agents/${AGENT_ID}/versions" \
  -d "{
    \"model\": \"${MODEL}\",
    \"system_prompt\": \"Review code carefully and explain concrete risks.\",
    \"allowed_tools\": [\"read_file\", \"search_files\", \"patch\"],
    \"max_iterations\": 90,
    \"temperature\": 0.0,
    \"approval_policy\": \"ask\",
    \"timeout_secs\": 300
  }"

echo
echo
echo "== Invoke managed agent =="
json \
  -X POST "${BASE_URL}/v1/chat/completions" \
  -d "{
    \"model\": \"agent:${AGENT_NAME}\",
    \"messages\": [
      {\"role\": \"user\", \"content\": \"${PROMPT}\"}
    ]
  }"

echo
echo
echo "== List runs =="
json "${BASE_URL}/v1/runs"

cat <<EOF

== Optional: streaming plus cancel ==
1. Start a streaming request:

curl -N -sS \\
  -H "Authorization: Bearer \$HERMES_API_KEY" \\
  -H "Content-Type: application/json" \\
  "${BASE_URL}/v1/chat/completions" \\
  -d '{
    "model": "agent:${AGENT_NAME}",
    "stream": true,
    "messages": [{"role": "user", "content": "Do a long, detailed repository review."}]
  }'

2. In another shell, inspect active runs:

curl -sS -H "Authorization: Bearer \$HERMES_API_KEY" "${BASE_URL}/v1/runs"

3. Cancel one run by id:

curl -sS -X DELETE \\
  -H "Authorization: Bearer \$HERMES_API_KEY" \\
  "${BASE_URL}/v1/runs/<run_id>"

Cancellation in managed beta is best-effort, not a hard real-time guarantee.
EOF
