#!/usr/bin/env bash
set -euo pipefail
# 单镜像本地部署的权威 e2e（#125 / #108 AC-4）：一个镜像三个角色、共享卷、
# moto-free。write→(build 轮询 SQLite 队列自动构建)→query 命中，随后
# **保留卷重启**（`down` 不带 -v → `up -d --wait`）验证：query 仍服务已建版本、
# 新 write 仍能触发构建（SQLite 队列/指针在卷上存活）。
# 前置：ltsearch-local:dev 已构建；`docker compose -f docker-compose.local.yml
# up -d --wait` 已就绪（三个服务 healthcheck 通过）。
BASE_WRITE="${LTSEARCH_E2E_LOCAL_WRITE_BASE:-http://localhost:19081}"
BASE_QUERY="${LTSEARCH_E2E_LOCAL_QUERY_BASE:-http://localhost:19080}"
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FIXTURES="$REPO_ROOT/tests/fixtures/e2e"
COMPOSE="docker compose -f $REPO_ROOT/docker-compose.local.yml"

# 轮询 query /health 直到 index_version >= $1（上限 120s），版本号写入 VERSION。
wait_for_index_version() {
  local target="$1"
  VERSION=0
  for _ in $(seq 1 60); do
    VERSION=$(curl -sf "$BASE_QUERY/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("index_version") or 0)' || echo 0)
    [ "$VERSION" -ge "$target" ] && return 0
    sleep 2
  done
  echo "index version never reached $target (last seen: $VERSION)" >&2
  $COMPOSE logs build | tail -40 >&2 || true
  return 1
}

echo "--- POST /write ---" >&2
curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request.json" | tee /tmp/local-image-write.json
echo >&2
python3 -c 'import json;r=json.load(open("/tmp/local-image-write.json"));assert r["accepted_count"]==6,r'

echo "--- 等 build 轮询 SQLite 队列并发布 v1（上限 120s）---" >&2
wait_for_index_version 1
echo "query /health 报告 index_version=$VERSION" >&2

echo "--- POST /query ---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | tee /tmp/local-image-query.json
echo >&2
python3 - <<'PY'
import json
r = json.load(open("/tmp/local-image-query.json"))
assert r["index_version"] >= 1, r
assert r["dynamic_count"] >= 1, r
assert "doc-rust-hybrid" in [c["doc_id"] for c in r["dynamic_chunks"]], r
print("local image flow OK:", r["index_version"], r["dynamic_count"])
PY

FIRST_VERSION="$VERSION"

echo "--- 保留卷重启：down（不带 -v）→ up -d --wait ---" >&2
$COMPOSE down
$COMPOSE up -d --wait

echo "--- 重启后 query 仍服务 v$FIRST_VERSION ---" >&2
RESTART_VERSION=$(curl -sf "$BASE_QUERY/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("index_version") or 0)')
[ "$RESTART_VERSION" -ge "$FIRST_VERSION" ] || {
  echo "restart lost the active version: $RESTART_VERSION < $FIRST_VERSION" >&2
  exit 1
}
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | python3 -c 'import json,sys;r=json.load(sys.stdin);assert "doc-rust-hybrid" in [c["doc_id"] for c in r["dynamic_chunks"]],r'

echo "--- POST /write（第二批，重启后队列仍可用）---" >&2
curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request_batch2.json" | tee /tmp/local-image-write2.json
echo >&2
python3 -c 'import json;r=json.load(open("/tmp/local-image-write2.json"));assert r["accepted_count"]==1,r'

echo "--- 等第二次自动 build 发布 v$((FIRST_VERSION + 1))（上限 120s）---" >&2
wait_for_index_version "$((FIRST_VERSION + 1))"

echo "--- POST /query（第二批文档命中 + 第一批仍在）---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request_batch2.json" | tee /tmp/local-image-query2.json
echo >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | tee /tmp/local-image-query1-again.json
echo >&2
python3 - "$FIRST_VERSION" <<'PY'
import json, sys
first_version = int(sys.argv[1])

r2 = json.load(open("/tmp/local-image-query2.json"))
assert r2["index_version"] > first_version, r2
assert "doc-golang-batch2" in [c["doc_id"] for c in r2["dynamic_chunks"]], r2

r1 = json.load(open("/tmp/local-image-query1-again.json"))
assert r1["index_version"] > first_version, r1
assert "doc-rust-hybrid" in [c["doc_id"] for c in r1["dynamic_chunks"]], (
    "第一批文档在重启后的第二次 build 中丢失——快照必须覆盖全部 WAL 段", r1)
print("local image restart flow OK:", r1["index_version"])
PY
