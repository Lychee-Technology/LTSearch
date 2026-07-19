#!/usr/bin/env bash
set -euo pipefail
# 静态 release v3 端到端流（#112 PR-2 Task 14）：本地（AWS-free、moto/docker-free）
# 单二进制原生链路，与 run-local-server-flow.sh 同范式（同一临时 LTSEARCH_LOCAL_ROOT、
# 双侧 fixed embeddings、curl + 内联 python3 断言、健康轮询、清理 trap）。
#
# 五步：
#   1) example 产 512 维 Lance fixture（数据集 A，含 zh/en 行 + citation 元数据）；
#   2) 最小动态管线 write→build 发布 v1（512 维——query bootstrap 校验 embedding dim
#      与 dynamic manifest 一致，且无 dynamic _head 时 /query resolve 会失败）；
#   3) static-build --config→relA → static-activate --release relA --root $ROOT；
#   4) POST /query（filters lang:zh + include_metadata）断 static_chunks[0].doc_id、
#      citation、顶层 static_release_id == 激活 id；
#   5) variant b 造 relB → activate → /health 的 static_release_id 翻转 + 再查询命中新 id。
#
# 无 Docker、无 moto、无模型下载（fixed embeddings）。
# 前置：`cargo build --no-default-features --features local --bin ltsearch \
#   --example emit_static_lance_fixture` 已完成，或设 LTSEARCH_E2E_BIN /
#   LTSEARCH_E2E_FIXTURE_BIN 指向现成二进制。
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${LTSEARCH_E2E_BIN:-$REPO_ROOT/target/debug/ltsearch}"
FIXTURE_BIN="${LTSEARCH_E2E_FIXTURE_BIN:-$REPO_ROOT/target/debug/examples/emit_static_lance_fixture}"
FIXTURES="$REPO_ROOT/tests/fixtures/e2e"

PORT_WRITE="${LTSEARCH_E2E_STATIC_WRITE_PORT:-18191}"
PORT_BUILD="${LTSEARCH_E2E_STATIC_BUILD_PORT:-18192}"
PORT_QUERY="${LTSEARCH_E2E_STATIC_QUERY_PORT:-18190}"
BASE_WRITE="http://localhost:$PORT_WRITE"
BASE_BUILD="http://localhost:$PORT_BUILD"
BASE_QUERY="http://localhost:$PORT_QUERY"

ROOT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ltsearch-static-e2e.XXXXXX")"
PIDS=()

# 双侧 512 维 fixed embeddings（同维同向量常量）。Fixture 行的 embedding 亦全 0.1，
# 故对本查询相似度一致，lang 过滤单独选出 top chunk。
EMBED_512="$(python3 -c 'print(",".join(["0.1"]*512))')"
export LTSEARCH_LOCAL_ROOT="$ROOT_DIR"
export LTSEARCH_BUILD_EMBEDDING_PROVIDER=fixed
export LTSEARCH_BUILD_FIXED_EMBEDDING="$EMBED_512"
export LTSEARCH_BUILD_EMBEDDING_DIM=512
export LTSEARCH_QUERY_EMBEDDING_PROVIDER=fixed
export LTSEARCH_QUERY_FIXED_EMBEDDING="$EMBED_512"

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

  for base in "$BASE_WRITE" "$BASE_BUILD" "$BASE_QUERY"; do
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

# 产一个变体的 Lance fixture 并 static-build 出 release 目录，回显 release_id。
# 用法: build_release <variant a|b> <dataset_dir> <release_out_dir>
build_release() {
  local variant="$1" dataset_dir="$2" release_dir="$3"
  local table_version
  table_version="$("$FIXTURE_BIN" "$dataset_dir" --variant "$variant")"
  echo "--- fixture variant=$variant → $dataset_dir @ table_version=$table_version ---" >&2

  local config_path="$ROOT_DIR/static-config-$variant.json"
  python3 - "$dataset_dir" "$table_version" "$config_path" <<'PY'
import json, sys
dataset_path, table_version, out = sys.argv[1:4]
json.dump({
    "dataset_path": dataset_path,
    "table_version": int(table_version),
    "corpus_type": "legal",
    "embedding_profile": {"model_id": "jina-v5-nano/512", "dim": 512},
}, open(out, "w"))
PY

  "$BIN" static-build --config "$config_path" --output "$release_dir" >&2
  # release_id 从 build 产物 manifest 读——activate 会 move 掉 release_dir,故先读。
  python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["release_id"])' \
    "$release_dir/release_manifest.json"
}

echo "--- 启动 write/build/query（root=${ROOT_DIR}）---" >&2
start_processes

# --- Step 2: 最小动态管线 write→build（512 维），满足 query bootstrap 的 dim 校验 ---
echo "--- POST /write（动态前置）---" >&2
curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request.json" | tee "$ROOT_DIR/write-resp.json"
echo >&2
python3 -c 'import json;r=json.load(open("'"$ROOT_DIR"'/write-resp.json"));assert r["accepted_count"]==6,r'

