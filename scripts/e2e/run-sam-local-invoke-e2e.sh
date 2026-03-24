#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "$0")/lib.sh"

readonly REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly E2E_FIXTURES_DIR="$REPO_ROOT/tests/fixtures/e2e"
readonly E2E_OUTPUT_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$REPO_ROOT/.e2e-tmp}"
readonly E2E_RUN_ID="${LTSEARCH_E2E_RUN_ID:-$(date +%s)-$$}"
readonly E2E_BUCKET="${LTSEARCH_E2E_BUCKET:-ltsearch-e2e-$E2E_RUN_ID}"
readonly E2E_QUEUE_NAME="${LTSEARCH_E2E_QUEUE_NAME:-ltsearch-e2e-$E2E_RUN_ID}"
readonly SAM_SOURCE_TEMPLATE="$REPO_ROOT/template.sam-e2e.yaml"
readonly SAM_BUILT_TEMPLATE="$REPO_ROOT/.aws-sam/build/template.yaml"

mkdir -p "$E2E_OUTPUT_DIR"

wait_for_moto
create_e2e_bucket "$E2E_BUCKET"
QUEUE_URL="$(create_e2e_queue "$E2E_QUEUE_NAME")"

run_with_heartbeat "sam build" sam build --template-file "$SAM_SOURCE_TEMPLATE"

ENV_VARS_JSON="$E2E_OUTPUT_DIR/env-vars.json"
python3 - <<'PY' "$ENV_VARS_JSON" "$E2E_BUCKET" "$QUEUE_URL"
import json, sys
env_path, bucket, queue_url = sys.argv[1:4]
env = {
    'WriteFunction': {
        'LTSEARCH_WRITE_S3_BUCKET': bucket,
        'LTSEARCH_WRITE_SQS_QUEUE_URL': queue_url,
        'AWS_ENDPOINT_URL_S3': 'http://host.docker.internal:5000',
        'AWS_ENDPOINT_URL_SQS': 'http://host.docker.internal:5000',
    },
    'BuildFunction': {
        'LTSEARCH_BUILD_S3_BUCKET': bucket,
        'LTSEARCH_BUILD_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts',
        'AWS_ENDPOINT_URL_S3': 'http://host.docker.internal:5000',
    },
    'QueryFunction': {
        'LTSEARCH_QUERY_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts',
        'LTSEARCH_QUERY_S3_BUCKET': bucket,
        'AWS_ENDPOINT_URL_S3': 'http://host.docker.internal:5000',
    },
}
json.dump(env, open(env_path, 'w'))
PY

WRITE_RESPONSE_JSON="$E2E_OUTPUT_DIR/write-response.json"
sam local invoke WriteFunction \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$E2E_FIXTURES_DIR/write_request.json" \
  > "$WRITE_RESPONSE_JSON"

BATCH_RESPONSE_JSON="$E2E_OUTPUT_DIR/batch-response.json"
receive_one_sqs_batch "$QUEUE_URL" > "$BATCH_RESPONSE_JSON"

python3 - <<'PY' "$BATCH_RESPONSE_JSON" "$E2E_OUTPUT_DIR/build-event.json"
import json, sys
response = json.load(open(sys.argv[1]))
messages = response.get('Messages', [])
if not messages:
    raise SystemExit('expected one SQS batch message')
body = json.loads(messages[0]['Body'])
event = {
    'batch_id': body['batch_id'],
    'wal_key': body['wal_key'],
    'version_id': 1,
    'embedding_dim': 3,
}
json.dump(event, open(sys.argv[2], 'w'))
PY

BUILD_RESPONSE_JSON="$E2E_OUTPUT_DIR/build-response.json"
sam local invoke BuildFunction \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$E2E_OUTPUT_DIR/build-event.json" \
  > "$BUILD_RESPONSE_JSON"

QUERY_RESPONSE_JSON="$E2E_OUTPUT_DIR/query-response.json"
sam local invoke QueryFunction \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$E2E_FIXTURES_DIR/query_request.json" \
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
PY
