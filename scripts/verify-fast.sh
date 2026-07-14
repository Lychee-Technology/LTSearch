#!/usr/bin/env bash
set -euo pipefail

cargo build --bin query_lambda --bin write_lambda --bin index_builder_lambda
cargo test --lib --bins

test_targets=(
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
  vector_searcher_test
  wal_test
  write_api_test
  write_lambda_test
)

for target in "${test_targets[@]}"; do
  cargo test --test "$target"
done

cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
