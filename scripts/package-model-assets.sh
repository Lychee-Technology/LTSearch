#!/usr/bin/env bash
# 打包 LTEmbed 模型资产（#111，S3→/tmp 路线）：只构建 sam/builder.Dockerfile 的
# bundle stage（无 cargo 编译、无 LTEmbed 源 checkout 依赖），GGUF bundle
# （model.gguf + tokenizer.json + build-info.json）平铺到 dist/model-assets/ 并写
# manifest.json（逐源 URL/sha256 provenance + 逐文件 sha256/bytes）。
# 部署前整目录上传到函数可读的 S3 前缀：
#   aws s3 cp --recursive dist/model-assets s3://<bucket>/<ModelAssetPrefix>/
# query/index-builder 冷启动按 manifest 下载校验到 /tmp/ltembed（src/embedding/
# model_assets.rs）。GGUF/tokenizer URL + sha256 pin 的单一来源是 builder.Dockerfile
# 的 ARG 默认值（scripts/ltembed-pins.sh 读取，同名环境变量可显式覆盖）；解析后的值
# 同时透传 build-arg 并写入 manifest，保证 provenance 与实际构建一致。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}"
readonly BUNDLE_IMAGE="${LTSEARCH_BUNDLE_IMAGE:-ltsearch-model-bundle}"

# shellcheck source=scripts/ltembed-pins.sh
source "$REPO_ROOT/scripts/ltembed-pins.sh"
ltembed_pin_build_args "${LTEMBED_ASSET_PIN_NAMES[@]}"

DOCKER_BUILDKIT=1 docker build \
  --platform linux/arm64 \
  --target bundle \
  --build-arg LTEMBED_MODE=real \
  "${LTEMBED_PIN_BUILD_ARGS[@]}" \
  --tag "$BUNDLE_IMAGE" \
  --file "$REPO_ROOT/sam/builder.Dockerfile" \
  "$REPO_ROOT"

container_id="$(docker create --platform linux/arm64 "$BUNDLE_IMAGE")"
trap 'docker rm -f "$container_id" >/dev/null' EXIT

assets_dir="$DIST_DIR/model-assets"
rm -rf "$assets_dir"
mkdir -p "$assets_dir"
docker cp "$container_id:/ltembed-assets/." "$assets_dir/"

# manifest 里的 pin 值与上方 build-arg 同源（ltembed_pin），显式覆盖时以环境变量为准。
gguf_url="$(ltembed_pin LTEMBED_GGUF_URL)"
gguf_sha256="$(ltembed_pin LTEMBED_GGUF_SHA256)"
tokenizer_url="$(ltembed_pin LTEMBED_TOKENIZER_URL)"
tokenizer_sha256="$(ltembed_pin LTEMBED_TOKENIZER_SHA256)"

LTSEARCH_ASSETS_GGUF_URL="$gguf_url" LTSEARCH_ASSETS_GGUF_SHA256="$gguf_sha256" \
LTSEARCH_ASSETS_TOKENIZER_URL="$tokenizer_url" LTSEARCH_ASSETS_TOKENIZER_SHA256="$tokenizer_sha256" \
python3 - "$assets_dir" <<'PY'
import hashlib
import json
import os
import pathlib
import sys

assets_dir = pathlib.Path(sys.argv[1])
files = []
for path in sorted(assets_dir.iterdir()):
    if path.name == "manifest.json":
        continue
    files.append(
        {
            "name": path.name,
            "bytes": path.stat().st_size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        }
    )
# GGUF bundle 与架构无关（llama.cpp 静态链接进二进制，不随资产分发）；
# build-info.json 来自仓库 sam/ltembed-build-info.json，无外部源。
manifest = {
    "bundle_format": "gguf",
    "sources": {
        "model.gguf": {
            "url": os.environ["LTSEARCH_ASSETS_GGUF_URL"],
            "sha256": os.environ["LTSEARCH_ASSETS_GGUF_SHA256"],
        },
        "tokenizer.json": {
            "url": os.environ["LTSEARCH_ASSETS_TOKENIZER_URL"],
            "sha256": os.environ["LTSEARCH_ASSETS_TOKENIZER_SHA256"],
        },
    },
    "tmp_path": "/tmp/ltembed",
    "files": files,
}
(assets_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
PY

echo "packaged model assets into $assets_dir" >&2
