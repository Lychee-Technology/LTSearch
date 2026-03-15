# _head Manifest Path Doc Alignment Design

## Goal

Align documentation with the implemented `_head.manifest_path` contract so docs use the canonical relative object key rather than a full `s3://...` URI.

## Current Contract

- `src/storage/manifest_store.rs` validates `_head.manifest_path` against `version_manifest_key(version_id)`.
- The canonical value is `index/versions/<version_id>/manifest.json`.
- This contract applies only to `_head.manifest_path`.
- Other artifact location fields such as shard `lance_path` and `tantivy_path` remain full S3 URIs.

## Scope

Update only documentation that incorrectly shows `_head.manifest_path` as a full S3 URI.

Target files:
- `docs/design.md`
- `docs/superpowers/plans/2026-03-14-doc-alignment.md`
- `docs/arch.md` only if it contains a conflicting `_head` field-level example or wording

## Changes

1. Replace `_head` JSON examples from:
   - `"manifest_path": "s3://bucket/index/versions/42/manifest.json"`
   to:
   - `"manifest_path": "index/versions/42/manifest.json"`
2. Add one short normative sentence in `docs/design.md` that `_head.manifest_path` stores the canonical bucket-relative object key, not a full S3 URI.
3. Leave `lance_path` / `tantivy_path` examples unchanged.

## Non-Goals

- No change to runtime code behavior
- No change to shard artifact path fields
- No broader storage-layout rewrite outside the `_head.manifest_path` contract

## Verification

- Grep docs for `_head.manifest_path` examples using full `s3://` URIs
- Re-read updated sections and confirm they match `src/storage/manifest_store.rs`