echo "--- 等 build 发布 dynamic v1（上限 120s）---" >&2
wait_for_index_version 1
echo "query /health 报告 dynamic index_version=$VERSION" >&2

# --- Step 1+3: fixture A → static-build relA → static-activate ---
echo "--- static-build 数据集 A → relA ---" >&2
RELEASE_A="$(build_release a "$ROOT_DIR/dataset-a" "$ROOT_DIR/release-a")"
echo "relA release_id=$RELEASE_A" >&2

echo "--- static-activate relA ---" >&2
"$BIN" static-activate --release "$ROOT_DIR/release-a" --root "$ROOT_DIR" \
  --expect-dim 512 >&2

# --- Step 4: POST /query（filters lang:zh + include_metadata），断言静态路径 ---
echo "--- POST /query（lang:zh 过滤，断 static_chunks / citation / static_release_id）---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d '{"query":"中文法规检索","top_k":3,"filters":{"lang":"zh"},"include_metadata":true}' \
  | tee "$ROOT_DIR/query-resp-a.json"
echo >&2
python3 - "$ROOT_DIR/query-resp-a.json" "$RELEASE_A" <<'PY'
import json, sys
resp = json.load(open(sys.argv[1]))
expected_release = sys.argv[2]

assert resp.get("static_release_id") == expected_release, (
    "top-level static_release_id must equal the activated release", resp.get("static_release_id"), expected_release)

static_chunks = resp["static_chunks"]
assert static_chunks, ("lang:zh must select the Chinese static chunk", resp)
top = static_chunks[0]
assert top["doc_id"] == "doc-alpha", ("static_chunks[0].doc_id must be the raw string doc-alpha", top["doc_id"])

citation = top.get("citation")
assert citation, ("static top chunk must carry a citation", top)
assert citation.get("resource_id"), ("citation.resource_id must be non-empty", citation)
assert citation.get("url"), ("citation.url must be non-empty", citation)

# lang:zh must exclude the English rows.
assert all(c["metadata"]["lang"] == "zh" for c in static_chunks), (
    "every returned static chunk must satisfy the lang:zh filter", static_chunks)
print("static release A query OK:", top["doc_id"], resp["static_release_id"][:12])
PY

# --- Step 5: variant b → relB → activate → /health 翻转 + 再查询命中新 id ---
echo "--- static-build 数据集 B（variant b，改一行）→ relB ---" >&2
RELEASE_B="$(build_release b "$ROOT_DIR/dataset-b" "$ROOT_DIR/release-b")"
echo "relB release_id=$RELEASE_B" >&2
[ "$RELEASE_A" != "$RELEASE_B" ] || {
  echo "variant b must produce a different release_id than variant a ($RELEASE_A)" >&2
  exit 1
}

echo "--- static-activate relB ---" >&2
"$BIN" static-activate --release "$ROOT_DIR/release-b" --root "$ROOT_DIR" \
  --expect-dim 512 >&2

echo "--- GET /health：static_release_id 必须翻转到 relB ---" >&2
curl -sf "$BASE_QUERY/health" | tee "$ROOT_DIR/health-b.json"
echo >&2
python3 - "$ROOT_DIR/health-b.json" "$RELEASE_A" "$RELEASE_B" <<'PY'
import json, sys
health = json.load(open(sys.argv[1]))
release_a, release_b = sys.argv[2], sys.argv[3]
head = health.get("static_release_id")
assert head == release_b, ("/health static_release_id must flip to relB", head, release_b)
assert head != release_a, ("/health static_release_id must no longer be relA", head)
print("/health static_release_id flipped:", head[:12])
PY

echo "--- POST /query（第二次）：命中新 release_id relB ---" >&2
curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d '{"query":"中文法规检索","top_k":3,"filters":{"lang":"zh"},"include_metadata":true}' \
  | tee "$ROOT_DIR/query-resp-b.json"
echo >&2
python3 - "$ROOT_DIR/query-resp-b.json" "$RELEASE_B" <<'PY'
import json, sys
resp = json.load(open(sys.argv[1]))
expected_release = sys.argv[2]
assert resp.get("static_release_id") == expected_release, (
    "second query must hit the newly activated release B", resp.get("static_release_id"), expected_release)
static_chunks = resp["static_chunks"]
assert static_chunks and static_chunks[0]["doc_id"] == "doc-alpha", (
    "doc-alpha must still be the top zh static chunk under release B", static_chunks)
print("static release B query OK:", resp["static_release_id"][:12])
PY

echo "static-build->activate->query v3 e2e OK (relA=${RELEASE_A} relB=${RELEASE_B})" >&2
