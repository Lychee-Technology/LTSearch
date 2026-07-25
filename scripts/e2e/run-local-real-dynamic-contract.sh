#!/usr/bin/env bash
set -euo pipefail
# 动态索引本地 HTTP 契约黑盒 E2E（#142）：在 real-LTEmbed 三角色拓扑上按序
# 验证六组契约，断言仅读 HTTP 状态码与 JSON 响应，不读本地存储：
#   Phase 1  无 index：health/404/405、malformed 与写侧 400、query 500
#   Phase 2  写 batch1 → 自动 worker 发布首个版本
#   Phase 3  有 index：query 字段级 400、投影/过滤/窗口/权重 no-op 语义
#   Phase 4  双通道删除（/delete 与 tagged /write delete）各自生效
#   Phase 5  第二次写入的新版本保留先前批次
#   Phase 6  保留共享卷重启三角色 → 版本与动态查询仍可经 HTTP 访问
#
# 版本推进用 PREV=$LHTTP_VERSION 链式 + `wait PREV+1`，断言只用 >=：SQLite
# 队列的租约回收/退避重试可能多发布版本，绝对版本号会漂；文档集合断言才是
# 主判据。所有期望集合由 python 从 fixtures 现算，runner 不硬编码 doc_id。
#
# 真实模型语义排序有抖动：只做集合/计数断言（top_k=6 的检索窗口 18 > 语料
# 数，结果集恰等于存活全集），不断言名次/分数。
# 本套件依赖自动 worker 发布版本:显式钉死为 true,防止宿主环境残留的
# LTSEARCH_BUILD_WORKER_ENABLED=false 经 compose 插值透传后,Phase 2 写入
# 成功却永远等不到版本发布(白等 180s 才失败)。
export LTSEARCH_BUILD_WORKER_ENABLED=true

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FIXTURES="$REPO_ROOT/tests/fixtures/e2e"
CONTRACT="$FIXTURES/contract"
IMAGE_TAG="${LTSEARCH_LOCAL_LTEMBED_IMAGE:-ltsearch-local-ltembed:dev}"

source "$REPO_ROOT/scripts/e2e/local_http_lib.sh"

if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
  echo "--- image $IMAGE_TAG missing, building ---" >&2
  bash "$REPO_ROOT/scripts/e2e/build-local-ltembed-image.sh"
fi

lhttp_init "$REPO_ROOT/docker-compose.local-ltembed.yml" ltsearch-real-contract
trap 'lhttp_finish $?' EXIT

# down/up 重建后临时 host 端口全变，Phase 6 重启后必须重新调用本函数。
discover_ports() {
  WRITE_BASE="http://127.0.0.1:$(lhttp_port write)"
  BUILD_BASE="http://127.0.0.1:$(lhttp_port build)"
  QUERY_BASE="http://127.0.0.1:$(lhttp_port query)"
  echo "write=$WRITE_BASE build=$BUILD_BASE query=$QUERY_BASE" >&2
}

# 断言查询响应的存活文档集合恰等于「batch1 (+batch2) − 已删除」，并锁定
# 分组字段一致性与本拓扑的纯动态契约（无 static 结果、无 static_release_id）。
# $1=记录名 $2=逗号分隔的已应用 delete fixture 名（可空） $3=with-batch2|without-batch2
assert_alive_set() {
  local name="$1" deletes="${2:-}" batch2="${3:-without-batch2}"
  python3 - "$LHTTP_RUN_DIR/$name.response.json" "$FIXTURES" "$CONTRACT" "$deletes" "$batch2" <<'PY'
import json, sys
response_path, fixtures, contract, deletes, batch2 = sys.argv[1:6]
r = json.load(open(response_path))
expected = {
    d["doc_id"]
    for d in json.load(open(f"{fixtures}/write_request.json"))["documents"]
}
if batch2 == "with-batch2":
    expected |= {
        d["doc_id"]
        for d in json.load(open(f"{fixtures}/write_request_batch2.json"))["documents"]
    }
deleted = set()
for fixture in filter(None, deletes.split(",")):
    deleted |= set(json.load(open(f"{contract}/{fixture}"))["doc_ids"])
expected -= deleted
got = {c["doc_id"] for c in r["dynamic_chunks"]}
assert got == expected, (sorted(got), sorted(expected))
assert deleted.isdisjoint(got), (sorted(deleted), sorted(got))
assert r["dynamic_count"] == len(r["dynamic_chunks"]), r
assert r["static_count"] == len(r["static_chunks"]) == 0, r
assert "static_release_id" not in r, r
for chunk in r["dynamic_chunks"]:
    assert chunk["chunk_source"] == "dynamic", chunk
    # 双路（vector+keyword）融合后 source 恒为 hybrid；出现 keyword 说明
    # embedding 路径静默回退——本断言同时是「真实向量路径生效」的判别器。
    assert chunk["source"] == "hybrid", chunk
    assert "corpus_type" not in chunk, chunk
PY
}

