#!/usr/bin/env bash
set -euo pipefail

readonly LTSEARCH_E2E_MOTO_ENDPOINT="${LTSEARCH_E2E_MOTO_ENDPOINT:-http://localhost:5000}"
readonly LTSEARCH_E2E_AWS_REGION="${LTSEARCH_E2E_AWS_REGION:-us-east-1}"
readonly LTSEARCH_E2E_HEARTBEAT_SECONDS="${LTSEARCH_E2E_HEARTBEAT_SECONDS:-20}"

tail_log_snapshot() {
  local log_file="$1"
  if [[ -f "$log_file" ]]; then
    echo "--- recent log: $log_file ---"
    python3 - <<'PY' "$log_file"
import pathlib, sys
path = pathlib.Path(sys.argv[1])
lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
for line in lines[-20:]:
    print(line)
PY
    echo "--- end log: $log_file ---"
  fi
}

start_docker_events_capture() {
  local docker_events_log="$1"
  if ! command -v docker >/dev/null 2>&1; then
    return 1
  fi

  : > "$docker_events_log"
  docker events --since 0s > "$docker_events_log" 2>&1 &
  echo $!
}

stop_docker_events_capture() {
  local docker_events_pid="$1"
  if [[ -n "$docker_events_pid" ]] && kill -0 "$docker_events_pid" >/dev/null 2>&1; then
    kill "$docker_events_pid" >/dev/null 2>&1 || true
    wait "$docker_events_pid" >/dev/null 2>&1 || true
  fi
}

run_with_heartbeat() {
  local label="$1"
  local log_file="$2"
  local docker_events_log="$3"
  shift
  shift
  shift

  local docker_events_pid=""
  if command -v docker >/dev/null 2>&1; then
    docker_events_pid="$(start_docker_events_capture "$docker_events_log")"
  fi

  "$@" 2>&1 | tee "$log_file" &
  local command_pid=$!

  while kill -0 "$command_pid" >/dev/null 2>&1; do
    sleep "$LTSEARCH_E2E_HEARTBEAT_SECONDS"
    if kill -0 "$command_pid" >/dev/null 2>&1; then
      echo "$label still running..."
      tail_log_snapshot "$log_file"
      tail_log_snapshot "$docker_events_log"
    fi
  done

  stop_docker_events_capture "$docker_events_pid"
  wait "$command_pid"
}

aws_e2e() {
  if ! command -v aws >/dev/null 2>&1; then
    echo "aws CLI is required for SAM E2E helpers but was not found on PATH" >&2
    return 127
  fi

  AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-test}" \
  AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-test}" \
  AWS_DEFAULT_REGION="$LTSEARCH_E2E_AWS_REGION" \
  aws --endpoint-url "$LTSEARCH_E2E_MOTO_ENDPOINT" "$@"
}

wait_for_moto() {
  local attempts="${1:-90}"
  local i
  for ((i = 1; i <= attempts; i++)); do
    if python3 - <<'PY' "$LTSEARCH_E2E_MOTO_ENDPOINT" >/dev/null 2>&1
import sys, urllib.request
endpoint = sys.argv[1].rstrip('/') + '/'
with urllib.request.urlopen(endpoint, timeout=2) as response:
    if response.status >= 200 and response.status < 500:
        raise SystemExit(0)
raise SystemExit(1)
PY
    then
      if aws_e2e s3api list-buckets >/dev/null 2>&1; then
        return 0
      fi
    fi
    sleep 1
  done

  if command -v docker >/dev/null 2>&1; then
    echo "=== docker compose ps ===" >&2
    docker compose -f docker-compose.moto.yml ps >&2 || true
    echo "=== moto logs ===" >&2
    docker compose -f docker-compose.moto.yml logs moto >&2 || true
  fi

  echo "Moto did not become ready at $LTSEARCH_E2E_MOTO_ENDPOINT after ${attempts}s" >&2
  return 1
}

create_e2e_bucket() {
  local bucket="$1"
  if ! aws_e2e s3api head-bucket --bucket "$bucket" >/dev/null 2>&1; then
    aws_e2e s3api create-bucket --bucket "$bucket" >/dev/null
  fi
}

create_e2e_queue() {
  local queue_name="$1"
  aws_e2e sqs create-queue --queue-name "$queue_name" --output text --query 'QueueUrl'
}

