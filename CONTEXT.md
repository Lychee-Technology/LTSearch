# LTSearch Context

LTSearch is a Rust hybrid retrieval engine. It serves dynamic user documents through LanceDB vector
search and Tantivy keyword search, and serves authoritative static corpora through TurboQuant mmap
search. Dynamic results are fused with reciprocal-rank fusion; static results remain separate.

The system has query, write, and index-builder deployables. Writes create document events, the
builder materializes and publishes immutable index versions, and query resolves the active version.

Current architecture work makes AWS optional: local deployment uses SQLite for durable events, build
jobs, and active-release coordination, while AWS remains an adapter implementation. Static corpora
are built from immutable Lance releases into versioned TurboQuant v3 releases and are activated
separately from dynamic indexes.