echo "=== Phase 1: 无 index 的契约（health / 404 / 405 / 400 / query 500）===" >&2
lhttp_up
discover_ports

lhttp_assert_health health-write "$WRITE_BASE" ltsearch-write
lhttp_assert_health health-build "$BUILD_BASE" ltsearch-index-builder
lhttp_assert_health health-query "$QUERY_BASE" ltsearch-query
python3 - "$LHTTP_RUN_DIR/health-query.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["index_version"] is None, body
PY

echo "--- 未知路径 404（axum 默认，空 body，只断状态码）---" >&2
lhttp_expect_status nf-write GET "$WRITE_BASE/nope" 404
lhttp_expect_status nf-build GET "$BUILD_BASE/nope" 404
lhttp_expect_status nf-query GET "$QUERY_BASE/nope" 404

echo "--- 已注册路径错误方法 405（axum 默认，空 body，只断状态码）---" >&2
lhttp_expect_status mn-write GET "$WRITE_BASE/write" 405
lhttp_expect_status mn-delete GET "$WRITE_BASE/delete" 405
lhttp_expect_status mn-write-health POST "$WRITE_BASE/health" 405
lhttp_expect_status mn-build GET "$BUILD_BASE/build" 405
lhttp_expect_status mn-build-health POST "$BUILD_BASE/health" 405
lhttp_expect_status mn-query GET "$QUERY_BASE/query" 405
lhttp_expect_status mn-query-health POST "$QUERY_BASE/health" 405

echo "--- malformed JSON → 400 validation_error（四端点，handler 层 serde）---" >&2
lhttp_expect_status bad-json-query POST "$QUERY_BASE/query" 400 "$CONTRACT/malformed_body.txt"
lhttp_assert_error_body bad-json-query validation_error "failed to deserialize search request"
lhttp_expect_status bad-json-write POST "$WRITE_BASE/write" 400 "$CONTRACT/malformed_body.txt"
lhttp_assert_error_body bad-json-write validation_error "failed to deserialize write request"
lhttp_expect_status bad-json-delete POST "$WRITE_BASE/delete" 400 "$CONTRACT/malformed_body.txt"
lhttp_assert_error_body bad-json-delete validation_error "failed to deserialize delete request"
lhttp_expect_status bad-json-build POST "$BUILD_BASE/build" 400 "$CONTRACT/malformed_body.txt"
lhttp_assert_error_body bad-json-build validation_error "failed to deserialize build request"
lhttp_expect_status empty-body-write POST "$WRITE_BASE/write" 400
lhttp_assert_error_body empty-body-write validation_error "failed to deserialize write request"

echo "--- 写侧校验 400（与索引无关，随时生效）---" >&2
lhttp_expect_status empty-docs POST "$WRITE_BASE/write" 400 "$CONTRACT/write_empty_documents.json"
lhttp_assert_error_body empty-docs validation_error "documents is required"
lhttp_expect_status invalid-doc POST "$WRITE_BASE/write" 400 "$CONTRACT/write_invalid_document.json"
lhttp_assert_error_body invalid-doc validation_error "text is required"
lhttp_expect_status missing-field-doc POST "$WRITE_BASE/write" 400 "$CONTRACT/write_document_missing_field.json"
lhttp_assert_error_body missing-field-doc validation_error "failed to deserialize write request"
lhttp_expect_status empty-delete POST "$WRITE_BASE/delete" 400 "$CONTRACT/delete_empty.json"
lhttp_assert_error_body empty-delete validation_error "doc_ids is required"
lhttp_expect_status empty-tagged-delete POST "$WRITE_BASE/write" 400 "$CONTRACT/write_delete_empty.json"
lhttp_assert_error_body empty-tagged-delete validation_error "doc_ids is required"