prepare_local_ltembed_checkout() {
  local repo_root="$1"
  local configured_checkout="${LTSEARCH_LTEMBED_CHECKOUT:-}"
  local cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  local common_git_dir
  common_git_dir="$(git -C "$repo_root" rev-parse --path-format=absolute --git-common-dir)"
  local shared_repo_root
  shared_repo_root="$(dirname "$common_git_dir")"
  local sibling_checkout
  sibling_checkout="$(dirname "$shared_repo_root")/LTEmbed"
  local nested_checkout
  nested_checkout="$repo_root/LTEmbed"
  local cargo_checkout=""
  local vendor_root="$repo_root/.sam-local-deps/LTEmbed"

  cargo fetch --locked >/dev/null

  if [[ -d "$cargo_home/git/checkouts" ]]; then
    cargo_checkout="$(find "$cargo_home/git/checkouts" -maxdepth 2 -mindepth 2 -type f -name Cargo.toml -path '*/ltembed-*/*' 2>/dev/null | head -n 1 | xargs -I{} dirname '{}')"
  fi

  local source_checkout=""
  if [[ -n "$configured_checkout" && -f "$configured_checkout/Cargo.toml" ]]; then
    source_checkout="$configured_checkout"
  elif [[ -f "$nested_checkout/Cargo.toml" ]]; then
    source_checkout="$nested_checkout"
  elif [[ -f "$sibling_checkout/Cargo.toml" ]]; then
    source_checkout="$sibling_checkout"
  elif [[ -n "$cargo_checkout" && -f "$cargo_checkout/Cargo.toml" ]]; then
    source_checkout="$cargo_checkout"
  fi

  if [[ -z "$source_checkout" ]]; then
    echo "Missing LTEmbed checkout. Looked at: ${configured_checkout:-<unset>}, $nested_checkout, $sibling_checkout, ${cargo_checkout:-<cargo-cache-miss>}" >&2
    return 1
  fi

  python3 - <<'PY' "$source_checkout" "$vendor_root"
import pathlib, shutil, sys
src = pathlib.Path(sys.argv[1])
dst = pathlib.Path(sys.argv[2])
if dst.exists():
    shutil.rmtree(dst)
shutil.copytree(src, dst, ignore=shutil.ignore_patterns('.git', 'target'))
PY
}

receive_one_sqs_batch() {
  local queue_url="$1"
  aws_e2e sqs receive-message \
    --queue-url "$queue_url" \
    --max-number-of-messages 1 \
    --wait-time-seconds 5 \
    --output json
}

sync_e2e_artifacts_from_moto() {
  local bucket="$1"
  local destination="$2"
  rm -rf "$destination"
  mkdir -p "$destination"
  aws_e2e s3 cp "s3://$bucket/index" "$destination/index" --recursive >/dev/null
  aws_e2e s3 cp "s3://$bucket/lance" "$destination/lance" --recursive >/dev/null
}

assert_json_field() {
  local json_file="$1"
  local jq_filter="$2"
  local expected="$3"
  local actual
  actual=$(python3 -c 'import json,sys
obj=json.load(open(sys.argv[1]))
path=sys.argv[2].split(".")
cur=obj
for part in path:
    if part:
        cur=cur[part]
print(cur)' "$json_file" "$jq_filter")

  if [[ "$actual" != "$expected" ]]; then
    echo "Expected $jq_filter=$expected but got $actual" >&2
    return 1
  fi
}

# 把裸请求体包成 API Gateway HTTP API payload v2 信封事件文件。
# 用法: make_apigw_event <body-json-file> <raw-path> <out-file>
make_apigw_event() {
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
body_path, raw_path, out_path = sys.argv[1:4]
event = {
    'version': '2.0',
    'routeKey': f'POST {raw_path}',
    'rawPath': raw_path,
    'requestContext': {'http': {'method': 'POST', 'path': raw_path}},
    'isBase64Encoded': False,
    'body': open(body_path).read(),
}
json.dump(event, open(out_path, 'w'))
PY
}

# 把 `aws sqs receive-message` 的响应包成 Lambda SQS 触发事件文件。
# 用法: make_sqs_event <receive-message-response-file> <out-file>
make_sqs_event() {
  python3 - "$1" "$2" <<'PY'
import json, sys
response = json.load(open(sys.argv[1]))
messages = response.get('Messages', [])
if not messages:
    raise SystemExit('expected one SQS batch message')
event = {'Records': [{
    'messageId': messages[0].get('MessageId', 'e2e-message-1'),
    'body': messages[0]['Body'],
    'eventSource': 'aws:sqs',
}]}
json.dump(event, open(sys.argv[2], 'w'))
PY
}

# 断言 APIGW v2 信封响应: statusCode==200 且 body 内字段等于期望值。
# 用法: assert_lambda_json_field <response-file> <field> <expected>
assert_lambda_json_field() {
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
path, field, expected = sys.argv[1:4]
response = json.load(open(path))
assert response.get('statusCode') == 200, f'non-200 lambda response: {response}'
body = json.loads(response['body'])
actual = str(body.get(field))
assert actual == expected, f'{field}: expected {expected}, got {actual} in {body}'
PY
}
