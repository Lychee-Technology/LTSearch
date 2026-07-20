#!/usr/bin/env bash
# release 组装器（#113）：把既有打包脚本的产物收拢成一套可发布的 release 载荷
#   dist/release/{query_lambda.zip,write_lambda.zip,index_builder_lambda.zip,
#                 model-assets.zip,release-provenance.json,SHA256SUMS}
# 组装链：package-lambda-zips.sh（LTSEARCH_LTEMBED_MODE 透传）→
# package-model-assets.sh → check-lambda-size-budget.sh（发布前闸门）→
# 收拢 + provenance + checksums。不重复任何构建逻辑；不引入第三方打包工具。
#
# 用法：package-release.sh [--mode real|stub] [--version <tag>]
#   --mode     Lambda ZIP 的 embedding 档（默认 real；CI 组装校验用 stub）。
#              model-assets 恒为 real bundle（资产本身无 stub 形态）。
#   --version  记入 provenance 的版本（默认 git describe --tags --exact-match，
#              否则 dev-<short-sha>）。
# GITHUB_REPOSITORY/GITHUB_RUN_ID/GITHUB_SERVER_URL 存在时 provenance 记录
# workflow 运行信息，本地运行为 null。哈希在 python 内做（macOS 无 sha256sum）。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}"
readonly RELEASE_DIR="$DIST_DIR/release"

mode="real"
version=""
while [ $# -gt 0 ]; do
  case "$1" in
    --mode)
      mode="${2:?--mode requires real|stub}"
      shift 2
      ;;
    --version)
      version="${2:?--version requires a value}"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      echo "usage: package-release.sh [--mode real|stub] [--version <tag>]" >&2
      exit 2
      ;;
  esac
done

case "$mode" in
  real | stub) ;;
  *)
    echo "invalid --mode '$mode' (expected real|stub)" >&2
    exit 2
    ;;
esac

if [ -z "$version" ]; then
  version="$(git -C "$REPO_ROOT" describe --tags --exact-match 2>/dev/null ||
    echo "dev-$(git -C "$REPO_ROOT" rev-parse --short HEAD)")"
fi

LTSEARCH_LTEMBED_MODE="$mode" bash "$REPO_ROOT/scripts/package-lambda-zips.sh"
bash "$REPO_ROOT/scripts/package-model-assets.sh"
bash "$REPO_ROOT/scripts/check-lambda-size-budget.sh" "$DIST_DIR"

rm -rf "$RELEASE_DIR"
mkdir -p "$RELEASE_DIR"
for fn in query_lambda write_lambda index_builder_lambda; do
  cp "$DIST_DIR/$fn.zip" "$RELEASE_DIR/$fn.zip"
done
# model-assets.zip 顶层含 model-assets/ 目录（解压后可整目录
# `aws s3 cp --recursive model-assets s3://<bucket>/<ModelAssetPrefix>/`）。
(cd "$DIST_DIR" && zip -q -r -X "$RELEASE_DIR/model-assets.zip" model-assets)

# bundle pin 的单一来源是 sam/builder.Dockerfile 的 ARG 默认值（与
# package-model-assets.sh 同一提取模式），显式覆盖时以环境变量为准。
bundle_url="${LTEMBED_BUNDLE_URL:-$(sed -n 's/^ARG LTEMBED_BUNDLE_URL=//p' "$REPO_ROOT/sam/builder.Dockerfile")}"
bundle_sha256="${LTEMBED_BUNDLE_SHA256:-$(sed -n 's/^ARG LTEMBED_BUNDLE_SHA256=//p' "$REPO_ROOT/sam/builder.Dockerfile")}"
git_sha="$(git -C "$REPO_ROOT" rev-parse HEAD)"

LTSEARCH_RELEASE_VERSION="$version" \
  LTSEARCH_RELEASE_GIT_SHA="$git_sha" \
  LTSEARCH_RELEASE_MODE="$mode" \
  LTSEARCH_RELEASE_BUNDLE_URL="$bundle_url" \
  LTSEARCH_RELEASE_BUNDLE_SHA256="$bundle_sha256" \
  python3 - "$RELEASE_DIR" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import sys

release_dir = pathlib.Path(sys.argv[1])
version = os.environ["LTSEARCH_RELEASE_VERSION"]

ARTIFACTS = (
    "query_lambda.zip",
    "write_lambda.zip",
    "index_builder_lambda.zip",
    "model-assets.zip",
)


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


workflow = None
if os.environ.get("GITHUB_RUN_ID"):
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    server_url = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    run_id = os.environ["GITHUB_RUN_ID"]
    workflow = {
        "repository": repository,
        "run_id": run_id,
        "run_url": f"{server_url}/{repository}/actions/runs/{run_id}",
    }

provenance = {
    "schema_version": 1,
    "tag": version,
    "git_sha": os.environ["LTSEARCH_RELEASE_GIT_SHA"],
    "built_at": datetime.datetime.now(datetime.timezone.utc)
    .replace(microsecond=0)
    .isoformat()
    .replace("+00:00", "Z"),
    "workflow": workflow,
    "ltembed_mode": os.environ["LTSEARCH_RELEASE_MODE"],
    "ltembed_bundle": {
        "url": os.environ["LTSEARCH_RELEASE_BUNDLE_URL"],
        "sha256": os.environ["LTSEARCH_RELEASE_BUNDLE_SHA256"],
    },
    # 镜像 digest 有意不记：push 前未知，registry 是 digest 的权威来源；
    # ref+tag+dockerfile 足以复现构建。
    "local_image": {
        "ref": f"ghcr.io/lychee-technology/ltsearch-local:{version}",
        "platform": "linux/arm64",
        "dockerfile": "sam/local.Dockerfile",
    },
    "artifacts": [
        {
            "name": name,
            "bytes": (release_dir / name).stat().st_size,
            "sha256": sha256(release_dir / name),
        }
        for name in ARTIFACTS
    ],
}
provenance_path = release_dir / "release-provenance.json"
provenance_path.write_text(json.dumps(provenance, indent=2) + "\n")

# `sha256sum -c` 兼容格式（两空格）。覆盖 4 个 zip + provenance 本身。
sums = [
    f"{sha256(release_dir / name)}  {name}"
    for name in (*ARTIFACTS, provenance_path.name)
]
(release_dir / "SHA256SUMS").write_text("\n".join(sums) + "\n")
PY

echo "assembled release payload ($mode, $version) into $RELEASE_DIR" >&2