echo "--- 非法 build（JSON 形状错）→ 400 validation_error ---" >&2
lhttp_expect_status bad-shape-build POST "$BUILD_BASE/build" 400 "$CONTRACT/build_invalid_shape.json"
lhttp_assert_error_body bad-shape-build validation_error "failed to deserialize build request"

echo "--- 合法 query 无 active index → 500 execution_error ---" >&2
lhttp_expect_status query-no-index POST "$QUERY_BASE/query" 500 "$FIXTURES/query_request_real.json"
lhttp_assert_error_body query-no-index execution_error "bootstrap failed"

echo "--- 非法 query 无 active index 也稳定 400（校验前置于 bootstrap，AC-1）---" >&2
# execution_error 只覆盖「合法请求 + 无索引」；字段级校验在 handler 层前置,
# 400 validation_error 不随索引状态漂移。Phase 3 会以同一 fixture 复测有
# index 的一侧,两侧共同锁定该契约。
lhttp_expect_status query-no-index-topk POST "$QUERY_BASE/query" 400 "$CONTRACT/query_top_k_zero.json"
lhttp_assert_error_body query-no-index-topk validation_error "top_k must be between 1 and 100"
lhttp_expect_status query-empty POST "$QUERY_BASE/query" 400 "$CONTRACT/query_empty.json"
lhttp_assert_error_body query-empty validation_error "query is required"
lhttp_expect_status query-too-long POST "$QUERY_BASE/query" 400 "$CONTRACT/query_too_long.json"
lhttp_assert_error_body query-too-long validation_error "query must be between 1 and 1000"

echo "=== Phase 2: 写 batch1 → 自动 worker 发布首个版本 ===" >&2
lhttp_request write-batch1 POST "$WRITE_BASE/write" "$FIXTURES/write_request.json" >/dev/null
lhttp_assert_status 200 write-batch1
python3 - "$LHTTP_RUN_DIR/write-batch1.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["accepted_count"] == 6, body
assert body["wal_key"], body
PY
PREV=0
lhttp_wait_index_version "$QUERY_BASE" $((PREV + 1))
PREV=$LHTTP_VERSION
echo "published index_version=$PREV" >&2

echo "=== Phase 3: 有 index 的查询契约 ===" >&2
echo "--- query 字段级校验 400（此刻才走到 router.search 的 validate）---" >&2
lhttp_expect_status topk-zero POST "$QUERY_BASE/query" 400 "$CONTRACT/query_top_k_zero.json"
lhttp_assert_error_body topk-zero validation_error "top_k must be between 1 and 100"
lhttp_expect_status topk-over POST "$QUERY_BASE/query" 400 "$CONTRACT/query_top_k_over_max.json"
lhttp_assert_error_body topk-over validation_error "top_k must be between 1 and 100"
lhttp_expect_status bad-filter-field POST "$QUERY_BASE/query" 400 "$CONTRACT/query_invalid_filter_field.json"
lhttp_assert_error_body bad-filter-field validation_error "invalid value"
lhttp_expect_status bad-weights POST "$QUERY_BASE/query" 400 "$CONTRACT/query_invalid_weights.json"
lhttp_assert_error_body bad-weights validation_error "static_bias"

echo "--- 非法 build（语义错：WAL 段不存在）→ 500 build_error ---" >&2
# fixture 双保险防污染：wal_key 指向不存在的段（读段即失败）,且 version_id=1
# ≤ 当前 head（即使读段意外成功，发布 CAS 也会拒绝）,绝不会推进索引状态。
lhttp_expect_status missing-wal-build POST "$BUILD_BASE/build" 500 "$CONTRACT/build_missing_wal.json"
lhttp_assert_error_body missing-wal-build build_error

echo "--- 全量查询：成员/计数/分组字段/纯动态契约 ---" >&2
lhttp_request query-full POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-full
assert_alive_set query-full
python3 - "$LHTTP_RUN_DIR/query-full.response.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r["index_version"] >= 1, r
PY

