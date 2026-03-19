#!/usr/bin/env bash
set -euo pipefail

cargo test --test write_build_publish_test -- --nocapture
