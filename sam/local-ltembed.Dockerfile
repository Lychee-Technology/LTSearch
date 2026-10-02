# real-LTEmbed 单镜像本地运行时（#141）：与 sam/local.Dockerfile 同构，但以
# `--features local,ltembed` 编译真实推理引擎（llama.cpp/GGUF，LTEmbed#149），并把
# 锁定校验的 GGUF bundle 烘焙进镜像 /opt/ltembed（模型经
# LTSEARCH_{SIDE}_LTEMBED_BUNDLE_DIR 预置路径供给，无需 S3/AWS env——见
# src/embedding/model_assets.rs 的"镜像预置"分支）。
#
# 仅供 real-model E2E（docker-compose.local-ltembed.yml）使用，不是发布物；
# 发布镜像仍是 sam/local.Dockerfile（fixed embedding，release.yml 原样构建）。
#
# pin 单一来源：GGUF/tokenizer 与 static llama 的 URL/SHA256 权威默认值只在
# sam/builder.Dockerfile 的 ARG；本文件 ARG 故意留空，由
# scripts/e2e/build-local-ltembed-image.sh 提取后经 --build-arg 注入，空值直接构建
# 失败——不允许第二处硬编码 pin。build-info.json 与 builder.Dockerfile 共用
# sam/ltembed-build-info.json。
#
# real 编译需要 LTEmbed 源 checkout（Cargo.lock rev；不取上游 HEAD），构建脚本先用
# prepare_locked_ltembed_checkout 物化 .sam-local-deps/LTEmbed。
#
# base 镜像 digest pin 与 releasever 锁对齐 sam/{local,builder}.Dockerfile，
# bump 时三处一起更新；cache mount id 一致以共享本机缓存。
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS bundle
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
ARG LTEMBED_GGUF_URL=
ARG LTEMBED_GGUF_SHA256=
ARG LTEMBED_TOKENIZER_URL=
ARG LTEMBED_TOKENIZER_SHA256=
COPY sam/ltembed-build-info.json /tmp/ltembed-build-info.json
RUN if [ -z "$LTEMBED_GGUF_URL" ] || [ -z "$LTEMBED_GGUF_SHA256" ] || \
       [ -z "$LTEMBED_TOKENIZER_URL" ] || [ -z "$LTEMBED_TOKENIZER_SHA256" ]; then \
      echo "LTEMBED_{GGUF,TOKENIZER}_{URL,SHA256} must be injected from sam/builder.Dockerfile pins (use scripts/e2e/build-local-ltembed-image.sh)" >&2; \
      exit 1; \
    fi && \
    mkdir -p /ltembed-assets && \
    curl -fSL --retry 3 "$LTEMBED_GGUF_URL" -o /ltembed-assets/model.gguf && \
    echo "$LTEMBED_GGUF_SHA256  /ltembed-assets/model.gguf" | sha256sum -c - && \
    curl -fSL --retry 3 "$LTEMBED_TOKENIZER_URL" -o /ltembed-assets/tokenizer.json && \
    echo "$LTEMBED_TOKENIZER_SHA256  /ltembed-assets/tokenizer.json" | sha256sum -c - && \
    cp /tmp/ltembed-build-info.json /ltembed-assets/build-info.json

FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS builder
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
RUN dnf install -y --allowerasing gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.1
ENV PATH="/root/.cargo/bin:${PATH}"
ARG STATIC_LLAMA_URL=
ARG STATIC_LLAMA_SHA256=
COPY scripts/fetch-static-llama.sh /opt/ltsearch-build/fetch-static-llama.sh
RUN if [ -z "$STATIC_LLAMA_URL" ] || [ -z "$STATIC_LLAMA_SHA256" ]; then \
      echo "STATIC_LLAMA_URL / STATIC_LLAMA_SHA256 must be injected from sam/builder.Dockerfile pins (use scripts/e2e/build-local-ltembed-image.sh)" >&2; \
      exit 1; \
    fi && \
    /opt/ltsearch-build/fetch-static-llama.sh /opt/static-llama >/dev/null
ENV STATIC_LLAMA_DIR=/opt/static-llama/extracted
WORKDIR /src
COPY . .
RUN test -f /src/.sam-local-deps/LTEmbed/Cargo.toml || { \
      echo "missing .sam-local-deps/LTEmbed checkout (run scripts/e2e/build-local-ltembed-image.sh)" >&2; \
      exit 1; \
    }
RUN printf '\n[patch."https://github.com/Lychee-Technology/LTEmbed"]\nltembed = { path = "/src/.sam-local-deps/LTEmbed" }\n' >> /src/.cargo/config.toml
RUN --mount=type=cache,id=ltsearch-cargo-registry,target=/root/.cargo/registry \
    --mount=type=cache,id=ltsearch-cargo-git,target=/root/.cargo/git \
    --mount=type=cache,id=ltsearch-cargo-target,target=/src/target \
    cargo build --release --no-default-features --features local,ltembed --bin ltsearch --example emit_static_lance_fixture && \
    cp target/release/ltsearch /ltsearch && \
    cp target/release/examples/emit_static_lance_fixture /emit_static_lance_fixture

FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3
COPY --from=builder /ltsearch /app/ltsearch
# #143: fixture 生成器随镜像分发，静态契约 runner 在容器内以真实 bundle 产 Lance fixture。
COPY --from=builder /emit_static_lance_fixture /app/emit_static_lance_fixture
COPY --from=bundle /ltembed-assets /opt/ltembed
ENV LTSEARCH_HTTP_PORT=8080
EXPOSE 8080
ENTRYPOINT ["/app/ltsearch"]
