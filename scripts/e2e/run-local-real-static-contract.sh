#!/usr/bin/env bash
set -euo pipefail
# 静态语料本地 HTTP 检索契约黑盒 E2E（#143）：在 real-LTEmbed 三角色拓扑上，
# 于容器内用真实 LTEmbed bundle 生成 512 维 Lance fixture，实际执行
# static-build / static-activate，然后仅经公开 HTTP API 验证静态检索契约：
#   Phase 0  镜像预检：fixture 生成器必须已烘焙进镜像（防陈旧 tag）
#   Phase 1  拓扑起立：三角色健康，激活前无静态语料痕迹
#   Phase 2  动态前置：写入 → 自动 worker 发布 active 动态索引（静态解析的
#            先决条件），并留下纯动态对照组响应
#   Phase 3  容器内 stage release A：生成 dataset（--embedder ltembed）→
#            static-build → static-activate，全程在共享卷内（同文件系统 rename）
#   Phase 4  静态契约：static_chunks/static_count/corpus_type/citation、
#            动静分组共存且不融合、lang:zh 后过滤、include_metadata 投影
#            不破坏 citation、corpus_weights 对 static 同样 no-op
#   Phase 5  variant b 重建并重新激活 → static_release_id 经 HTTP 观测翻转
#
# 断言纪律（#143 AC-5）：一切行为断言只读 HTTP 状态码与 JSON 响应；Lance
# fixture、static-build 产物只作测试准备，不从文件系统读取任何静态制品或
# manifest 内容。static_release_id 的唯一来源是 /health 与 /query 响应——
# 断「非空、跨端点一致、variant 翻转」，不断具体取值。
#
# 真实模型语义排序有抖动：静态组只做集合/计数/降序断言；
# static_chunks[0] 只在 lang:zh 过滤后断（doc-alpha 是唯一 zh 行）。
# 本套件依赖自动 worker 发布动态版本:显式钉死为 true,防止宿主环境残留的
# LTSEARCH_BUILD_WORKER_ENABLED=false 经 compose 插值透传后写入成功却
# 永远等不到版本发布。
export LTSEARCH_BUILD_WORKER_ENABLED=true

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FIXTURES="$REPO_ROOT/tests/fixtures/e2e"
CONTRACT="$FIXTURES/contract"
STATIC_FIXTURES="$FIXTURES/static"
IMAGE_TAG="${LTSEARCH_LOCAL_LTEMBED_IMAGE:-ltsearch-local-ltembed:dev}"
# 容器内共享卷上的工作区：dataset 与 release 产物都必须落在共享卷内——
# static-activate 以同文件系统 rename 安装 release，跨设备会失败；且三角色
# 经同一卷立即可见。
STAGING=/var/lib/ltsearch/staging

source "$REPO_ROOT/scripts/e2e/local_http_lib.sh"

echo "=== Phase 0: 镜像预检（fixture 生成器必须在镜像内）===" >&2
if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
  echo "--- image $IMAGE_TAG missing, building ---" >&2
  bash "$REPO_ROOT/scripts/e2e/build-local-ltembed-image.sh"
fi
# 各 runner 共用「镜像存在即跳过构建」的粗粒度检查，Dockerfile 演进后旧 tag
# 不会自动重建；本套件依赖 #143 新增的 /app/emit_static_lance_fixture，
# 显式探测并自愈一次，二次探测仍缺失则硬失败。
if ! docker run --rm --entrypoint test "$IMAGE_TAG" -f /app/emit_static_lance_fixture; then
  echo "--- image $IMAGE_TAG predates #143 (no fixture emitter), rebuilding ---" >&2
  bash "$REPO_ROOT/scripts/e2e/build-local-ltembed-image.sh"
  if ! docker run --rm --entrypoint test "$IMAGE_TAG" -f /app/emit_static_lance_fixture; then
    echo "image $IMAGE_TAG still lacks /app/emit_static_lance_fixture after rebuild" >&2
    exit 1
  fi
fi

lhttp_init "$REPO_ROOT/docker-compose.local-ltembed.yml" ltsearch-real-static
trap 'lhttp_finish $?' EXIT

