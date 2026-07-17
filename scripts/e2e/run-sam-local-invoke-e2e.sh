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

BUILDER_LOG="$E2E_OUTPUT_DIR/ltsearch-builder.log"
BUILDER_DOCKER_EVENTS_LOG="$E2E_OUTPUT_DIR/ltsearch-builder-docker-events.log"
run_with_heartbeat "docker build ltsearch-e2e-builder" "$BUILDER_LOG" "$BUILDER_DOCKER_EVENTS_LOG" \
  env DOCKER_BUILDKIT=1 docker build \
    --platform linux/arm64 \
    --tag ltsearch-e2e-builder \
    --file "$REPO_ROOT/sam/builder.Dockerfile" \
    "$REPO_ROOT"

SAM_BUILD_LOG="$E2E_OUTPUT_DIR/sam-build.log"
SAM_BUILD_DOCKER_EVENTS_LOG="$E2E_OUTPUT_DIR/sam-build-docker-events.log"
run_with_heartbeat "sam build" "$SAM_BUILD_LOG" "$SAM_BUILD_DOCKER_EVENTS_LOG" sam build --debug --template-file "$SAM_SOURCE_TEMPLATE"

ENV_VARS_JSON="$E2E_OUTPUT_DIR/env-vars.json"
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
        'LTSEARCH_BUILD_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts',
        'LTSEARCH_BUILD_EMBEDDING_DIM': '3',
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
    'QueryFunction': {
        'LTSEARCH_QUERY_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts',
        'LTSEARCH_QUERY_S3_BUCKET': bucket,
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
}
json.dump(env, open(env_path, 'w'))
PY

WRITE_EVENT_JSON="$E2E_OUTPUT_DIR/write-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/write_request.json" /write "$WRITE_EVENT_JSON"

WRITE_RESPONSE_JSON="$E2E_OUTPUT_DIR/write-response.json"
sam local invoke WriteFunction \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$WRITE_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$WRITE_RESPONSE_JSON"

BATCH_RESPONSE_JSON="$E2E_OUTPUT_DIR/batch-response.json"
receive_one_sqs_batch "$QUEUE_URL" > "$BATCH_RESPONSE_JSON"

make_sqs_event "$BATCH_RESPONSE_JSON" "$E2E_OUTPUT_DIR/build-event.json"

BUILD_RESPONSE_JSON="$E2E_OUTPUT_DIR/build-response.json"
sam local invoke BuildFunction \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$E2E_OUTPUT_DIR/build-event.json" \
  --docker-network ltsearch-e2e \
  > "$BUILD_RESPONSE_JSON"

QUERY_EVENT_JSON="$E2E_OUTPUT_DIR/query-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/query_request.json" /query "$QUERY_EVENT_JSON"

QUERY_RESPONSE_JSON="$E2E_OUTPUT_DIR/query-response.json"
sam local invoke QueryFunction \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$QUERY_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$QUERY_RESPONSE_JSON"

assert_lambda_json_field "$WRITE_RESPONSE_JSON" accepted_count 6

python3 - <<'PY' "$BUILD_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response == {'batchItemFailures': []}, response
PY

python3 - <<'PY' "$QUERY_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response['statusCode'] == 200, response
body = json.loads(response['body'])
assert body['index_version'] == 1, body
assert body['dynamic_count'] >= 1, body
doc_ids = [item['doc_id'] for item in body['dynamic_chunks']]
assert 'doc-rust-hybrid' in doc_ids, body
PY

