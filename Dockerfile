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

CMD ["bootstrap"]