discover_ports() {
  WRITE_BASE="http://127.0.0.1:$(lhttp_port write)"
  BUILD_BASE="http://127.0.0.1:$(lhttp_port build)"
  QUERY_BASE="http://127.0.0.1:$(lhttp_port query)"
  echo "write=$WRITE_BASE build=$BUILD_BASE query=$QUERY_BASE" >&2
}

echo "=== Phase 1: 拓扑起立，激活前无静态语料痕迹 ===" >&2
lhttp_up
discover_ports

lhttp_assert_health health-write "$WRITE_BASE" ltsearch-write
lhttp_assert_health health-build "$BUILD_BASE" ltsearch-index-builder
lhttp_assert_health health-query "$QUERY_BASE" ltsearch-query
python3 - "$LHTTP_RUN_DIR/health-query.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body["index_version"] is None, body
# 激活前 health 不得携带 static_release_id 键（skip_serializing_if None）。
assert "static_release_id" not in body, body
PY

echo "=== Phase 2: 动态前置链路 + 纯动态对照组 ===" >&2
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
echo "published dynamic index_version=$PREV" >&2

# 激活前对照组：静态组为空、无 static_release_id 键、动态集合为写入全集。
# Phase 4 的动静共存断言以此为基线——静态激活不得扰动动态分组。
lhttp_request query-pre-static POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-pre-static
python3 - "$LHTTP_RUN_DIR/query-pre-static.response.json" "$FIXTURES/write_request.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
expected = {d["doc_id"] for d in json.load(open(sys.argv[2]))["documents"]}
assert r["static_count"] == len(r["static_chunks"]) == 0, r
assert "static_release_id" not in r, r
got = {c["doc_id"] for c in r["dynamic_chunks"]}
assert got == expected, (sorted(got), sorted(expected))
assert r["dynamic_count"] == len(r["dynamic_chunks"]), r
PY

# 在共享卷内完成一个 variant 的 stage：生成 dataset → static-build →
# static-activate。全部经一次性 compose 容器执行（复用服务定义的卷与
# LTSEARCH_BUILD_LTEMBED_* env，不发布端口）；-T 关 TTY 保证 stdout 可捕获。
# $1=variant(a|b) $2=目录后缀
stage_release() {
  local variant="$1" suffix="$2" table_version attempt rc

  lhttp_compose exec -T build mkdir -p "$STAGING"

  # bash 3.2 对 $var 紧跟全角字符的解析不安全，凡后接 CJK 一律用 ${var}。
  echo "--- 容器内生成 dataset-${suffix}（--embedder ltembed，真实 512 维）---" >&2
  table_version="$(lhttp_compose run --rm -T --no-deps \
    --entrypoint /app/emit_static_lance_fixture build \
    "$STAGING/dataset-$suffix" --variant "$variant" --embedder ltembed \
    | tr -d '\r' | tail -n1)"
  if ! [[ "$table_version" =~ ^[0-9]+$ ]] || [ "$table_version" -lt 1 ]; then
    echo "fixture emitter did not print a table version, got: '$table_version'" >&2
    exit 1
  fi
  echo "dataset-$suffix pinned at table_version=$table_version" >&2

  # config 由 host 侧 python 生成（避免 shell 转义脆弱拼接），落在 run dir
  # 兼作失败诊断物，再经 compose cp 拷进共享卷（目标服务须运行中）。
  python3 - "$LHTTP_RUN_DIR/static-config-$suffix.json" "$STAGING/dataset-$suffix" "$table_version" <<'PY'
import json, sys
config_path, dataset_path, table_version = sys.argv[1:4]
with open(config_path, "w") as f:
    json.dump(
        {
            "dataset_path": dataset_path,
            "table_version": int(table_version),
            "corpus_type": "legal",
            "embedding_profile": {"model_id": "jina-v5-nano/512", "dim": 512},
        },
        f,
    )
PY
  lhttp_compose cp "$LHTTP_RUN_DIR/static-config-$suffix.json" "build:$STAGING/static-config-$suffix.json"

  echo "--- static-build release-${suffix}（消费 dataset 内已有向量，不重嵌）---" >&2
  lhttp_compose run --rm -T --no-deps build \
    static-build --config "$STAGING/static-config-$suffix.json" --output "$STAGING/release-$suffix"

  echo "--- static-activate release-${suffix}（同卷 rename + SQLite CAS 指针）---" >&2
  # 不做同路径重试：activate 的安装步骤会把 --release 目录 rename 进受管存储，
  # 首次失败后原路径已不存在，重试注定失败（见 app.rs run_static_activate 注释）；
  # 瞬时 SQLite 锁由 schema 层 busy_timeout 兜底。失败即硬停，保留诊断。
  if ! lhttp_compose run --rm -T --no-deps build \
    static-activate --release "$STAGING/release-$suffix" --root /var/lib/ltsearch --expect-dim 512; then
    echo "static-activate failed for release-$suffix" >&2
    exit 1
  fi
}

