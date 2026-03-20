FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS builder
RUN dnf install -y gcc gcc-c++ make perl pkgconfig openssl-devel git tar gzip && dnf clean all
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.94.0
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /src
COPY . .
RUN printf '\n[patch."https://github.com/Lychee-Technology/LTEmbed"]\nltembed = { path = "/src/vendor/ltembed-stub" }\n' >> /src/.cargo/config.toml
RUN cargo build --release --no-default-features --bin index_builder_lambda

FROM public.ecr.aws/lambda/provided:al2023-arm64
COPY --from=builder /src/target/release/index_builder_lambda /var/runtime/bootstrap