if [[ "${LTSEARCH_E2E_LTEMBED:-}" == "true" ]]; then
  echo "--- LTEmbed E2E ---" >&2

  # Defaults to the public minimal-ort-builder release asset; override to test a
  # different bundle. Empty is only possible if explicitly set to "", which we reject.
  LTEMBED_BUNDLE_URL="${LTSEARCH_E2E_LTEMBED_BUNDLE_URL-https://github.com/Lychee-Technology/minimal-ort-builder/releases/download/v1.0.9/jinaai__jina-embeddings-v5-text-nano-retrieval_q4f16_linux-arm64.tar.gz}"
  if [[ -z "$LTEMBED_BUNDLE_URL" ]]; then
    echo "LTSEARCH_E2E_LTEMBED_BUNDLE_URL was set to an empty value" >&2
    exit 1
  fi

  # The real-mode build patches ltembed to /src/.sam-local-deps/LTEmbed, so the
  # checkout must be staged into the Docker context first.
  prepare_local_ltembed_checkout "$REPO_ROOT"

  LTEMBED_BUILDER_LOG="$E2E_OUTPUT_DIR/ltsearch-builder-ltembed.log"
  LTEMBED_BUILDER_DOCKER_EVENTS_LOG="$E2E_OUTPUT_DIR/ltsearch-builder-ltembed-docker-events.log"
  run_with_heartbeat "docker build ltsearch-e2e-builder (ltembed)" "$LTEMBED_BUILDER_LOG" "$LTEMBED_BUILDER_DOCKER_EVENTS_LOG" \
    env DOCKER_BUILDKIT=1 docker build \
      --platform linux/arm64 \
      --build-arg LTEMBED_MODE=real \
      --build-arg "LTEMBED_BUNDLE_URL=${LTEMBED_BUNDLE_URL}" \
      --tag ltsearch-e2e-builder \
      --file "$REPO_ROOT/sam/builder.Dockerfile" \
      "$REPO_ROOT"

  LTEMBED_SAM_BUILD_LOG="$E2E_OUTPUT_DIR/sam-build-ltembed.log"
  LTEMBED_SAM_BUILD_DOCKER_EVENTS_LOG="$E2E_OUTPUT_DIR/sam-build-ltembed-docker-events.log"
  run_with_heartbeat "sam build (ltembed)" "$LTEMBED_SAM_BUILD_LOG" "$LTEMBED_SAM_BUILD_DOCKER_EVENTS_LOG" \
    sam build --debug --template-file "$SAM_SOURCE_TEMPLATE"

  ENV_VARS_LTEMBED_JSON="$E2E_OUTPUT_DIR/env-vars-ltembed.json"
  python3 - <<'PY' "$ENV_VARS_LTEMBED_JSON" "$E2E_BUCKET" "$QUEUE_URL"
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
        'LTSEARCH_BUILD_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts-ltembed',
        'LTSEARCH_BUILD_EMBEDDING_PROVIDER': 'ltembed',
        'LTSEARCH_BUILD_EMBEDDING_DIM': '512',
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
    'QueryFunction': {
        'LTSEARCH_QUERY_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts-ltembed',
        'LTSEARCH_QUERY_S3_BUCKET': bucket,
        'LTSEARCH_QUERY_EMBEDDING_PROVIDER': 'ltembed',
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
}
json.dump(env, open(env_path, 'w'))
PY

  LTEMBED_WRITE_EVENT_JSON="$E2E_OUTPUT_DIR/ltembed-write-event.json"
  make_apigw_event "$E2E_FIXTURES_DIR/write_request.json" /write "$LTEMBED_WRITE_EVENT_JSON"

  LTEMBED_WRITE_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-write-response.json"
  sam local invoke WriteFunction \
    --template-file "$SAM_BUILT_TEMPLATE" \
    --env-vars "$ENV_VARS_LTEMBED_JSON" \
    --event "$LTEMBED_WRITE_EVENT_JSON" \
    --docker-network ltsearch-e2e \
    > "$LTEMBED_WRITE_RESPONSE_JSON"

  LTEMBED_BATCH_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-batch-response.json"
  receive_one_sqs_batch "$QUEUE_URL" > "$LTEMBED_BATCH_RESPONSE_JSON"

  # The fixed-provider run above already activated version 1 in the shared
  # bucket; the builder allocates the next version from _head, so this run
  # publishes version 2 without a hand-authored version id.
  make_sqs_event "$LTEMBED_BATCH_RESPONSE_JSON" "$E2E_OUTPUT_DIR/ltembed-build-event.json"

  LTEMBED_BUILD_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-build-response.json"
  sam local invoke BuildFunction \
    --template-file "$SAM_BUILT_TEMPLATE" \
    --env-vars "$ENV_VARS_LTEMBED_JSON" \
    --event "$E2E_OUTPUT_DIR/ltembed-build-event.json" \
    --docker-network ltsearch-e2e \
    > "$LTEMBED_BUILD_RESPONSE_JSON"

  LTEMBED_QUERY_EVENT_JSON="$E2E_OUTPUT_DIR/ltembed-query-event.json"
  make_apigw_event "$E2E_FIXTURES_DIR/query_request.json" /query "$LTEMBED_QUERY_EVENT_JSON"

  LTEMBED_QUERY_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-query-response.json"
  sam local invoke QueryFunction \
    --template-file "$SAM_BUILT_TEMPLATE" \
    --env-vars "$ENV_VARS_LTEMBED_JSON" \
    --event "$LTEMBED_QUERY_EVENT_JSON" \
    --docker-network ltsearch-e2e \
    > "$LTEMBED_QUERY_RESPONSE_JSON"

  assert_lambda_json_field "$LTEMBED_WRITE_RESPONSE_JSON" accepted_count 6

  python3 - <<'PY' "$LTEMBED_BUILD_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response == {'batchItemFailures': []}, response
PY

  python3 - <<'PY' "$LTEMBED_QUERY_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response['statusCode'] == 200, response
body = json.loads(response['body'])
assert body['index_version'] == 2, body
assert body['dynamic_count'] >= 1, body
doc_ids = [item['doc_id'] for item in body['dynamic_chunks']]
assert 'doc-rust-hybrid' in doc_ids, body
print(f"LTEmbed query OK — index_version={body['index_version']}, dynamic_count={body['dynamic_count']}, doc_ids={doc_ids[:3]}...")
PY

  echo "LTEmbed E2E complete." >&2
fi
