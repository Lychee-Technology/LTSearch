#!/usr/bin/env bash
# Embeds a turbo_bench real-embedding fixture with LTEmbed (#177): reads the
# fixture's docs.jsonl and queries.jsonl, writes docs.f32 and queries.f32, and
# then manifest.json, which holds the vectors' counts and digest (checked by
# the harness) and the pins that produced them.
#
# LTEmbed links llama.cpp built for Graviton2, so the embedder is built and run
# in a linux/arm64 container: natively on an arm64 host, or under qemu-user
# emulation (a registered binfmt handler) on x86_64, which is much slower.
# Every pin comes from where the Lambda build reads it: the base image, dnf
# release and Rust toolchain from sam/builder.Dockerfile, the GGUF bundle and
# static llama.cpp through scripts/ltembed-pins.sh, and the LTEmbed rev from
# Cargo.lock, which embed/Cargo.toml must match.
#
# usage: examples/turbo_bench/fixture_gen/generate.sh <fixture_dir> [workers]
#   fixture_dir must be inside the repository; workers defaults to nproc.
#   CONTAINER_ENGINE=podman uses podman instead of docker.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
GEN_DIR="$REPO_ROOT/examples/turbo_bench/fixture_gen"
IMAGE_TAG="${TURBO_BENCH_EMBED_IMAGE:-ltsearch-turbo-bench-embed:dev}"
ENGINE="${CONTAINER_ENGINE:-docker}"

fixture="$(cd "${1:?usage: generate.sh <fixture_dir> [workers]}" && pwd)"
workers="${2:-$(nproc)}"
case "$fixture" in
  "$REPO_ROOT"/*) ;;
  *) echo "generate.sh: $fixture is outside $REPO_ROOT" >&2; exit 1 ;;
esac

# shellcheck source=scripts/ltembed-pins.sh
source "$REPO_ROOT/scripts/ltembed-pins.sh"

locked_rev="$(sed -n 's|.*git+https://github.com/Lychee-Technology/LTEmbed?[^#]*#\([0-9a-f]\{40\}\)".*|\1|p' "$REPO_ROOT/Cargo.lock" | head -n 1)"
embed_rev="$(sed -n 's|^ltembed = .*rev = "\([0-9a-f]\{40\}\)".*|\1|p' "$GEN_DIR/embed/Cargo.toml")"
if [[ -z "$locked_rev" || "$locked_rev" != "$embed_rev" ]]; then
  echo "generate.sh: embed/Cargo.toml pins LTEmbed '$embed_rev', Cargo.lock '$locked_rev'" >&2
  exit 1
fi

builder="$REPO_ROOT/sam/builder.Dockerfile"
base_image="$(sed -n 's/^FROM \(.*amazonlinux:2023@sha256:[0-9a-f]*\) AS builder$/\1/p' "$builder")"
releasever="$(sed -n 's|^RUN echo "\([0-9.]*\)" > /etc/dnf/vars/releasever$|\1|p' "$builder" | head -n 1)"
toolchain="$(sed -n 's/.*--default-toolchain \([0-9.]*\).*/\1/p' "$builder" | head -n 1)"
for pin in base_image releasever toolchain; do
  if [[ -z "${!pin}" ]]; then
    echo "generate.sh: no $pin found in $builder" >&2
    exit 1
  fi
done

gguf_url="$(ltembed_pin LTEMBED_GGUF_URL)"
gguf_sha256="$(ltembed_pin LTEMBED_GGUF_SHA256)"
tokenizer_url="$(ltembed_pin LTEMBED_TOKENIZER_URL)"
tokenizer_sha256="$(ltembed_pin LTEMBED_TOKENIZER_SHA256)"
static_llama_url="$(ltembed_pin STATIC_LLAMA_URL)"
static_llama_sha256="$(ltembed_pin STATIC_LLAMA_SHA256)"

static_llama="$("$REPO_ROOT/scripts/fetch-static-llama.sh" "$REPO_ROOT/target/static-llama")"
bundle="$REPO_ROOT/target/ltembed-bundle"
mkdir -p "$bundle"
fetch() {
  local url="$1" sha256="$2" dest="$3"
  if ! echo "$sha256  $dest" | sha256sum -c --quiet - >/dev/null 2>&1; then
    echo "generate.sh: downloading $url" >&2
    curl -fsSL --retry 3 "$url" -o "$dest"
    echo "$sha256  $dest" | sha256sum -c --quiet -
  fi
}
fetch "$gguf_url" "$gguf_sha256" "$bundle/model.gguf"
fetch "$tokenizer_url" "$tokenizer_sha256" "$bundle/tokenizer.json"
cp "$REPO_ROOT/sam/ltembed-build-info.json" "$bundle/build-info.json"

