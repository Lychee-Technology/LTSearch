# GGUF bundle 下载/校验独立成 bundle stage（#111）：Layer 打包只 build 该 stage
# （--target bundle），不触发 cargo 编译、不需要 LTEmbed 源 checkout。
#
# 可复现性（#113 review P1）：base 镜像按 digest pin（对应 AL2023 release
# 2023.12.20260710），dnf 以 /etc/dnf/vars/releasever 锁定同一 release 快照
# （AL2023 默认 releasever=latest 是可变仓库指针）。bump base 时两处一起更新。
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS bundle
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
ARG LTEMBED_MODE=stub
# LTEmbed GGUF bundle（LTEmbed#149 起后端为 llama.cpp）for
# jina-embeddings-v5-text-nano-retrieval：model.gguf + tokenizer.json 按 HuggingFace
# commit 固定的 resolve URL 下载并逐文件 sha256 校验；build-info.json 取自仓库内
# sam/ltembed-build-info.json（与 LTEmbed release-bundles.yml 同构）。Q5_K_M 是上游
# 推荐量化（过 cosine ≥0.99 parity 门禁，体积适配 Lambda）。bump 时 URL 与 SHA256
# 成对更新；换量化还要同步 build-info.json 的 quant。
ARG LTEMBED_GGUF_URL=https://huggingface.co/jinaai/jina-embeddings-v5-text-nano-retrieval/resolve/ac5d898c8d382b17167c33e5c8af644a3519b47d/v5-nano-retrieval-Q5_K_M.gguf
ARG LTEMBED_GGUF_SHA256=46fbc0423862cb6a5d4ff776d885f349d2a87c36d821dd5630f9fa184c9b4b92
ARG LTEMBED_TOKENIZER_URL=https://huggingface.co/jinaai/jina-embeddings-v5-text-nano-retrieval/resolve/ac5d898c8d382b17167c33e5c8af644a3519b47d/tokenizer.json
ARG LTEMBED_TOKENIZER_SHA256=98d4a1d32152d6cedf85b5e88f3b205106dca1fe72aaab34e0ac13c238421069
COPY sam/ltembed-build-info.json /tmp/ltembed-build-info.json
RUN mkdir -p /ltembed-assets && \
    if [ "$LTEMBED_MODE" != "stub" ]; then \
      if [ -z "$LTEMBED_GGUF_URL" ] || [ -z "$LTEMBED_GGUF_SHA256" ] || \
         [ -z "$LTEMBED_TOKENIZER_URL" ] || [ -z "$LTEMBED_TOKENIZER_SHA256" ]; then \
        echo "LTEMBED_MODE=real requires LTEMBED_{GGUF,TOKENIZER}_{URL,SHA256} (GGUF bundle pins)" >&2; \
        exit 1; \
      fi; \
      curl -fSL --retry 3 "$LTEMBED_GGUF_URL" -o /ltembed-assets/model.gguf && \
      echo "$LTEMBED_GGUF_SHA256  /ltembed-assets/model.gguf" | sha256sum -c - && \
      curl -fSL --retry 3 "$LTEMBED_TOKENIZER_URL" -o /ltembed-assets/tokenizer.json && \
      echo "$LTEMBED_TOKENIZER_SHA256  /ltembed-assets/tokenizer.json" | sha256sum -c - && \
      cp /tmp/ltembed-build-info.json /ltembed-assets/build-info.json; \
    fi

FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS builder
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
RUN dnf install -y --allowerasing gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.1
ENV PATH="/root/.cargo/bin:${PATH}"
ARG LTEMBED_MODE=stub
# LTEmbed build.rs 静态链接预编译 llama.cpp（static-llama-cpp-rs-builder release，
# Graviton2/neoverse-n1 调优，artifact contract v2；与 LTEmbed CI 钉同一 release）。
# real 模式在 COPY . . 之前取用并校验（tarball sha256 + SHA256SUMS + contract），
# 源码改动不使该层失效。bump 时 URL 与 SHA256 成对更新。
ARG STATIC_LLAMA_URL=https://github.com/Lychee-Technology/static-llama-cpp-rs-builder/releases/download/v0.1.151-1/static-llama-cpp-v0.1.151-1-aarch64-graviton2.tar.gz
ARG STATIC_LLAMA_SHA256=48f3aa293824086d667d3938b40be37390d31275af20fd20a88c3880bfee2b90
COPY scripts/fetch-static-llama.sh /opt/ltsearch-build/fetch-static-llama.sh
RUN if [ "$LTEMBED_MODE" != "stub" ]; then \
      /opt/ltsearch-build/fetch-static-llama.sh /opt/static-llama >/dev/null; \
    fi
COPY --from=bundle /ltembed-assets /ltembed-assets
WORKDIR /src
COPY . .
RUN if [ "$LTEMBED_MODE" = "stub" ]; then \
      printf '\n[patch."https://github.com/Lychee-Technology/LTEmbed"]\nltembed = { path = "/src/vendor/ltembed-stub" }\n' >> /src/.cargo/config.toml; \
    else \
      printf '\n[patch."https://github.com/Lychee-Technology/LTEmbed"]\nltembed = { path = "/src/.sam-local-deps/LTEmbed" }\n' >> /src/.cargo/config.toml; \
    fi
RUN --mount=type=cache,id=ltsearch-cargo-registry,target=/root/.cargo/registry \
    --mount=type=cache,id=ltsearch-cargo-git,target=/root/.cargo/git \
    --mount=type=cache,id=ltsearch-cargo-target,target=/src/target \
    if [ "$LTEMBED_MODE" = "stub" ]; then \
      cargo build --release --no-default-features --features lambda \
          --bin write_lambda \
          --bin index_builder_lambda \
          --bin query_lambda; \
    else \
      STATIC_LLAMA_DIR=/opt/static-llama/extracted \
      cargo build --release --no-default-features --features lambda,ltembed \
          --bin write_lambda \
          --bin index_builder_lambda \
          --bin query_lambda; \
    fi && \
    cp target/release/write_lambda /write_lambda && \
    cp target/release/index_builder_lambda /index_builder_lambda && \
    cp target/release/query_lambda /query_lambda
