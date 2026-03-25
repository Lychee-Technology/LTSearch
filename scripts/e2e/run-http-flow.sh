#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "$0")/lib.sh"

readonly REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly E2E_FIXTURES_DIR="$REPO_ROOT/tests/fixtures/e2e"
readonly E2E_OUTPUT_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$REPO_ROOT/.e2e-tmp}"
readonly SAM_BUILT_TEMPLATE="$REPO_ROOT/.aws-sam/build/template.yaml"
readonly SAM_API_BASE="${LTSEARCH_E2E_SAM_API_BASE:-http://localhost:3000}"

readonly STATE_FILE="$E2E_OUTPUT_DIR/e2e-state.json"
QUEUE_URL="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["queue_url"])' "$STATE_FILE")"
E2E_BUCKET="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["bucket"])' "$STATE_FILE")"

ENV_VARS_JSON="$E2E_OUTPUT_DIR/env-vars.json"

echo "--- POST /write ---" >&2
WRITE_RESPONSE_JSON="$E2E_OUTPUT_DIR/write-response.json"
curl -sf -X POST "$SAM_API_BASE/write" \
  -H "Content-Type: application/json" \
  -d @"$E2E_FIXTURES_DIR/write_request.json" \
  > "$WRITE_RESPONSE_JSON"

BATCH_RESPONSE_JSON="$E2E_OUTPUT_DIR/batch-response.json"
receive_one_sqs_batch "$QUEUE_URL" > "$BATCH_RESPONSE_JSON"

python3 - <<'PY' "$BATCH_RESPONSE_JSON" "$E2E_OUTPUT_DIR/build-event.json"
import json, sys
response = json.load(open(sys.argv[1]))
messages = response.get('Messages', [])
if not messages:
    raise SystemExit('expected one SQS batch message after POST /write')
body = json.loads(messages[0]['Body'])
event = {
    'batch_id': body['batch_id'],
    'wal_key': body['wal_key'],
    'version_id': 1,
    'embedding_dim': 3,
}
json.dump(event, open(sys.argv[2], 'w'))
PY

echo "--- invoke BuildFunction ---" >&2
BUILD_RESPONSE_JSON="$E2E_OUTPUT_DIR/build-response.json"
sam local invoke BuildFunction \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$E2E_OUTPUT_DIR/build-event.json" \
  --docker-network ltsearch-e2e \
  > "$BUILD_RESPONSE_JSON"

echo "--- POST /query ---" >&2
QUERY_RESPONSE_JSON="$E2E_OUTPUT_DIR/query-response.json"
curl -sf -X POST "$SAM_API_BASE/query" \
  -H "Content-Type: application/json" \
  -d @"$E2E_FIXTURES_DIR/query_request.json" \
  > "$QUERY_RESPONSE_JSON"

assert_json_field "$WRITE_RESPONSE_JSON" accepted_count 6
assert_json_field "$BUILD_RESPONSE_JSON" activated_version_id 1

python3 - <<'PY' "$QUERY_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response['index_version'] == 1, response
assert response['total_count'] >= 1, response
doc_ids = [item['doc_id'] for item in response['results']]
assert 'doc-rust-hybrid' in doc_ids, response
print(f"query OK — index_version={response['index_version']}, total_count={response['total_count']}, doc_ids={doc_ids[:3]}...")
PY

echo "HTTP flow complete." >&2
