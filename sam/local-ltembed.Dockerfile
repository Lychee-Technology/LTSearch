# real-LTEmbed 单镜像本地运行时（#141）：与 sam/local.Dockerfile 同构，但以
# `--features local,ltembed` 编译真实推理引擎，并把锁定校验的 ort_bundle 烘焙进
# 镜像 /opt/ltembed（模型经 LTSEARCH_{SIDE}_LTEMBED_BUNDLE_DIR 预置路径供给，
# 无需 S3/AWS env——见 src/embedding/model_assets.rs 的"镜像预置"分支）。
#
# 仅供 real-model E2E（docker-compose.local-ltembed.yml）使用，不是发布物；
# 发布镜像仍是 sam/local.Dockerfile（fixed embedding，release.yml 原样构建）。
#
# pin 单一来源：bundle URL/SHA256 的权威默认值只在 sam/builder.Dockerfile 的
# ARG；本文件 ARG 故意留空，由 scripts/e2e/build-local-ltembed-image.sh 提取后
# 经 --build-arg 注入，空值直接构建失败——不允许第二处硬编码 pin。
#
# real 编译需要 LTEmbed 源 checkout（Cargo.lock rev；上游 HEAD 已切 llama.cpp
# 不兼容），构建脚本先用 prepare_local_ltembed_checkout 物化 .sam-local-deps/LTEmbed。
#
# base 镜像 digest pin 与 releasever 锁对齐 sam/{local,builder}.Dockerfile，
# bump 时三处一起更新；cache mount id 一致以共享本机缓存。
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS bundle
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
ARG LTEMBED_BUNDLE_URL=
ARG LTEMBED_BUNDLE_SHA256=
RUN if [ -z "$LTEMBED_BUNDLE_URL" ] || [ -z "$LTEMBED_BUNDLE_SHA256" ]; then \
      echo "LTEMBED_BUNDLE_URL / LTEMBED_BUNDLE_SHA256 must be injected from sam/builder.Dockerfile pins (use scripts/e2e/build-local-ltembed-image.sh)" >&2; \
      exit 1; \
    fi && \
    mkdir -p /ltembed-assets && \
    dnf install -y tar gzip >/dev/null && dnf clean all >/dev/null && \
    curl -fSL "$LTEMBED_BUNDLE_URL" -o /tmp/ltembed-bundle.tar.gz && \
    echo "$LTEMBED_BUNDLE_SHA256  /tmp/ltembed-bundle.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/ltembed-bundle.tar.gz -C /ltembed-assets --strip-components=1 && \
    rm /tmp/ltembed-bundle.tar.gz && \
    test -f /ltembed-assets/model.ort && \
    test -f /ltembed-assets/tokenizer.json && \
    test -f /ltembed-assets/build-info.json && \
    test -f /ltembed-assets/libonnxruntime.so

FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS builder
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
RUN dnf install -y --allowerasing gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.0
ENV PATH="/root/.cargo/bin:${PATH}"
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
    cargo build --release --no-default-features --features local,ltembed --bin ltsearch && \
    cp target/release/ltsearch /ltsearch

FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3
COPY --from=builder /ltsearch /app/ltsearch
COPY --from=bundle /ltembed-assets /opt/ltembed
ENV LTSEARCH_HTTP_PORT=8080
EXPOSE 8080
ENTRYPOINT ["/app/ltsearch"]
