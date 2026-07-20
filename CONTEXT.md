# LTSearch Context

LTSearch is a Rust hybrid retrieval engine. It serves dynamic user documents through LanceDB vector
search and Tantivy keyword search, and serves authoritative static corpora through TurboQuant mmap
search. Dynamic results are fused with reciprocal-rank fusion; static results remain separate.

The system has query, write, and index-builder deployables. Writes create document events, the
builder materializes and publishes immutable index versions, and query resolves the active version.

AWS is optional: local deployment uses SQLite for durable events, build jobs, and active-release
coordination, while AWS remains an adapter implementation. Static corpora are built from immutable
Lance releases into versioned TurboQuant v3 releases and are activated separately from dynamic
indexes.

Release artifacts (#113) are exactly one local OCI image (`ghcr.io/lychee-technology/ltsearch-local`,
the unified `ltsearch` binary with write/build/query/static-build/static-activate subcommands) plus
three Lambda function ZIPs and a `model-assets.zip`, published with checksums and provenance by the
tag-triggered release workflow. The per-component HTTP server binaries and image-based Lambda
deployment were removed in #113 (the `server` feature remains — it is the shared axum layer).

## Build profiles

AWS is an optional cargo feature, not a compile-time requirement (ADR-0001). Four runtime features:
`local` (the default — `default = ["local"]`, AWS-free), `server` (shared axum HTTP layer, pulled by
`local` and `aws`), `aws` (adds the AWS SDK adapters), and `lambda` (adds the Lambda runtime and the
handler binaries); `ltembed` is orthogonal. A bare `cargo build` is AWS-free; AWS/Lambda binaries
must name their profile (`--features aws` / `--features lambda`). The domain core depends only on the
provider-neutral contracts in `src/contracts.rs`, each with a local and an AWS impl: document events →
`WalStorage`; build jobs → `BuildQueue` + `BuildJobSource`; artifact access → `PublishStorage` +
`ArtifactSync`; active-release coordination → `ManifestStore`.
