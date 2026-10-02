# shellcheck shell=bash
# LTEmbed 资产与链接 pin 读取（source 用）。单一来源是 sam/builder.Dockerfile 的
# ARG 默认值；同名环境变量非空时优先（显式覆盖）。
#   LTEMBED_GGUF_URL / LTEMBED_GGUF_SHA256             GGUF 权重（model.gguf）
#   LTEMBED_TOKENIZER_URL / LTEMBED_TOKENIZER_SHA256   tokenizer.json
#   STATIC_LLAMA_URL / STATIC_LLAMA_SHA256             预编译静态 llama.cpp（仅 real 编译链接用，
#                                                      scripts/fetch-static-llama.sh 同源读取）
LTEMBED_ASSET_PIN_NAMES=(LTEMBED_GGUF_URL LTEMBED_GGUF_SHA256 LTEMBED_TOKENIZER_URL LTEMBED_TOKENIZER_SHA256)
LTEMBED_PIN_NAMES=("${LTEMBED_ASSET_PIN_NAMES[@]}" STATIC_LLAMA_URL STATIC_LLAMA_SHA256)

# ltembed_pin <NAME>：打印 pin 值；取不到即失败。
ltembed_pin() {
  local name="$1" value="${!1:-}"
  local dockerfile
  dockerfile="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/sam/builder.Dockerfile"
  if [[ -z "$value" ]]; then
    value="$(sed -n "s/^ARG ${name}=//p" "$dockerfile")"
  fi
  if [[ -z "$value" ]]; then
    echo "failed to extract LTEmbed pin $name from sam/builder.Dockerfile" >&2
    return 1
  fi
  printf '%s\n' "$value"
}

# ltembed_pin_build_args <NAME>...：把给定 pin 展开成 `--build-arg NAME=value` 序列到
# 全局数组 LTEMBED_PIN_BUILD_ARGS（docker build 透传用）。
ltembed_pin_build_args() {
  LTEMBED_PIN_BUILD_ARGS=()
  local name value
  for name in "$@"; do
    value="$(ltembed_pin "$name")" || return 1
    LTEMBED_PIN_BUILD_ARGS+=(--build-arg "$name=$value")
  done
}
