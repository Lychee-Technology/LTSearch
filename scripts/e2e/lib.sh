#!/usr/bin/env bash
set -euo pipefail

readonly LTSEARCH_E2E_MOTO_ENDPOINT="${LTSEARCH_E2E_MOTO_ENDPOINT:-http://localhost:5000}"
readonly LTSEARCH_E2E_AWS_REGION="${LTSEARCH_E2E_AWS_REGION:-us-east-1}"

aws_e2e() {
  AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-test}" \
  AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-test}" \
  AWS_DEFAULT_REGION="$LTSEARCH_E2E_AWS_REGION" \
  aws --endpoint-url "$LTSEARCH_E2E_MOTO_ENDPOINT" "$@"
}

wait_for_moto() {
  local attempts="${1:-30}"
  local i
  for ((i = 1; i <= attempts; i++)); do
    if aws_e2e s3api list-buckets >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "Moto did not become ready at $LTSEARCH_E2E_MOTO_ENDPOINT" >&2
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

receive_one_sqs_batch() {
  local queue_url="$1"
  aws_e2e sqs receive-message \
    --queue-url "$queue_url" \
    --max-number-of-messages 1 \
    --wait-time-seconds 5 \
    --output json
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
