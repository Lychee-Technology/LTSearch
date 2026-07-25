#!/usr/bin/env bash
set -euo pipefail
# 降级 bundle 健康契约黑盒 E2E（#142 AC-2 后半）：base compose 叠加
# docker-compose.local-ltembed.degraded.yml（query 侧 bundle 缺失、build 侧
# bundle 损坏/不完整），仅经 HTTP 断言：
#   - write /health 仍 200（write 无模型依赖）且 /write 仍可接受写入；
#   - query /health 503，detail 带 bundle_dir=/nonexistent（缺失分支）；
#   - build /health 503，detail 带 bundle_dir=/app（存在但非法分支）。
#
# degraded 下 query/build 的容器 healthcheck 注定 unhealthy，因此用
# lhttp_up_nowait（up -d 不带 --wait）启动，lhttp_wait_http_ready 等 HTTP
# 可达后再断言状态码。隔离与清理由 local_http_lib.sh 承担。
#
# 关掉后台 build worker：degraded write 产生的 job 在 build 侧必然失败,
# 会重试三次进 dead_jobs 刷日志噪音；本场景只验健康契约，不验 build。
export LTSEARCH_BUILD_WORKER_ENABLED=false

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FIXTURES="$REPO_ROOT/tests/fixtures/e2e"
IMAGE_TAG="${LTSEARCH_LOCAL_LTEMBED_IMAGE:-ltsearch-local-ltembed:dev}"

source "$REPO_ROOT/scripts/e2e/local_http_lib.sh"

if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
  echo "--- image $IMAGE_TAG missing, building ---" >&2
  bash "$REPO_ROOT/scripts/e2e/build-local-ltembed-image.sh"
fi

lhttp_init "$REPO_ROOT/docker-compose.local-ltembed.yml" ltsearch-real-degraded \
  "$REPO_ROOT/docker-compose.local-ltembed.degraded.yml"
trap 'lhttp_finish $?' EXIT

echo "--- up -d（no-wait：degraded 的 query/build healthcheck 注定 unhealthy）---" >&2
lhttp_up_nowait

WRITE_BASE="http://127.0.0.1:$(lhttp_port write)"
BUILD_BASE="http://127.0.0.1:$(lhttp_port build)"
QUERY_BASE="http://127.0.0.1:$(lhttp_port query)"
echo "write=$WRITE_BASE build=$BUILD_BASE query=$QUERY_BASE" >&2

echo "--- 等三角色 HTTP 层可达（不等健康）---" >&2
lhttp_wait_http_ready ready-write "$WRITE_BASE/health"
lhttp_wait_http_ready ready-build "$BUILD_BASE/health"
lhttp_wait_http_ready ready-query "$QUERY_BASE/health"

echo "--- write /health 200（write 无模型依赖，降级不受影响）---" >&2
lhttp_request health-write GET "$WRITE_BASE/health" >/dev/null
lhttp_assert_status 200 health-write
python3 - "$LHTTP_RUN_DIR/health-write.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["status"] == "ok", body
assert body["component"] == "ltsearch-write", body
assert body["index_version"] is None, body
PY

echo "--- query /health 503（bundle 缺失：/nonexistent 存在性预检失败）---" >&2
lhttp_request health-query GET "$QUERY_BASE/health" >/dev/null
lhttp_assert_status 503 health-query
python3 - "$LHTTP_RUN_DIR/health-query.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["status"] == "unavailable", body
assert body["component"] == "ltsearch-query", body
# detail 的 bundle_dir= 前缀证明 503 来自 embedding probe（bundle 供给层），
# 而不是 artifact sync / 读 head 等其他 unavailable 分支。
assert "bundle_dir=/nonexistent" in (body["detail"] or ""), body
PY

echo "--- build /health 503（bundle 损坏：/app 存在但不是合法 bundle）---" >&2
lhttp_request health-build GET "$BUILD_BASE/health" >/dev/null
lhttp_assert_status 503 health-build
python3 - "$LHTTP_RUN_DIR/health-build.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["status"] == "unavailable", body
assert body["component"] == "ltsearch-index-builder", body
assert "bundle_dir=/app" in (body["detail"] or ""), body
PY

echo "--- POST /write 在降级拓扑下仍 200（写入面与模型解耦的契约）---" >&2
lhttp_request write-degraded POST "$WRITE_BASE/write" "$FIXTURES/write_request.json" >/dev/null
lhttp_assert_status 200 write-degraded
python3 - "$LHTTP_RUN_DIR/write-degraded.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["accepted_count"] == 6, body
assert body["wal_key"], body
PY

echo "--- degraded health 契约通过 ---" >&2