echo "=== Phase 3: 容器内 stage release A ===" >&2
stage_release a a

echo "=== Phase 4: 静态检索契约（纯 HTTP 断言）===" >&2
lhttp_assert_health health-activated-a "$QUERY_BASE" ltsearch-query
RELEASE_A="$(python3 - "$LHTTP_RUN_DIR/health-activated-a.response.json" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
release_id = body.get("static_release_id")
# release id 的唯一来源是 HTTP：此处只断非空，具体取值不预设。
assert isinstance(release_id, str) and release_id, body
print(release_id)
PY
)"
echo "activated static_release_id=$RELEASE_A (via /health)" >&2

echo "--- 全量查询：静态集合/citation/corpus_type + 动静共存不融合 ---" >&2
lhttp_request query-static-a POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-static-a
python3 - "$LHTTP_RUN_DIR/query-static-a.response.json" "$STATIC_FIXTURES/expected_static_corpus.json" "$FIXTURES/write_request.json" "$RELEASE_A" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
corpus = json.load(open(sys.argv[2]))
dynamic_expected = {d["doc_id"] for d in json.load(open(sys.argv[3]))["documents"]}
release_a = sys.argv[4]
expected = {d["doc_id"]: d for d in corpus["documents"]}

# /query 与 /health 上报同一 release id（同代一致）。
assert r["static_release_id"] == release_a, r

# 静态组：全集命中（top_k=6 的检索窗口远大于 3 行语料），计数一致。
assert r["static_count"] == len(r["static_chunks"]) == len(expected), r
got = {c["doc_id"] for c in r["static_chunks"]}
assert got == set(expected), (sorted(got), sorted(expected))
for chunk in r["static_chunks"]:
    doc = expected[chunk["doc_id"]]
    assert chunk["chunk_source"] == "static", chunk
    assert chunk["source"] == "static", chunk
    assert chunk["corpus_type"] == corpus["corpus_type"], chunk
    # 换代判据的基线：release A 必须服务 variant a 的文本（Phase 5 对照）。
    assert chunk["text"] == doc["text"], (chunk, doc)
    citation = chunk["citation"]
    assert citation is not None, chunk
    for key in ("resource_id", "source_type", "source_ref", "title", "url"):
        assert citation[key] == doc[key], (chunk, doc)
    assert chunk["metadata"]["lang"] == doc["lang"], chunk

# 真实向量判别器：0.1 常量向量下三条分数必然并列，真实 embedding 至少产生
# 两个不同分数即可判别；TurboQuant 是量化近似打分，不同向量可能合法撞值，
# 不断言全体互异。列表仍须按分数降序。
scores = [c["score"] for c in r["static_chunks"]]
assert len(set(scores)) > 1, scores
assert scores == sorted(scores, reverse=True), scores

# 动静共存：动态组维持激活前基线，两组 doc_id 不相交、从不融合。
dynamic_got = {c["doc_id"] for c in r["dynamic_chunks"]}
assert dynamic_got == dynamic_expected, (sorted(dynamic_got), sorted(dynamic_expected))
assert r["dynamic_count"] == len(r["dynamic_chunks"]), r
assert got.isdisjoint(dynamic_got), (sorted(got), sorted(dynamic_got))
for chunk in r["dynamic_chunks"]:
    assert chunk["chunk_source"] == "dynamic", chunk
    assert chunk["source"] == "hybrid", chunk
    assert "corpus_type" not in chunk, chunk
PY