echo "--- top_k 窗口语义：top_k=1 → 每组最多 min(3*top_k,100)=3 条 ---" >&2
lhttp_request query-topk-one POST "$QUERY_BASE/query" "$CONTRACT/query_top_k_one.json" >/dev/null
lhttp_assert_status 200 query-topk-one
python3 - "$LHTTP_RUN_DIR/query-topk-one.response.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
# 语料 6 篇 > 窗口 3 ⇒ 截断后恰为 3；同时锁 count 与数组长度一致。
assert r["dynamic_count"] == len(r["dynamic_chunks"]) == 3, r
PY

echo "--- metadata AND filter：恰命中同时满足全部键的文档 ---" >&2
lhttp_request query-filter POST "$QUERY_BASE/query" "$CONTRACT/query_filter_and.json" >/dev/null
lhttp_assert_status 200 query-filter
python3 - "$LHTTP_RUN_DIR/query-filter.response.json" "$FIXTURES/write_request.json" "$CONTRACT/query_filter_and.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
documents = json.load(open(sys.argv[2]))["documents"]
filters = json.load(open(sys.argv[3]))["filters"]
expected = {
    d["doc_id"]
    for d in documents
    if all(d["metadata"].get(k) == v for k, v in filters.items())
}
partial = {
    d["doc_id"]
    for d in documents
    if any(d["metadata"].get(k) == v for k, v in filters.items())
} - expected
got = {c["doc_id"] for c in r["dynamic_chunks"]}
assert expected, "filter fixture 与语料失配：AND 命中集为空"
assert partial, "filter fixture 失去判别力：没有『只满足部分键』的文档"
assert got == expected, (sorted(got), sorted(expected))
# AND 语义 vs OR 语义的判别：只满足其中一个键的文档必须缺席。
assert partial.isdisjoint(got), (sorted(partial), sorted(got))
PY

echo "--- include_metadata:false：metadata 置 null,citation 保留,无 corpus_type ---" >&2
lhttp_request query-no-meta POST "$QUERY_BASE/query" "$CONTRACT/query_no_metadata.json" >/dev/null
lhttp_assert_status 200 query-no-meta
python3 - "$LHTTP_RUN_DIR/query-no-meta.response.json" "$FIXTURES/write_request.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
documents = json.load(open(sys.argv[2]))["documents"]
citation_docs = {
    d["doc_id"]
    for d in documents
    if {"resource_id", "source_type", "source_ref"} <= set(d["metadata"])
}
assert citation_docs, "语料失去判别力：没有带 citation 三键的文档"
# 先断言 citation 文档确实在响应里（top_k 全覆盖保证其必然出现）,否则
# 实现丢掉整篇文档时下面的逐 chunk 检查会真空通过。
returned = {c["doc_id"] for c in r["dynamic_chunks"]}
assert citation_docs <= returned, (sorted(citation_docs), sorted(returned))
for chunk in r["dynamic_chunks"]:
    assert chunk["metadata"] is None, chunk
    assert "corpus_type" not in chunk, chunk
    if chunk["doc_id"] in citation_docs:
        # citation 是一等出处字段,不属于自由 metadata,剥离后必须保留。
        assert chunk["citation"] is not None, chunk
        assert chunk["citation"]["title"], chunk
PY

echo "--- filter + include_metadata:false 组合：先过滤后剥离 ---" >&2
lhttp_request query-filter-no-meta POST "$QUERY_BASE/query" "$CONTRACT/query_filter_and_no_metadata.json" >/dev/null
lhttp_assert_status 200 query-filter-no-meta
python3 - "$LHTTP_RUN_DIR/query-filter-no-meta.response.json" "$LHTTP_RUN_DIR/query-filter.response.json" <<'PY'
import json, sys
stripped = json.load(open(sys.argv[1]))
with_meta = json.load(open(sys.argv[2]))
# 过滤基于 metadata、发生在剥离之前：结果集必须与带 metadata 的同 filter
# 查询一致，且每条 metadata 已置 null。
assert {c["doc_id"] for c in stripped["dynamic_chunks"]} == {
    c["doc_id"] for c in with_meta["dynamic_chunks"]
}, (stripped, with_meta)
for chunk in stripped["dynamic_chunks"]:
    assert chunk["metadata"] is None, chunk
PY

