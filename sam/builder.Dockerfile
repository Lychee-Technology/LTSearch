FROM --platform=linux/arm64 public.ecr.aws/amazonlinux/amazonlinux:2023
RUN dnf install -y --allowerasing gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.0
ENV PATH="/root/.cargo/bin:${PATH}"
ARG LTEMBED_MODE=stub
# Tarball with an ort_bundle for jina-embeddings-v5-text-nano at its root:
# model.ort, tokenizer.json, build-info.json, libonnxruntime.so (linux/arm64).
# Produced by the LTEmbed bundle builder; required when LTEMBED_MODE=real.
ARG LTEMBED_BUNDLE_URL=
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
      test -f /ltembed-assets/build-info.json; \
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
      cargo build --release --no-default-features \
          --bin write_lambda \
          --bin index_builder_lambda \
          --bin query_lambda; \
    else \
      cargo build --release --features ltembed \
          --bin write_lambda \
          --bin index_builder_lambda \
          --bin query_lambda; \
    fi && \
    cp target/release/write_lambda /write_lambda && \
    cp target/release/index_builder_lambda /index_builder_lambda && \
    cp target/release/query_lambda /query_lambda