echo "--- lang:zh 后过滤：静态唯一 zh 行胜出，动态组无 zh 应为空 ---" >&2
lhttp_request query-static-zh POST "$QUERY_BASE/query" "$STATIC_FIXTURES/query_static_zh.json" >/dev/null
lhttp_assert_status 200 query-static-zh
python3 - "$LHTTP_RUN_DIR/query-static-zh.response.json" "$STATIC_FIXTURES/expected_static_corpus.json" "$RELEASE_A" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
corpus = json.load(open(sys.argv[2]))
zh_docs = {d["doc_id"]: d for d in corpus["documents"] if d["lang"] == "zh"}
assert len(zh_docs) == 1, zh_docs  # fixture 判别力：恰一条 zh 行
assert r["static_release_id"] == sys.argv[3], r
got = {c["doc_id"] for c in r["static_chunks"]}
assert got == set(zh_docs), (sorted(got), sorted(zh_docs))
top = r["static_chunks"][0]
(zh_doc,) = zh_docs.values()
assert top["doc_id"] == zh_doc["doc_id"], top
assert top["citation"]["resource_id"] == zh_doc["resource_id"], top
assert r["static_count"] == len(r["static_chunks"]) == 1, r
# 过滤统一作用于两组：动态语料无 lang:zh 文档，动态组必须为空。
assert r["dynamic_count"] == len(r["dynamic_chunks"]) == 0, r
PY

echo "--- include_metadata:false：metadata 剥离但 citation/corpus_type 保留 ---" >&2
lhttp_request query-static-no-meta POST "$QUERY_BASE/query" "$STATIC_FIXTURES/query_static_no_metadata.json" >/dev/null
lhttp_assert_status 200 query-static-no-meta
python3 - "$LHTTP_RUN_DIR/query-static-no-meta.response.json" "$LHTTP_RUN_DIR/query-static-a.response.json" <<'PY'
import json, sys
stripped = json.load(open(sys.argv[1]))
with_meta = json.load(open(sys.argv[2]))
assert stripped["static_release_id"] == with_meta["static_release_id"], stripped
# 投影唯一允许的差异是 metadata 置 null：按 doc_id 对齐后逐 chunk 全字段
# 相等（citation/corpus_type/text/score 等原样保留）。只断非空不足以证明
# citation 契约未被投影破坏——错误的非空值也能通过。
baseline = {c["doc_id"]: c for c in with_meta["static_chunks"]}
assert {c["doc_id"] for c in stripped["static_chunks"]} == set(baseline), (
    stripped,
    with_meta,
)
for chunk in stripped["static_chunks"]:
    assert chunk["metadata"] is None, chunk
    expected = dict(baseline[chunk["doc_id"]], metadata=None)
    assert chunk == expected, (chunk, expected)
PY

echo "--- lang:zh + include_metadata:false 组合：先过滤后剥离 ---" >&2
lhttp_request query-static-zh-no-meta POST "$QUERY_BASE/query" "$STATIC_FIXTURES/query_static_zh_no_metadata.json" >/dev/null
lhttp_assert_status 200 query-static-zh-no-meta
python3 - "$LHTTP_RUN_DIR/query-static-zh-no-meta.response.json" "$LHTTP_RUN_DIR/query-static-zh.response.json" <<'PY'
import json, sys
stripped = json.load(open(sys.argv[1]))
with_meta = json.load(open(sys.argv[2]))
# 与 no-meta 场景同一口径：先过滤后剥离，且剥离只动 metadata 一个字段。
baseline = {c["doc_id"]: c for c in with_meta["static_chunks"]}
assert {c["doc_id"] for c in stripped["static_chunks"]} == set(baseline), (
    stripped,
    with_meta,
)
for chunk in stripped["static_chunks"]:
    assert chunk["metadata"] is None, chunk
    expected = dict(baseline[chunk["doc_id"]], metadata=None)
    assert chunk == expected, (chunk, expected)
assert stripped["dynamic_count"] == len(stripped["dynamic_chunks"]) == 0, stripped
PY

