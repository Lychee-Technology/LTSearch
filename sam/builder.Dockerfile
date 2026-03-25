FROM --platform=linux/arm64 public.ecr.aws/amazonlinux/amazonlinux:2023
RUN dnf install -y gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip curl && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.0
ENV PATH="/root/.cargo/bin:${PATH}"
ARG LTEMBED_MODE=stub
ARG HF_MODEL=intfloat/multilingual-e5-small
RUN mkdir -p /ltembed-assets && \
    if [ "$LTEMBED_MODE" != "stub" ]; then \
      curl -fSL "https://huggingface.co/${HF_MODEL}/resolve/main/model.safetensors" \
           -o /ltembed-assets/model.safetensors && \
      curl -fSL "https://huggingface.co/${HF_MODEL}/resolve/main/config.json" \
           -o /ltembed-assets/config.json && \
      curl -fSL "https://huggingface.co/${HF_MODEL}/resolve/main/tokenizer.json" \
           -o /ltembed-assets/tokenizer.json; \
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
