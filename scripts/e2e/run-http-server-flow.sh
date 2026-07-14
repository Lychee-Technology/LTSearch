#!/usr/bin/env bash
set -euo pipefail
# HTTP 服务模式全自动冒烟：write→(index-builder SQS 轮询自动 build)→query。
# 前置：三个 :dev 镜像已构建；`docker compose -f docker-compose.http.yml up -d --wait`
# 已就绪（含 moto、aws-init 建桶建队列、三个服务 healthcheck 通过）。
# 与 run-http-flow.sh 的区别：不再手动搬运 SQS 消息，builder 后台轮询自动消费。
BASE_WRITE="${LTSEARCH_E2E_WRITE_BASE:-http://localhost:18081}"
BASE_QUERY="${LTSEARCH_E2E_QUERY_BASE:-http://localhost:18080}"
FIXTURES="$(cd "$(dirname "$0")/../.." && pwd)/tests/fixtures/e2e"

echo "--- POST /write ---" >&2
curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request.json" | tee /tmp/write-resp.json
echo >&2
python3 -c 'import json;r=json.load(open("/tmp/write-resp.json"));assert r["accepted_count"]==6,r'

echo "--- 等 index-builder 轮询消费并发布 v1（上限 120s）---" >&2
VERSION=0
for i in $(seq 1 60); do
  VERSION=$(curl -sf "$BASE_QUERY/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("index_version") or 0)')
  [ "$VERSION" -ge 1 ] && break
  sleep 2
done
[ "$VERSION" -ge 1 ] || { echo "index never became active" >&2; exit 1; }
echo "query /health 报告 index_version=$VERSION" >&2

echo "--- POST /query ---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | tee /tmp/query-resp.json
echo >&2
python3 - <<'PY'
import json
r = json.load(open("/tmp/query-resp.json"))
assert r["index_version"] >= 1, r
assert r["dynamic_count"] >= 1, r
assert "doc-rust-hybrid" in [c["doc_id"] for c in r["dynamic_chunks"]], r
print("HTTP server flow OK:", r["index_version"], r["dynamic_count"])
PY
