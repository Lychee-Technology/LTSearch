#!/usr/bin/env bash
# 打包 LTEmbed 模型资产（#111，S3→/tmp 路线）：只构建 sam/builder.Dockerfile 的
# bundle stage（无 cargo 编译、无 LTEmbed 源 checkout 依赖），资产平铺到
# dist/model-assets/ 并写 manifest.json（bundle provenance + 逐文件 sha256/bytes）。
# 部署前整目录上传到函数可读的 S3 前缀：
#   aws s3 cp --recursive dist/model-assets s3://<bucket>/<ModelAssetPrefix>/
# query/index-builder 冷启动按 manifest 下载校验到 /tmp/ltembed（src/embedding/
# model_assets.rs）。bundle URL + sha256 pin 的单一来源是 builder.Dockerfile 的
# ARG 默认值；此处仅在显式覆盖时透传 build-arg。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}"
readonly BUNDLE_IMAGE="${LTSEARCH_BUNDLE_IMAGE:-ltsearch-model-bundle}"

DOCKER_BUILDKIT=1 docker build \
  --platform linux/arm64 \
  --target bundle \
  --build-arg LTEMBED_MODE=real \
  ${LTEMBED_BUNDLE_URL:+--build-arg LTEMBED_BUNDLE_URL="$LTEMBED_BUNDLE_URL"} \
  ${LTEMBED_BUNDLE_SHA256:+--build-arg LTEMBED_BUNDLE_SHA256="$LTEMBED_BUNDLE_SHA256"} \
  --tag "$BUNDLE_IMAGE" \
  --file "$REPO_ROOT/sam/builder.Dockerfile" \
  "$REPO_ROOT"

container_id="$(docker create --platform linux/arm64 "$BUNDLE_IMAGE")"
trap 'docker rm -f "$container_id" >/dev/null' EXIT

assets_dir="$DIST_DIR/model-assets"
rm -rf "$assets_dir"
mkdir -p "$assets_dir"
docker cp "$container_id:/ltembed-assets/." "$assets_dir/"

# manifest 里的 pin 值直接从 builder.Dockerfile 的 ARG 默认值提取（单一来源），
# 显式覆盖时以环境变量为准。
bundle_url="${LTEMBED_BUNDLE_URL:-$(sed -n 's/^ARG LTEMBED_BUNDLE_URL=//p' "$REPO_ROOT/sam/builder.Dockerfile")}"
bundle_sha256="${LTEMBED_BUNDLE_SHA256:-$(sed -n 's/^ARG LTEMBED_BUNDLE_SHA256=//p' "$REPO_ROOT/sam/builder.Dockerfile")}"

LTSEARCH_ASSETS_BUNDLE_URL="$bundle_url" LTSEARCH_ASSETS_BUNDLE_SHA256="$bundle_sha256" \
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
manifest = {
    "bundle_url": os.environ["LTSEARCH_ASSETS_BUNDLE_URL"],
    "bundle_sha256": os.environ["LTSEARCH_ASSETS_BUNDLE_SHA256"],
    "arch": "aarch64",
    "tmp_path": "/tmp/ltembed",
    "files": files,
}
(assets_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
PY

echo "packaged model assets into $assets_dir" >&2
