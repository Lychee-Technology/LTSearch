#!/usr/bin/env bash
# 打包 3 个 Lambda ZIP（#109）：在 AL2023 builder 镜像内编译（glibc 2.34 兼容
# provided.al2023 运行时；ubuntu 宿主机原生编译会链接更新的 glibc 符号），提取
# 二进制改名 bootstrap 置于 zip 根。不依赖第三方打包工具。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}"
readonly BUILDER_IMAGE="${LTSEARCH_BUILDER_IMAGE:-ltsearch-lambda-zip-builder}"
# stub = features lambda（fixed embedding，e2e/CI 用）；real = features lambda,ltembed。
readonly LTEMBED_MODE="${LTSEARCH_LTEMBED_MODE:-stub}"

docker build \
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
  (cd "$fn_dir" && zip -q -X "$DIST_DIR/$fn.zip" bootstrap)
done

echo "packaged lambda zips into $DIST_DIR" >&2
