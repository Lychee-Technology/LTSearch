#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "$0")/lib.sh"

readonly REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly E2E_OUTPUT_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$REPO_ROOT/.e2e-tmp}"
readonly E2E_RUN_ID="${LTSEARCH_E2E_RUN_ID:-$(date +%s)-$$}"
readonly E2E_BUCKET="${LTSEARCH_E2E_BUCKET:-ltsearch-e2e-$E2E_RUN_ID}"
readonly E2E_QUEUE_NAME="${LTSEARCH_E2E_QUEUE_NAME:-ltsearch-e2e-$E2E_RUN_ID}"
readonly SAM_SOURCE_TEMPLATE="$REPO_ROOT/template.sam-e2e.yaml"
readonly SAM_BUILT_TEMPLATE="$REPO_ROOT/.aws-sam/build/template.yaml"
readonly SAM_API_PID_FILE="$E2E_OUTPUT_DIR/sam-api.pid"
readonly SAM_API_LOG="$E2E_OUTPUT_DIR/sam-api.log"

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
run_with_heartbeat "sam build" "$SAM_BUILD_LOG" "$SAM_BUILD_DOCKER_EVENTS_LOG" \
  sam build --debug --template-file "$SAM_SOURCE_TEMPLATE"

moto_endpoint='http://moto:5000'
container_queue_url="${QUEUE_URL/http:\/\/localhost:5000/$moto_endpoint}"

ENV_VARS_JSON="$E2E_OUTPUT_DIR/env-vars.json"
python3 - <<'PY' "$ENV_VARS_JSON" "$E2E_BUCKET" "$container_queue_url" "$moto_endpoint"
import json, sys
env_path, bucket, queue_url, moto = sys.argv[1:5]
env = {
    'WriteFunction': {
        'LTSEARCH_WRITE_S3_BUCKET': bucket,
        'LTSEARCH_WRITE_SQS_QUEUE_URL': queue_url,
        'AWS_ENDPOINT_URL_S3': moto,
        'AWS_ENDPOINT_URL_SQS': moto,
    },
    'BuildFunction': {
        'LTSEARCH_BUILD_S3_BUCKET': bucket,
        'LTSEARCH_BUILD_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts',
        'LTSEARCH_BUILD_EMBEDDING_DIM': '3',
        'AWS_ENDPOINT_URL_S3': moto,
    },
    'QueryFunction': {
        'LTSEARCH_QUERY_ARTIFACT_ROOT': '/tmp/ltsearch-e2e-artifacts',
        'LTSEARCH_QUERY_S3_BUCKET': bucket,
        'AWS_ENDPOINT_URL_S3': moto,
    },
}
json.dump(env, open(env_path, 'w'))
PY

SAM_API_LOG_TMP="$E2E_OUTPUT_DIR/sam-api-start.log"
sam local start-api \
  --template-file "$SAM_BUILT_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --docker-network ltsearch-e2e \
  >"$SAM_API_LOG" 2>&1 &
echo $! > "$SAM_API_PID_FILE"

echo "SAM API starting (PID $(cat "$SAM_API_PID_FILE"))..." >&2
for i in $(seq 1 30); do
  if curl -sf http://localhost:3000 >/dev/null 2>&1 \
       || curl -sf http://localhost:3000/write >/dev/null 2>&1 \
       || curl -sf http://localhost:3000/query >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

python3 - <<'PY' "$E2E_OUTPUT_DIR" "$E2E_BUCKET" "$QUEUE_URL"
import json, pathlib, sys
output_dir, bucket, queue_url = sys.argv[1:4]
state = {'bucket': bucket, 'queue_url': queue_url}
(pathlib.Path(output_dir) / 'e2e-state.json').write_text(json.dumps(state))
PY

echo "SAM API ready at http://localhost:3000" >&2
echo "  POST /write  — WriteFunction" >&2
echo "  POST /query  — QueryFunction" >&2
echo "  BuildFunction: sam local invoke BuildFunction" >&2