echo "--- corpus_weights 对 static 同样 no-op：极端权重响应等价 ---" >&2
lhttp_request query-static-noop-a POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-static-noop-a
lhttp_request query-static-noop-a2 POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-static-noop-a2
lhttp_request query-static-noop-b POST "$QUERY_BASE/query" "$CONTRACT/query_weights_extreme.json" >/dev/null
lhttp_assert_status 200 query-static-noop-b
python3 - "$LHTTP_RUN_DIR/query-static-noop-a.response.json" "$LHTTP_RUN_DIR/query-static-noop-a2.response.json" "$LHTTP_RUN_DIR/query-static-noop-b.response.json" <<'PY'
import json, sys
def load(path):
    r = json.load(open(path))
    r.pop("latency_ms", None)
    return r
a, a2, b = (load(p) for p in sys.argv[1:4])
# A==A' 先证明同请求响应确定（静态排序有 doc_id 全序 tie-break），
# 再以 A==B 锁定 corpus_weights 对含静态组的响应同样是 no-op。
assert a == a2, "同请求两次响应不一致：no-op 判别失效"
assert a == b, "带极端 corpus_weights 的响应有差异：no-op 契约被破坏"
assert a["static_count"] > 0, a  # 判别力：对照发生在静态语料激活之后
PY

echo "=== Phase 5: variant b 重建并重新激活 → release id 经 HTTP 翻转 ===" >&2
stage_release b b

lhttp_assert_health health-activated-b "$QUERY_BASE" ltsearch-query
RELEASE_B="$(python3 - "$LHTTP_RUN_DIR/health-activated-b.response.json" "$RELEASE_A" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
release_id = body.get("static_release_id")
assert isinstance(release_id, str) and release_id, body
# variant b 只改一行英文文本，内容指纹即变：新 id 必须不同于 A。
assert release_id != sys.argv[2], body
print(release_id)
PY
)"
echo "flipped static_release_id=$RELEASE_B (via /health)" >&2

lhttp_request query-static-b POST "$QUERY_BASE/query" "$FIXTURES/query_request_real.json" >/dev/null
lhttp_assert_status 200 query-static-b
python3 - "$LHTTP_RUN_DIR/query-static-b.response.json" "$STATIC_FIXTURES/expected_static_corpus.json" "$FIXTURES/write_request.json" "$RELEASE_A" "$RELEASE_B" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
corpus = json.load(open(sys.argv[2]))
dynamic_expected = {d["doc_id"] for d in json.load(open(sys.argv[3]))["documents"]}
release_a, release_b = sys.argv[4], sys.argv[5]
assert r["static_release_id"] == release_b, r
assert r["static_release_id"] != release_a, r
# 换代后静态集合不变（三行语料 doc_id 同名），动态组不受扰动。
assert {c["doc_id"] for c in r["static_chunks"]} == {
    d["doc_id"] for d in corpus["documents"]
}, r
assert r["static_count"] == len(r["static_chunks"]), r
assert {c["doc_id"] for c in r["dynamic_chunks"]} == dynamic_expected, r
# 换代必须体现在语料内容上：仅报新 release id 而 query 仍服务 A 代
# mmap 的失败模式在此被拦截。variant b 唯一可观察差异是 doc-gamma 文本。
gamma = {d["doc_id"]: d for d in corpus["documents"]}["doc-gamma"]
assert gamma["text_variant_b"] != gamma["text"], gamma
(gamma_chunk,) = [c for c in r["static_chunks"] if c["doc_id"] == "doc-gamma"]
assert gamma_chunk["text"] == gamma["text_variant_b"], (gamma_chunk, gamma)
PY

lhttp_request query-static-zh-b POST "$QUERY_BASE/query" "$STATIC_FIXTURES/query_static_zh.json" >/dev/null
lhttp_assert_status 200 query-static-zh-b
python3 - "$LHTTP_RUN_DIR/query-static-zh-b.response.json" "$RELEASE_B" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
# doc-alpha 行不随 variant 变：zh 过滤断言跨代稳定。
assert r["static_chunks"][0]["doc_id"] == "doc-alpha", r
assert r["static_release_id"] == sys.argv[2], r
PY

echo "--- 静态语料 HTTP 契约全部通过 (release a=$RELEASE_A -> b=$RELEASE_B, dynamic v>=$PREV) ---" >&2