echo "--- corpus_weights no-op 契约：带极端权重与不带权重响应等价 ---" >&2
lhttp_request query-noop-a POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-noop-a
lhttp_request query-noop-a2 POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-noop-a2
lhttp_request query-noop-b POST "$QUERY_BASE/query" "$CONTRACT/query_weights_extreme.json" >/dev/null
lhttp_assert_status 200 query-noop-b
python3 - "$LHTTP_RUN_DIR/query-noop-a.response.json" "$LHTTP_RUN_DIR/query-noop-a2.response.json" "$LHTTP_RUN_DIR/query-noop-b.response.json" <<'PY'
import json, sys
def load(path):
    r = json.load(open(path))
    r.pop("latency_ms", None)
    return r
a, a2, b = (load(p) for p in sys.argv[1:4])
# A==A' 是对照组：先证明同请求响应确定（检索排序有 doc_id 全序 tie-break），
# 再看 A==B——若 A==A' 而 A!=B，说明 corpus_weights 生效了（no-op 契约被破坏）。
assert a == a2, "同请求两次响应不一致：引擎不确定,no-op 判别失效"
assert a == b, "带极端 corpus_weights 的响应有差异：no-op 契约被破坏"
PY

echo "=== Phase 4: 双通道删除,自动发布的后续版本中生效 ===" >&2
lhttp_request delete-endpoint POST "$WRITE_BASE/delete" "$CONTRACT/delete_doc_python_noise.json" >/dev/null
lhttp_assert_status 200 delete-endpoint
python3 - "$LHTTP_RUN_DIR/delete-endpoint.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["accepted_count"] == 1, body
assert body["wal_key"], body
PY
lhttp_wait_index_version "$QUERY_BASE" $((PREV + 1))
PREV=$LHTTP_VERSION
lhttp_request query-after-delete1 POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-after-delete1
assert_alive_set query-after-delete1 "delete_doc_python_noise.json"

lhttp_request delete-tagged POST "$WRITE_BASE/write" "$CONTRACT/write_delete_doc_tenant_b.json" >/dev/null
lhttp_assert_status 200 delete-tagged
python3 - "$LHTTP_RUN_DIR/delete-tagged.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["accepted_count"] == 1, body
assert body["wal_key"], body
PY
lhttp_wait_index_version "$QUERY_BASE" $((PREV + 1))
PREV=$LHTTP_VERSION
lhttp_request query-after-delete2 POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-after-delete2
assert_alive_set query-after-delete2 "delete_doc_python_noise.json,write_delete_doc_tenant_b.json"

echo "=== Phase 5: 第二次写入,新版本保留先前批次 ===" >&2
lhttp_request write-batch2 POST "$WRITE_BASE/write" "$FIXTURES/write_request_batch2.json" >/dev/null
lhttp_assert_status 200 write-batch2
python3 - "$LHTTP_RUN_DIR/write-batch2.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["accepted_count"] == 1, body
assert body["wal_key"], body
PY
lhttp_wait_index_version "$QUERY_BASE" $((PREV + 1))
PREV=$LHTTP_VERSION
lhttp_request query-after-batch2 POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-after-batch2
assert_alive_set query-after-batch2 "delete_doc_python_noise.json,write_delete_doc_tenant_b.json" with-batch2

echo "=== Phase 6: 保留共享卷重启三角色,版本与动态查询仍可访问 ===" >&2
lhttp_snapshot_logs pre-restart
lhttp_restart
discover_ports
lhttp_assert_health health-write-restarted "$WRITE_BASE" ltsearch-write
lhttp_assert_health health-build-restarted "$BUILD_BASE" ltsearch-index-builder
lhttp_assert_health health-query-restarted "$QUERY_BASE" ltsearch-query
python3 - "$LHTTP_RUN_DIR/health-query-restarted.response.json" "$PREV" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
# 卷被误清时版本会掉回 null/0——这是重启持久性的第一道判据。
assert (body["index_version"] or 0) >= int(sys.argv[2]), (body, sys.argv[2])
PY
lhttp_request query-after-restart POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-after-restart
assert_alive_set query-after-restart "delete_doc_python_noise.json,write_delete_doc_tenant_b.json" with-batch2

echo "--- 动态索引 HTTP 契约全部通过(final index_version=$PREV) ---" >&2
