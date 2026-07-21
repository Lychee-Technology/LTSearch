#!/usr/bin/env bash
set -euo pipefail
# real-LTEmbed 本地主链路黑盒 E2E（#141）：以 local,ltembed 真实模型镜像跑
# write/build/query 三角色（docker-compose.local-ltembed.yml），仅经
# /health、/write、/query 的 HTTP 响应断言 health → write → 自动 build →
# query 主链路。无 Moto、无 AWS env、无 Lambda/SAM。
#
# 隔离与清理由 local_http_lib.sh 承担：每次运行独立 compose project/端口/
# 卷/日志目录；无论成败 teardown，失败保留 .e2e-tmp/ltsearch-real-<id>/ 诊断。
#
# 真实模型断言只取稳定性质（语义排序有抖动）：accepted_count、版本推进、
# 成员命中（top_k=6 覆盖全部文档，不断言名次/分数）。
#
# 前置：镜像 ltsearch-local-ltembed:dev（或 LTSEARCH_LOCAL_LTEMBED_IMAGE 指定
# tag）；缺失时自动调 build-local-ltembed-image.sh 构建。
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FIXTURES="$REPO_ROOT/tests/fixtures/e2e"
IMAGE_TAG="${LTSEARCH_LOCAL_LTEMBED_IMAGE:-ltsearch-local-ltembed:dev}"

source "$REPO_ROOT/scripts/e2e/local_http_lib.sh"

if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
  echo "--- image $IMAGE_TAG missing, building ---" >&2
  bash "$REPO_ROOT/scripts/e2e/build-local-ltembed-image.sh"
fi

lhttp_init "$REPO_ROOT/docker-compose.local-ltembed.yml" ltsearch-real
trap 'lhttp_finish $?' EXIT

echo "--- up -d --wait（healthcheck 即真实推理门，首次模型加载较慢）---" >&2
lhttp_up

WRITE_BASE="http://127.0.0.1:$(lhttp_port write)"
BUILD_BASE="http://127.0.0.1:$(lhttp_port build)"
QUERY_BASE="http://127.0.0.1:$(lhttp_port query)"
echo "write=$WRITE_BASE build=$BUILD_BASE query=$QUERY_BASE" >&2

echo "--- 三角色 /health 均 200（build/query 含真实 embedding probe）---" >&2
lhttp_assert_health health-write "$WRITE_BASE"
lhttp_assert_health health-build "$BUILD_BASE"
lhttp_assert_health health-query "$QUERY_BASE"

echo "--- POST /write ---" >&2
lhttp_request write POST "$WRITE_BASE/write" "$FIXTURES/write_request.json" >/dev/null
lhttp_assert_status 200 write
python3 - "$LHTTP_RUN_DIR/write.response.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r["accepted_count"] == 6, r
assert r["wal_key"], r
PY

echo "--- 等自动 worker 消费队列并发布 v1（真实 embedding，上限 180s）---" >&2
lhttp_wait_index_version "$QUERY_BASE" 1
echo "query /health 报告 index_version=$LHTTP_VERSION" >&2

echo "--- POST /query（真实语义检索，成员断言）---" >&2
lhttp_request query POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query
python3 - "$LHTTP_RUN_DIR/query.response.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r["index_version"] >= 1, r
assert r["dynamic_count"] >= 1, r
doc_ids = [c["doc_id"] for c in r["dynamic_chunks"]]
assert "doc-rust-hybrid" in doc_ids, ("真实模型未命中相关文档", doc_ids, r)
print("local real flow OK:", r["index_version"], r["dynamic_count"])
PY

echo "--- real-LTEmbed 主链路通过 ---" >&2
