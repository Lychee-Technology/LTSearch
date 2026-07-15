FROM --platform=linux/arm64 public.ecr.aws/amazonlinux/amazonlinux:2023
RUN dnf install -y --allowerasing gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.0
ENV PATH="/root/.cargo/bin:${PATH}"
ARG LTEMBED_MODE=stub
# ort_bundle tarball for jina-embeddings-v5-text-nano-retrieval, with
# model.ort, tokenizer.json, build-info.json, libonnxruntime.so (linux/arm64)
# under a leading ./ (hence --strip-components=1 below). Defaults to the public
# minimal-ort-builder release asset; override to bump the pinned model version.
ARG LTEMBED_BUNDLE_URL=https://github.com/Lychee-Technology/minimal-ort-builder/releases/download/v1.0.9/jinaai__jina-embeddings-v5-text-nano-retrieval_q4f16_linux-arm64.tar.gz
RUN mkdir -p /ltembed-assets && \
    if [ "$LTEMBED_MODE" != "stub" ]; then \
      if [ -z "$LTEMBED_BUNDLE_URL" ]; then \
        echo "LTEMBED_MODE=real requires LTEMBED_BUNDLE_URL (ort_bundle tarball)" >&2; \
        exit 1; \
      fi; \
      curl -fSL "$LTEMBED_BUNDLE_URL" -o /tmp/ltembed-bundle.tar.gz && \
      tar -xzf /tmp/ltembed-bundle.tar.gz -C /ltembed-assets --strip-components=1 && \
      rm /tmp/ltembed-bundle.tar.gz && \
      test -f /ltembed-assets/model.ort && \
      test -f /ltembed-assets/tokenizer.json && \
      test -f /ltembed-assets/build-info.json && \
      test -f /ltembed-assets/libonnxruntime.so; \
    fi
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
          --bin query_lambda && \
      cargo build --release --no-default-features --features aws \
          --bin write_server \
          --bin index_builder_server \
          --bin query_server; \
    else \
      cargo build --release --no-default-features --features lambda,ltembed \
          --bin write_lambda \
          --bin index_builder_lambda \
          --bin query_lambda && \
      cargo build --release --no-default-features --features aws,ltembed \
          --bin write_server \
          --bin index_builder_server \
          --bin query_server; \
    fi && \
    cp target/release/write_lambda /write_lambda && \
    cp target/release/index_builder_lambda /index_builder_lambda && \
    cp target/release/query_lambda /query_lambda && \
    cp target/release/query_server /query_server && \
    cp target/release/write_server /write_server && \
    cp target/release/index_builder_server /index_builder_server
