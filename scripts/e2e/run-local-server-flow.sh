#!/usr/bin/env bash
set -euo pipefail
# 本地（AWS-free）单二进制原生冒烟（#124 AC-3）：三个 `ltsearch` 子命令进程共享
# 一个临时 LTSEARCH_LOCAL_ROOT（SQLite 控制面 + 文件制品），驱动
# write→(build 轮询 SQLite 队列自动构建)→query，随后**重启全部进程**（保留数据目录）
# 验证耐久性：query 仍见已建版本，新 write 仍能触发构建（队列/指针在 SQLite 中存活）。
# 无 Docker、无 moto、无模型下载（fixed embeddings）。
#
# 前置：`cargo build --no-default-features --features local --bin ltsearch` 已完成，
# 或设 LTSEARCH_E2E_BIN 指向现成二进制。
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${LTSEARCH_E2E_BIN:-$REPO_ROOT/target/debug/ltsearch}"
FIXTURES="$REPO_ROOT/tests/fixtures/e2e"

PORT_WRITE="${LTSEARCH_E2E_LOCAL_WRITE_PORT:-18091}"
PORT_BUILD="${LTSEARCH_E2E_LOCAL_BUILD_PORT:-18092}"
PORT_QUERY="${LTSEARCH_E2E_LOCAL_QUERY_PORT:-18090}"
BASE_WRITE="http://localhost:$PORT_WRITE"
BASE_QUERY="http://localhost:$PORT_QUERY"

ROOT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ltsearch-local-e2e.XXXXXX")"
PIDS=()

# 共享环境：单一本地根 + 双侧 fixed embeddings（同维同向量）。
export LTSEARCH_LOCAL_ROOT="$ROOT_DIR"
export LTSEARCH_BUILD_EMBEDDING_PROVIDER=fixed
export LTSEARCH_BUILD_FIXED_EMBEDDING="0.1,0.2,0.3"
export LTSEARCH_BUILD_EMBEDDING_DIM=3
export LTSEARCH_QUERY_EMBEDDING_PROVIDER=fixed
export LTSEARCH_QUERY_FIXED_EMBEDDING="0.1,0.2,0.3"

stop_processes() {
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${PIDS[@]:-}"; do
    wait "$pid" 2>/dev/null || true
  done
  PIDS=()
}

cleanup() {
  stop_processes
  rm -rf "$ROOT_DIR"
}
trap cleanup EXIT

start_processes() {
  LTSEARCH_HTTP_PORT="$PORT_WRITE" "$BIN" write >>"$ROOT_DIR/write.log" 2>&1 &
  PIDS+=($!)
  LTSEARCH_HTTP_PORT="$PORT_BUILD" "$BIN" build >>"$ROOT_DIR/build.log" 2>&1 &
  PIDS+=($!)
  LTSEARCH_HTTP_PORT="$PORT_QUERY" "$BIN" query >>"$ROOT_DIR/query.log" 2>&1 &
  PIDS+=($!)

  for base in "$BASE_WRITE" "http://localhost:$PORT_BUILD" "$BASE_QUERY"; do
    for _ in $(seq 1 30); do
      curl -sf "$base/health" >/dev/null 2>&1 && break
      sleep 1
    done
    curl -sf "$base/health" >/dev/null 2>&1 || {
      echo "service at $base never became healthy" >&2
      tail -20 "$ROOT_DIR"/*.log >&2 || true
      return 1
    }
  done
}

# 轮询 query /health 直到 index_version >= $1（上限 120s），版本号写入 VERSION。
wait_for_index_version() {
  local target="$1"
  VERSION=0
  for _ in $(seq 1 60); do
    VERSION=$(curl -sf "$BASE_QUERY/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("index_version") or 0)')
    [ "$VERSION" -ge "$target" ] && return 0
    sleep 2
  done
  echo "index version never reached $target (last seen: $VERSION)" >&2
  tail -40 "$ROOT_DIR/build.log" >&2 || true
  return 1
}

echo "--- 启动 write/build/query（root=${ROOT_DIR}）---" >&2
start_processes

echo "--- POST /write ---" >&2
curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request.json" | tee /tmp/local-write-resp.json
echo >&2
python3 -c 'import json;r=json.load(open("/tmp/local-write-resp.json"));assert r["accepted_count"]==6,r'

echo "--- 等 build 轮询 SQLite 队列并发布 v1（上限 120s）---" >&2
wait_for_index_version 1
echo "query /health 报告 index_version=$VERSION" >&2

echo "--- POST /query ---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | tee /tmp/local-query-resp.json
echo >&2
python3 - <<'PY'
import json
r = json.load(open("/tmp/local-query-resp.json"))
assert r["index_version"] >= 1, r
assert r["dynamic_count"] >= 1, r
assert "doc-rust-hybrid" in [c["doc_id"] for c in r["dynamic_chunks"]], r
print("local native flow OK:", r["index_version"], r["dynamic_count"])
PY

FIRST_VERSION="$VERSION"

echo "--- 重启全部进程（保留 ${ROOT_DIR}，验证 SQLite 耐久性）---" >&2
stop_processes
start_processes

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
  -d @"$FIXTURES/write_request_batch2.json" | tee /tmp/local-write-resp2.json
echo >&2
python3 -c 'import json;r=json.load(open("/tmp/local-write-resp2.json"));assert r["accepted_count"]==1,r'

echo "--- 等第二次自动 build 发布 v$((FIRST_VERSION + 1))（上限 120s）---" >&2
wait_for_index_version "$((FIRST_VERSION + 1))"

echo "--- POST /query（第二批文档命中 + 第一批仍在）---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request_batch2.json" | tee /tmp/local-query-resp2.json
echo >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | tee /tmp/local-query-resp1-again.json
echo >&2
python3 - "$FIRST_VERSION" <<'PY'
import json, sys
first_version = int(sys.argv[1])

r2 = json.load(open("/tmp/local-query-resp2.json"))
assert r2["index_version"] > first_version, r2
assert "doc-golang-batch2" in [c["doc_id"] for c in r2["dynamic_chunks"]], r2

r1 = json.load(open("/tmp/local-query-resp1-again.json"))
assert r1["index_version"] > first_version, r1
assert "doc-rust-hybrid" in [c["doc_id"] for c in r1["dynamic_chunks"]], (
    "第一批文档在重启后的第二次 build 中丢失——快照必须覆盖全部 WAL 段", r1)
print("local native restart flow OK:", r1["index_version"])
PY
