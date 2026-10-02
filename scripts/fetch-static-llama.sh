#!/usr/bin/env bash
# 获取 LTEmbed llama.cpp 后端链接所需的预编译静态库（LTEmbed#149 起 ltembed
# build.rs 强制要求 STATIC_LLAMA_DIR）：下载 static-llama-cpp-rs-builder release
# tarball → 仓库钉住的 sha256 校验 → 解压 → release 自带 SHA256SUMS 复核 →
# artifact_contract_version 断言（LTEmbed 按 contract v2 编写），最后在 stdout
# 打印可直接用作 STATIC_LLAMA_DIR 的目录（日志走 stderr）。
#
# pin 单一来源：sam/builder.Dockerfile 的 ARG STATIC_LLAMA_URL / STATIC_LLAMA_SHA256
# （与 LTEmbed 自身 CI 钉同一 release）；同名环境变量非空时优先——Docker 构建中
# ARG 即环境变量，走这条，不需要仓库文件。
#
# 幂等：目标目录已有同一 sha256 的校验通过标记即直接复用（不重新下载）。
# release 是公开资产，只需 curl，不依赖 gh/GH_TOKEN。
#
# 用法：export STATIC_LLAMA_DIR="$(scripts/fetch-static-llama.sh [dest_dir])"
#   dest_dir 默认 <repo>/target/static-llama（已 git/docker ignore）。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly PIN_FILE="$REPO_ROOT/sam/builder.Dockerfile"
readonly EXPECTED_CONTRACT=2
dest="${1:-$REPO_ROOT/target/static-llama}"

pin() {
  local name="$1" value="${!1:-}"
  if [[ -z "$value" && -f "$PIN_FILE" ]]; then
    value="$(sed -n "s/^ARG ${name}=//p" "$PIN_FILE")"
  fi
  if [[ -z "$value" ]]; then
    echo "fetch-static-llama: $name not set and no pin found in $PIN_FILE" >&2
    return 1
  fi
  echo "$value"
}

url="$(pin STATIC_LLAMA_URL)"
sha256="$(pin STATIC_LLAMA_SHA256)"
extracted="$dest/extracted"
marker="$dest/.verified-sha256"

if [[ -f "$marker" && "$(cat "$marker")" == "$sha256" && -f "$extracted/lib/libllama.a" ]]; then
  echo "$extracted"
  exit 0
fi

rm -rf "$dest"
mkdir -p "$extracted"
tarball="$dest/static-llama.tar.gz"
echo "fetch-static-llama: downloading $url" >&2
curl -fsSL --retry 3 "$url" -o "$tarball"
echo "$sha256  $tarball" | sha256sum -c - >&2
tar -xzf "$tarball" -C "$extracted"
rm -f "$tarball"
(cd "$extracted" && sha256sum --quiet -c SHA256SUMS) >&2

contract="$(sed -n 's/.*"artifact_contract_version": *"\([0-9]*\)".*/\1/p' "$extracted/build-info.json" | head -n 1)"
if [[ "$contract" != "$EXPECTED_CONTRACT" ]]; then
  echo "fetch-static-llama: unexpected artifact_contract_version '$contract' (expected $EXPECTED_CONTRACT)" >&2
  exit 1
fi

echo "$sha256" > "$marker"
echo "fetch-static-llama: verified static llama.cpp (contract v$contract) at $extracted" >&2
echo "$extracted"
