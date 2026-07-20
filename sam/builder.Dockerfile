# ort_bundle 下载/校验独立成 bundle stage（#111）：Layer 打包只 build 该 stage
# （--target bundle），不触发 cargo 编译、不需要 LTEmbed 源 checkout。
#
# 可复现性（#113 review P1）：base 镜像按 digest pin（对应 AL2023 release
# 2023.12.20260710），dnf 以 /etc/dnf/vars/releasever 锁定同一 release 快照
# （AL2023 默认 releasever=latest 是可变仓库指针）。bump base 时两处一起更新。
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS bundle
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
ARG LTEMBED_MODE=stub
# ort_bundle tarball for jina-embeddings-v5-text-nano-retrieval, with
# model.ort, tokenizer.json, build-info.json, libonnxruntime.so (linux/arm64)
# under a leading ./ (hence --strip-components=1 below). Defaults to the public
# minimal-ort-builder release asset; bump URL and SHA256 together to pin a new
# model version.
ARG LTEMBED_BUNDLE_URL=https://github.com/Lychee-Technology/minimal-ort-builder/releases/download/v1.0.9/jinaai__jina-embeddings-v5-text-nano-retrieval_q4f16_linux-arm64.tar.gz
ARG LTEMBED_BUNDLE_SHA256=4d781723f14f8a9791fc31c21347364dea095dbbf22676d30fec3e659e6f6af9
RUN mkdir -p /ltembed-assets && \
    if [ "$LTEMBED_MODE" != "stub" ]; then \
      if [ -z "$LTEMBED_BUNDLE_URL" ] || [ -z "$LTEMBED_BUNDLE_SHA256" ]; then \
        echo "LTEMBED_MODE=real requires LTEMBED_BUNDLE_URL and LTEMBED_BUNDLE_SHA256 (ort_bundle tarball pin)" >&2; \
        exit 1; \
      fi; \
      dnf install -y tar gzip >/dev/null && dnf clean all >/dev/null && \
      curl -fSL "$LTEMBED_BUNDLE_URL" -o /tmp/ltembed-bundle.tar.gz && \
      echo "$LTEMBED_BUNDLE_SHA256  /tmp/ltembed-bundle.tar.gz" | sha256sum -c - && \
      tar -xzf /tmp/ltembed-bundle.tar.gz -C /ltembed-assets --strip-components=1 && \
      rm /tmp/ltembed-bundle.tar.gz && \
      test -f /ltembed-assets/model.ort && \
      test -f /ltembed-assets/tokenizer.json && \
      test -f /ltembed-assets/build-info.json && \
      test -f /ltembed-assets/libonnxruntime.so; \
    fi

FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:590b8c9fdab65c7f5b8a2392739104ed6bc5055433ba8ff2bf0d2fa500db2ea3 AS builder
RUN echo "2023.12.20260710" > /etc/dnf/vars/releasever
RUN dnf install -y --allowerasing gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.0
ENV PATH="/root/.cargo/bin:${PATH}"
ARG LTEMBED_MODE=stub
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
      cargo build --release --no-default-features --features lambda,ltembed \
          --bin write_lambda \
          --bin index_builder_lambda \
          --bin query_lambda; \
    fi && \
    cp target/release/write_lambda /write_lambda && \
    cp target/release/index_builder_lambda /index_builder_lambda && \
    cp target/release/query_lambda /query_lambda