"$ENGINE" build --platform linux/arm64 \
  --build-arg BASE_IMAGE="$base_image" \
  --build-arg RELEASEVER="$releasever" \
  --build-arg RUST_TOOLCHAIN="$toolchain" \
  -t "$IMAGE_TAG" "$GEN_DIR"

# Rootless podman already maps the container's root to the invoking user;
# under docker, run as that user so the files written stay theirs.
user=()
if ! "$ENGINE" --version | grep -qi podman; then
  user=(--user "$(id -u):$(id -g)")
fi
# RUSTUP_TOOLCHAIN overrides the repository's rust-toolchain.toml, which would
# have rustup install components the build doesn't need.
"$ENGINE" run --rm --platform linux/arm64 --security-opt label=disable ${user[@]+"${user[@]}"} \
  -v "$REPO_ROOT:/work" -w /work/examples/turbo_bench/fixture_gen/embed \
  -e HOME=/tmp \
  -e RUSTUP_TOOLCHAIN="$toolchain" \
  -e CARGO_HOME=/work/target/turbo-bench-embed/cargo-home \
  -e CARGO_TARGET_DIR=/work/target/turbo-bench-embed \
  -e STATIC_LLAMA_DIR="/work/${static_llama#"$REPO_ROOT"/}" \
  "$IMAGE_TAG" \
  sh -c 'cargo build --locked --release && exec /work/target/turbo-bench-embed/release/turbo-bench-embed "$@"' \
  sh /work/target/ltembed-bundle "/work/${fixture#"$REPO_ROOT"/}" "$workers"

FIXTURE="$fixture" \
MODEL_ID="$(sed -n 's/.*"target_id": *"\([^"]*\)".*/\1/p' "$REPO_ROOT/sam/ltembed-build-info.json")" \
LTEMBED_REV="$locked_rev" GGUF_URL="$gguf_url" GGUF_SHA256="$gguf_sha256" \
TOKENIZER_URL="$tokenizer_url" TOKENIZER_SHA256="$tokenizer_sha256" \
STATIC_LLAMA_URL="$static_llama_url" STATIC_LLAMA_SHA256="$static_llama_sha256" \
BASE_IMAGE="$base_image" RUST_TOOLCHAIN="$toolchain" HOST_ARCH="$(uname -m)" \
python3 -I - <<'PY'
import hashlib
import json
import os
import struct
from datetime import datetime, timezone
from pathlib import Path

DIM = 512
env = os.environ
fixture = Path(env["FIXTURE"])
docs = (fixture / "docs.f32").read_bytes()
queries = (fixture / "queries.f32").read_bytes()
# The harness's digest: the counts as u64 LE, then every value as f32 LE.
digest = hashlib.sha256(struct.pack("<QQ", len(docs) // (4 * DIM), len(queries) // (4 * DIM)))
digest.update(docs)
digest.update(queries)
manifest = {
    "docs": len(docs) // (4 * DIM),
    "queries": len(queries) // (4 * DIM),
    "dim": DIM,
    "digest": digest.hexdigest(),
    "model_id": env["MODEL_ID"],
    "generated_with": {
        "ltembed_rev": env["LTEMBED_REV"],
        "engine": "EngineConfig::default() (Matryoshka truncation to 512 dims, then L2 "
        "normalization), one llama.cpp thread per engine; docs.jsonl embedded as "
        "Document, queries.jsonl as Query",
        "gguf": {"url": env["GGUF_URL"], "sha256": env["GGUF_SHA256"]},
        "tokenizer": {"url": env["TOKENIZER_URL"], "sha256": env["TOKENIZER_SHA256"]},
        "static_llama_cpp": {"url": env["STATIC_LLAMA_URL"], "sha256": env["STATIC_LLAMA_SHA256"]},
        "base_image": env["BASE_IMAGE"],
        "rust_toolchain": env["RUST_TOOLCHAIN"],
        "platform": f"linux/arm64 container on an {env['HOST_ARCH']} host",
        "date": datetime.now(timezone.utc).date().isoformat(),
    },
}
(fixture / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(f"generate.sh: wrote {fixture / 'manifest.json'}, digest {manifest['digest']}")
PY
