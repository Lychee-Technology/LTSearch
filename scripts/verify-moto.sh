#!/usr/bin/env bash
set -euo pipefail

cargo test --no-default-features --features aws --test write_build_publish_test -- --nocapture
