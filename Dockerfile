# Build stage
FROM rust:1.94 AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY vendor/ vendor/
COPY src/ src/

RUN cargo build --release --no-default-features --features lambda --bin query_lambda

# Runtime stage
FROM public.ecr.aws/lambda/provided:al2023

COPY --from=builder /build/target/release/query_lambda /var/task/bootstrap

# Static TurboQuant index — update this layer monthly/quarterly by rebuilding
# after running turbo_index_builder.
COPY static/ /app/static/

ENV LTSEARCH_QUERY_STATIC_DIR=/app

CMD ["bootstrap"]
