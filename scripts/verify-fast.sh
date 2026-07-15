#!/usr/bin/env bash
set -euo pipefail

# Lambda binaries only build under the lambda profile.
cargo build --no-default-features --features lambda --bin query_lambda --bin write_lambda --bin index_builder_lambda

# Library + binary unit tests on the AWS-free local profile.
cargo test --no-default-features --features local --lib

# Pure-local integration targets (no AWS clients) run under the local profile.
local_test_targets=(
  build_worker_test
  http_build_test
  http_common_test
  http_query_test
  http_write_test
  index_builder_lambda_test
  index_builder_test
  keyword_searcher_test
  lancedb_compile_smoke_test
  manifest_store_test
  models_test
  publisher_test
  query_flow_test
  query_lambda_test
  query_service_test
  ranker_test
  router_test
  runtime_local_test
  vector_searcher_test
  wal_test
  write_api_test
  write_lambda_test
)

for target in "${local_test_targets[@]}"; do
  cargo test --no-default-features --features local --test "$target"
done

# AWS-profile integration targets that construct AWS clients (no network I/O; the
# Moto-backed targets live in verify-moto.sh) run under the aws profile.
aws_test_targets=(
  runtime_aws_test
)

for target in "${aws_test_targets[@]}"; do
  cargo test --no-default-features --features aws --test "$target"
done

cargo fmt --check

# --all-features never compiles the #[cfg(not(feature="aws"))] branches, so lint
# each profile explicitly to cover both the local-only and AWS/lambda code paths.
cargo clippy --no-default-features --features local --all-targets -- -D warnings
cargo clippy --no-default-features --features aws,lambda,ltembed --all-targets -- -D warnings
