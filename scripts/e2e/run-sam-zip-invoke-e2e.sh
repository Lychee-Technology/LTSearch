#!/usr/bin/env bash
# ZIP 路径 SAM e2e（#109 AC-1/AC-5）：package-lambda-zips.sh（stub 模式）产出
# dist/，断言 zip 布局，再用生产 template.yaml 直接 sam local invoke（CodeUri
# 指向 dist/<fn>/ 目录，无需 sam build），走 write→SQS→build→query 全链路。
set -euo pipefail

source "$(dirname "$0")/lib.sh"

readonly REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly E2E_FIXTURES_DIR="$REPO_ROOT/tests/fixtures/e2e"
readonly E2E_OUTPUT_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$REPO_ROOT/.e2e-tmp}"
readonly E2E_RUN_ID="${LTSEARCH_E2E_RUN_ID:-$(date +%s)-$$}"
readonly E2E_BUCKET="${LTSEARCH_E2E_BUCKET:-ltsearch-zip-e2e-$E2E_RUN_ID}"
readonly E2E_QUEUE_NAME="${LTSEARCH_E2E_QUEUE_NAME:-ltsearch-zip-e2e-$E2E_RUN_ID}"

mkdir -p "$E2E_OUTPUT_DIR"

wait_for_moto
create_e2e_bucket "$E2E_BUCKET"
QUEUE_URL="$(create_e2e_queue "$E2E_QUEUE_NAME")"

LTSEARCH_LTEMBED_MODE=stub bash "$REPO_ROOT/scripts/package-lambda-zips.sh"

for fn in query_lambda write_lambda index_builder_lambda; do
  assert_zip_layout "$REPO_ROOT/dist/$fn.zip"
done

ZIP_E2E_TEMPLATE="$E2E_OUTPUT_DIR/template-zip-e2e.yaml"
make_zip_e2e_template "$REPO_ROOT/template.yaml" "$REPO_ROOT" "$ZIP_E2E_TEMPLATE"

ENV_VARS_JSON="$E2E_OUTPUT_DIR/zip-env-vars.json"
python3 - <<'PY' "$ENV_VARS_JSON" "$E2E_BUCKET" "$QUEUE_URL"
import json, sys
env_path, bucket, queue_url = sys.argv[1:4]
moto_endpoint = 'http://moto:5000'
container_queue_url = queue_url.replace('http://localhost:5000', moto_endpoint)
env = {
    'WriteFunction': {
        'LTSEARCH_WRITE_S3_BUCKET': bucket,
        'LTSEARCH_WRITE_SQS_QUEUE_URL': container_queue_url,
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
        'AWS_ENDPOINT_URL_SQS': moto_endpoint,
    },
    'BuildFunction': {
        'LTSEARCH_BUILD_S3_BUCKET': bucket,
        'LTSEARCH_BUILD_ARTIFACT_ROOT': '/tmp/ltsearch-zip-e2e-artifacts',
        'LTSEARCH_BUILD_EMBEDDING_PROVIDER': 'fixed',
        'LTSEARCH_BUILD_FIXED_EMBEDDING': '0.9,0.1,0.0',
        'LTSEARCH_BUILD_EMBEDDING_DIM': '3',
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
    'QueryFunction': {
        'LTSEARCH_QUERY_S3_BUCKET': bucket,
        'LTSEARCH_QUERY_ARTIFACT_ROOT': '/tmp/ltsearch-zip-e2e-artifacts',
        'LTSEARCH_QUERY_EMBEDDING_PROVIDER': 'fixed',
        'LTSEARCH_QUERY_FIXED_EMBEDDING': '0.9,0.1,0.0',
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
}
json.dump(env, open(env_path, 'w'))
PY

WRITE_EVENT_JSON="$E2E_OUTPUT_DIR/zip-write-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/write_request.json" /write "$WRITE_EVENT_JSON"
WRITE_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-write-response.json"
sam local invoke WriteFunction \
  --template-file "$ZIP_E2E_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$WRITE_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$WRITE_RESPONSE_JSON"
assert_lambda_json_field "$WRITE_RESPONSE_JSON" accepted_count 6

BATCH_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-batch-response.json"
receive_one_sqs_batch "$QUEUE_URL" > "$BATCH_RESPONSE_JSON"
BUILD_EVENT_JSON="$E2E_OUTPUT_DIR/zip-build-event.json"
make_sqs_event "$BATCH_RESPONSE_JSON" "$BUILD_EVENT_JSON"

BUILD_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-build-response.json"
sam local invoke BuildFunction \
  --template-file "$ZIP_E2E_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$BUILD_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$BUILD_RESPONSE_JSON"
python3 - <<'PY' "$BUILD_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response == {'batchItemFailures': []}, response
PY

QUERY_EVENT_JSON="$E2E_OUTPUT_DIR/zip-query-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/query_request.json" /query "$QUERY_EVENT_JSON"
QUERY_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-query-response.json"
sam local invoke QueryFunction \
  --template-file "$ZIP_E2E_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$QUERY_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$QUERY_RESPONSE_JSON"
python3 - <<'PY' "$QUERY_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response['statusCode'] == 200, response
body = json.loads(response['body'])
assert body['index_version'] == 1, body
doc_ids = [item['doc_id'] for item in body['dynamic_chunks']]
assert 'doc-rust-hybrid' in doc_ids, body
PY

echo "ZIP SAM e2e passed" >&2
