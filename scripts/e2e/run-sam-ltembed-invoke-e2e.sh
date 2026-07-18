#!/usr/bin/env bash
# ltembed ZIP 路径 SAM e2e（#111，S3→/tmp）：real 模式打包函数 ZIP + 模型资产，
# 断言 zip 布局与尺寸预算，把 dist/model-assets/ 上传到 moto S3 前缀，再用派生
# 模板 sam local invoke 走 write→SQS→build→query 全链路——provider/dim 不覆盖，
# 走生产模板默认 ltembed/512（#94），验证冷启动从 S3 下载校验资产到 /tmp/ltembed
# 后真实嵌入可用。
set -euo pipefail

source "$(dirname "$0")/lib.sh"

readonly REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly E2E_FIXTURES_DIR="$REPO_ROOT/tests/fixtures/e2e"
readonly E2E_OUTPUT_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$REPO_ROOT/.e2e-tmp}"
readonly E2E_RUN_ID="${LTSEARCH_E2E_RUN_ID:-$(date +%s)-$$}"
readonly E2E_BUCKET="${LTSEARCH_E2E_BUCKET:-ltsearch-ltembed-e2e-$E2E_RUN_ID}"
readonly E2E_QUEUE_NAME="${LTSEARCH_E2E_QUEUE_NAME:-ltsearch-ltembed-e2e-$E2E_RUN_ID}"
readonly E2E_MODEL_PREFIX="ltembed-assets"

mkdir -p "$E2E_OUTPUT_DIR"

wait_for_moto
create_e2e_bucket "$E2E_BUCKET"
QUEUE_URL="$(create_e2e_queue "$E2E_QUEUE_NAME")"

# real 模式编译 patch 到 /src/.sam-local-deps/LTEmbed，需先 stage checkout。
prepare_local_ltembed_checkout "$REPO_ROOT"

LTSEARCH_LTEMBED_MODE=real bash "$REPO_ROOT/scripts/package-lambda-zips.sh"
bash "$REPO_ROOT/scripts/package-model-assets.sh"

for fn in query_lambda write_lambda index_builder_lambda; do
  assert_zip_layout "$REPO_ROOT/dist/$fn.zip"
done
bash "$REPO_ROOT/scripts/check-lambda-size-budget.sh"

# 模型资产上传到 moto：函数冷启动按 manifest 从这里下载校验到 /tmp/ltembed。
aws_e2e s3 cp --recursive "$REPO_ROOT/dist/model-assets" "s3://$E2E_BUCKET/$E2E_MODEL_PREFIX/" >/dev/null

LTEMBED_E2E_TEMPLATE="$E2E_OUTPUT_DIR/template-ltembed-e2e.yaml"
make_zip_e2e_template "$REPO_ROOT/template.yaml" "$REPO_ROOT" "$LTEMBED_E2E_TEMPLATE"

ENV_VARS_JSON="$E2E_OUTPUT_DIR/ltembed-env-vars.json"
python3 - <<'PY' "$ENV_VARS_JSON" "$E2E_BUCKET" "$QUEUE_URL" "$E2E_MODEL_PREFIX"
import json, sys
env_path, bucket, queue_url, model_prefix = sys.argv[1:5]
moto_endpoint = 'http://moto:5000'
container_queue_url = queue_url.replace('http://localhost:5000', moto_endpoint)
# 只注入 moto endpoint / bucket / queue / artifact root 与模型资产 S3 定位；
# embedding provider、dim 与 /tmp/ltembed 路径沿用生产模板默认值(ltembed/512)。
env = {
    'WriteFunction': {
        'LTSEARCH_WRITE_S3_BUCKET': bucket,
        'LTSEARCH_WRITE_SQS_QUEUE_URL': container_queue_url,
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
        'AWS_ENDPOINT_URL_SQS': moto_endpoint,
    },
    'BuildFunction': {
        'LTSEARCH_BUILD_S3_BUCKET': bucket,
        'LTSEARCH_BUILD_ARTIFACT_ROOT': '/tmp/ltsearch-ltembed-e2e-artifacts',
        'LTSEARCH_BUILD_LTEMBED_S3_BUCKET': bucket,
        'LTSEARCH_BUILD_LTEMBED_S3_PREFIX': model_prefix,
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
    'QueryFunction': {
        'LTSEARCH_QUERY_S3_BUCKET': bucket,
        'LTSEARCH_QUERY_ARTIFACT_ROOT': '/tmp/ltsearch-ltembed-e2e-artifacts',
        'LTSEARCH_QUERY_LTEMBED_S3_BUCKET': bucket,
        'LTSEARCH_QUERY_LTEMBED_S3_PREFIX': model_prefix,
        'AWS_ENDPOINT_URL_S3': moto_endpoint,
    },
}
json.dump(env, open(env_path, 'w'))
PY

WRITE_EVENT_JSON="$E2E_OUTPUT_DIR/ltembed-write-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/write_request.json" /write "$WRITE_EVENT_JSON"
WRITE_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-write-response.json"
sam local invoke WriteFunction \
  --template-file "$LTEMBED_E2E_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --region "$LTSEARCH_E2E_AWS_REGION" \
  --event "$WRITE_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$WRITE_RESPONSE_JSON"
assert_lambda_json_field "$WRITE_RESPONSE_JSON" accepted_count 6

BATCH_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-batch-response.json"
receive_one_sqs_batch "$QUEUE_URL" > "$BATCH_RESPONSE_JSON"
BUILD_EVENT_JSON="$E2E_OUTPUT_DIR/ltembed-build-event.json"
make_sqs_event "$BATCH_RESPONSE_JSON" "$BUILD_EVENT_JSON"

BUILD_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-build-response.json"
sam local invoke BuildFunction \
  --template-file "$LTEMBED_E2E_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --region "$LTSEARCH_E2E_AWS_REGION" \
  --event "$BUILD_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$BUILD_RESPONSE_JSON"
python3 - <<'PY' "$BUILD_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response == {'batchItemFailures': []}, response
PY

QUERY_EVENT_JSON="$E2E_OUTPUT_DIR/ltembed-query-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/query_request.json" /query "$QUERY_EVENT_JSON"
QUERY_RESPONSE_JSON="$E2E_OUTPUT_DIR/ltembed-query-response.json"
sam local invoke QueryFunction \
  --template-file "$LTEMBED_E2E_TEMPLATE" \
  --env-vars "$ENV_VARS_JSON" \
  --region "$LTSEARCH_E2E_AWS_REGION" \
  --event "$QUERY_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$QUERY_RESPONSE_JSON"
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

echo "ltembed ZIP SAM e2e passed" >&2
