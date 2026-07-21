#!/usr/bin/env bash
set -euo pipefail
# 构建 real-LTEmbed 本地单镜像（#141）：
#   1. 物化 LTEmbed 源 checkout 到 .sam-local-deps/LTEmbed（Cargo.lock rev）；
#   2. 从 sam/builder.Dockerfile 提取 bundle pin（单一来源，与
#      scripts/package-model-assets.sh 同一套提取方式）；
#   3. docker build linux/arm64 出 ltsearch-local-ltembed:dev（可经
#      LTSEARCH_LOCAL_LTEMBED_IMAGE 覆盖 tag）。
# 环境覆盖：LTEMBED_BUNDLE_URL / LTEMBED_BUNDLE_SHA256（默认取 pin）。
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMAGE_TAG="${LTSEARCH_LOCAL_LTEMBED_IMAGE:-ltsearch-local-ltembed:dev}"

# shellcheck source=scripts/e2e/lib.sh
source "$REPO_ROOT/scripts/e2e/lib.sh"

prepare_local_ltembed_checkout "$REPO_ROOT"

bundle_url="${LTEMBED_BUNDLE_URL:-$(sed -n 's/^ARG LTEMBED_BUNDLE_URL=//p' "$REPO_ROOT/sam/builder.Dockerfile")}"
bundle_sha256="${LTEMBED_BUNDLE_SHA256:-$(sed -n 's/^ARG LTEMBED_BUNDLE_SHA256=//p' "$REPO_ROOT/sam/builder.Dockerfile")}"
if [[ -z "$bundle_url" || -z "$bundle_sha256" ]]; then
  echo "failed to extract LTEmbed bundle pin from sam/builder.Dockerfile" >&2
  exit 1
fi

echo "--- docker build $IMAGE_TAG (linux/arm64, features local,ltembed) ---" >&2
docker build --platform linux/arm64 \
  -f "$REPO_ROOT/sam/local-ltembed.Dockerfile" \
  --build-arg LTEMBED_BUNDLE_URL="$bundle_url" \
  --build-arg LTEMBED_BUNDLE_SHA256="$bundle_sha256" \
  -t "$IMAGE_TAG" \
  "$REPO_ROOT"

arch="$(docker inspect --format '{{.Architecture}}' "$IMAGE_TAG")"
if [[ "$arch" != "arm64" ]]; then
  echo "image architecture must be arm64, got: $arch" >&2
  exit 1
fi
echo "built $IMAGE_TAG (arm64)" >&2
