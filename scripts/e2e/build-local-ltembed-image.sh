#!/usr/bin/env bash
set -euo pipefail
# 构建 real-LTEmbed 本地单镜像（#141）：
#   1. 按 Cargo.lock 锁定 rev 物化 LTEmbed checkout 到 .sam-local-deps/LTEmbed
#      （不取 sibling/nested 工作区 HEAD——可能已越过 lock）；
#   2. 从 sam/builder.Dockerfile 提取 GGUF/tokenizer/static llama pin（单一来源，
#      scripts/ltembed-pins.sh，与 scripts/package-model-assets.sh 同一套提取方式）；
#   3. docker build linux/arm64 出 ltsearch-local-ltembed:dev（可经
#      LTSEARCH_LOCAL_LTEMBED_IMAGE 覆盖 tag）。
# 环境覆盖：LTEMBED_{GGUF,TOKENIZER}_{URL,SHA256} / STATIC_LLAMA_{URL,SHA256}（默认取 pin）。
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMAGE_TAG="${LTSEARCH_LOCAL_LTEMBED_IMAGE:-ltsearch-local-ltembed:dev}"

# shellcheck source=scripts/e2e/lib.sh
source "$REPO_ROOT/scripts/e2e/lib.sh"

prepare_locked_ltembed_checkout "$REPO_ROOT"

# shellcheck source=scripts/ltembed-pins.sh
source "$REPO_ROOT/scripts/ltembed-pins.sh"
ltembed_pin_build_args "${LTEMBED_PIN_NAMES[@]}"

echo "--- docker build $IMAGE_TAG (linux/arm64, features local,ltembed) ---" >&2
docker build --platform linux/arm64 \
  -f "$REPO_ROOT/sam/local-ltembed.Dockerfile" \
  "${LTEMBED_PIN_BUILD_ARGS[@]}" \
  -t "$IMAGE_TAG" \
  "$REPO_ROOT"

arch="$(docker inspect --format '{{.Architecture}}' "$IMAGE_TAG")"
if [[ "$arch" != "arm64" ]]; then
  echo "image architecture must be arm64, got: $arch" >&2
  exit 1
fi
echo "built $IMAGE_TAG (arm64)" >&2
