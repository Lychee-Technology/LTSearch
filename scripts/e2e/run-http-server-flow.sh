#!/usr/bin/env bash
set -euo pipefail
# HTTP 服务模式全自动冒烟：write→(index-builder SQS 轮询自动 build)→query，
# 再追加第二次 write 验证多写快照（两批文档在新版本中均可检索，PR #105 P1 回归）。
# 前置：三个 :dev 镜像已构建；`docker compose -f docker-compose.http.yml up -d --wait`
# 已就绪（含 moto、aws-init 建桶建队列、三个服务 healthcheck 通过）。
# 与 run-http-flow.sh 的区别：不再手动搬运 SQS 消息，builder 后台轮询自动消费。
BASE_WRITE="${LTSEARCH_E2E_WRITE_BASE:-http://localhost:18081}"
BASE_QUERY="${LTSEARCH_E2E_QUERY_BASE:-http://localhost:18080}"
FIXTURES="$(cd "$(dirname "$0")/../.." && pwd)/tests/fixtures/e2e"

# 轮询 query /health 直到 index_version >= $1（上限 120s），把版本号写入 VERSION。
wait_for_index_version() {
  local target="$1"
  VERSION=0
  for _ in $(seq 1 60); do
    VERSION=$(curl -sf "$BASE_QUERY/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("index_version") or 0)')
    [ "$VERSION" -ge "$target" ] && return 0
    sleep 2
  done
  echo "index version never reached $target (last seen: $VERSION)" >&2
  return 1
}

echo "--- POST /write ---" >&2
curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request.json" | tee /tmp/write-resp.json
echo >&2
python3 -c 'import json;r=json.load(open("/tmp/write-resp.json"));assert r["accepted_count"]==6,r'

echo "--- 等 index-builder 轮询消费并发布 v1（上限 120s）---" >&2
wait_for_index_version 1
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

echo "--- POST /write（第二批）---" >&2
curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request_batch2.json" | tee /tmp/write-resp2.json
echo >&2
python3 -c 'import json;r=json.load(open("/tmp/write-resp2.json"));assert r["accepted_count"]==1,r'

FIRST_VERSION="$VERSION"
echo "--- 等第二次自动 build 发布 v$((FIRST_VERSION + 1))（上限 120s）---" >&2
wait_for_index_version "$((FIRST_VERSION + 1))"
echo "query /health 报告 index_version=$VERSION" >&2

echo "--- POST /query（第二批文档命中）---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request_batch2.json" | tee /tmp/query-resp2.json
echo >&2

echo "--- POST /query（第一批文档在新版本中仍可检索）---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | tee /tmp/query-resp1-again.json
echo >&2
python3 - "$FIRST_VERSION" <<'PY'
import json, sys
first_version = int(sys.argv[1])

r2 = json.load(open("/tmp/query-resp2.json"))
assert r2["index_version"] > first_version, r2
assert "doc-golang-batch2" in [c["doc_id"] for c in r2["dynamic_chunks"]], r2

r1 = json.load(open("/tmp/query-resp1-again.json"))
assert r1["index_version"] > first_version, r1
assert "doc-rust-hybrid" in [c["doc_id"] for c in r1["dynamic_chunks"]], (
    "第一批文档在第二次 build 后丢失——快照必须覆盖全部 WAL 段", r1)
print("HTTP server multi-write flow OK:", r1["index_version"])
PY
