#!/usr/bin/env bash
# 打包 3 个 Lambda ZIP（#109）：在 AL2023 builder 镜像内编译（glibc 2.34 兼容
# provided.al2023 运行时；ubuntu 宿主机原生编译会链接更新的 glibc 符号），提取
# 二进制改名 bootstrap 置于 zip 根。不依赖第三方打包工具。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}"
readonly BUILDER_IMAGE="${LTSEARCH_BUILDER_IMAGE:-ltsearch-lambda-zip-builder}"
# stub = features lambda（fixed embedding；e2e/CI 用，配 --env-vars 覆盖回 fixed）；
# real = features lambda,ltembed（生产档，模型资产由 S3→/tmp 冷启动供给，#111；
# 需 .sam-local-deps/LTEmbed vendored checkout）。
readonly LTEMBED_MODE="${LTSEARCH_LTEMBED_MODE:-stub}"
# 可复现性（#113 review P1）：zip 会把文件 mtime 写进条目头，docker cp 出来的
# mtime 是构建时刻——归一化到 SOURCE_DATE_EPOCH（默认 HEAD 提交时间）并以
# TZ=UTC 打包（zip 存 DOS 本地时间），同一 commit 的产物字节稳定。
readonly SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$REPO_ROOT" log -1 --format=%ct)}"

DOCKER_BUILDKIT=1 docker build \
  --platform linux/arm64 \
  --build-arg LTEMBED_MODE="$LTEMBED_MODE" \
  --tag "$BUILDER_IMAGE" \
  --file "$REPO_ROOT/sam/builder.Dockerfile" \
  "$REPO_ROOT"

container_id="$(docker create --platform linux/arm64 "$BUILDER_IMAGE")"
trap 'docker rm -f "$container_id" >/dev/null' EXIT

mkdir -p "$DIST_DIR"
for fn in query_lambda write_lambda index_builder_lambda; do
  fn_dir="$DIST_DIR/$fn"
  rm -rf "$fn_dir" "$DIST_DIR/$fn.zip"
  mkdir -p "$fn_dir"
  docker cp "$container_id:/$fn" "$fn_dir/bootstrap"
  chmod +x "$fn_dir/bootstrap"
done

# strip 在 builder 镜像内做（宿主机无 aarch64 binutils，镜像随 gcc 自带）。
# #111 实测：real query 二进制解压 235.7 MiB，距 Lambda 250MB 单函数硬限仅
# ~14 MiB；strip 回收 ~55 MiB（→180.6 MiB）。仅 ZIP lineage strip，镜像
# lineage 保留符号便于诊断。
docker run --rm --platform linux/arm64 \
  --mount "type=bind,source=$DIST_DIR,target=/dist" \
  "$BUILDER_IMAGE" \
  bash -c 'strip /dist/query_lambda/bootstrap /dist/write_lambda/bootstrap /dist/index_builder_lambda/bootstrap'

for fn in query_lambda write_lambda index_builder_lambda; do
  python3 -c 'import os, sys; t = int(sys.argv[1]); os.utime(sys.argv[2], (t, t))' \
    "$SOURCE_DATE_EPOCH" "$DIST_DIR/$fn/bootstrap"
  (cd "$DIST_DIR/$fn" && TZ=UTC zip -q -X "$DIST_DIR/$fn.zip" bootstrap)
done

echo "packaged lambda zips into $DIST_DIR" >&2
