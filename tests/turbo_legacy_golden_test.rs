//! Golden pins for the legacy (v2/v3) TurboQuant codec.
//!
//! Every digest below was captured from `main` at 654a955, before #161 routed
//! the legacy codec constants through `TurboQuantConfig::legacy_v1()`.
//!
//! These are the post-#156 bytes. The pre-#156 generator (rand 0.8.5 +
//! rand_chacha 0.3.1) produced `centroids.bin` sha256 `a8c2e500…8d63` and
//! `projection.bin` sha256 `5bac235d…cb16`: the ChaCha8 stream is unchanged,
//! but rand 0.9 changed how `random_range(-1.0..=1.0)` maps it to f32
//! (rand#1289), moving about 81% of the values by up to 2.4e-7. Releases built
//! before #156 still load and score with their own stored assets, but
//! rebuilding the same input now yields different assets and a different
//! release ID.
//!
//! A failure here means v2/v3 artifacts and release IDs built from the same
//! input changed again. Do not update a digest without deciding that this is
//! acceptable.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use ltsearch::embedding::FixedEmbeddingGenerator;
use ltsearch::index::{
    sha256_hex, CentroidTable, EmbeddingProfile, MmapIndex, ProjectionMatrix, ReleaseSource,
    StaticChunk, StaticIndexBuilder, StaticReleaseBuilder, StaticReleaseFormat,
};
use ltsearch::models::{CorpusType, IndexManifest};
use ltsearch::query::{StaticRetriever, TurboQuantSearcher};
use ltsearch::storage::{ActiveManifest, ManifestHead};
use serde_json::{json, Value};
use tempfile::TempDir;

const LEGACY_CENTROIDS_SHA256: &str =
    "00f7523e6d75d0db3d922b926b98c03a4f6684db313b03b23c091cc36540f281";
const LEGACY_PROJECTION_SHA256: &str =
    "8b1aad044a0b1a56167be6373389c43c806261835fe133edb7d8be133d4b7dcb";

const V3_RELEASE_ID: &str = "ec494d0a941de1716d63b26f670f7bf7ba1c3321de6c7603449c9603fa927b4e";
const V3_RELEASE_FILES: &[(&str, &str)] = &[
    ("centroids.bin", LEGACY_CENTROIDS_SHA256),
    ("projection.bin", LEGACY_PROJECTION_SHA256),
    (
        "release_manifest.json",
        "c63751d387c549c42deb3dbd670c99d130bf23d89894a568094412efe5c880c7",
    ),
    (
        "turbo_static.bin",
        "3819456ccbc2b2ed49d81fbf2a3299f4824de92c4d8a61a07fcf3b3b58d2431b",
    ),
    (
        "turbo_static_docid.bin",
        "a00cc02b2c0bfa022b77ec432faf39abf70e6bb56e58d8d23b91b9ad4003e801",
    ),
    (
        "turbo_static_meta.bin",
        "03105b52ee680acd80fa3463d4990b197eaf494caef9e93c1c2f7ceb8e4b20bc",
    ),
    (
        "turbo_static_meta_ext.bin",
        "fa2074aba43389dc65fc7a2a432812a98a80c7b08742c5aaab3c699de1c28720",
    ),
    (
        "turbo_static_meta_json.bin",
        "9d1e060b79ea63fb4f4b0790ff0d35e0558835e98b0f1ffea33611a108f0b444",
    ),
    (
        "turbo_static_text.bin",
        "0455ae5bb3fa5c4f5100204a6bdff845f0aea5615deea1d5fa79e1013bbc0391",
    ),
    (
        "turbo_static_title.bin",
        "188426099f926533cd33247706c09264dfcd78812de19c6205d14f37ae289053",
    ),
];
const V2_INDEX_FILES: &[(&str, &str)] = &[
    ("centroids.bin", LEGACY_CENTROIDS_SHA256),
    ("projection.bin", LEGACY_PROJECTION_SHA256),
    (
        "turbo_static.bin",
        "f0d1422eb1189cfdf0f0938a5468f46fe3f228b26dfe0d9cb6e21d1f554d8de8",
    ),
    (
        "turbo_static_meta.bin",
        "03105b52ee680acd80fa3463d4990b197eaf494caef9e93c1c2f7ceb8e4b20bc",
    ),
    (
        "turbo_static_text.bin",
        "0455ae5bb3fa5c4f5100204a6bdff845f0aea5615deea1d5fa79e1013bbc0391",
    ),
    (
        "turbo_static_title.bin",
        "188426099f926533cd33247706c09264dfcd78812de19c6205d14f37ae289053",
    ),
];

/// Search results over the fixture, as `(doc_id, score bits)` in rank order
/// for each of the three [`queries`], with `top_k` covering all six records.
/// Captured from `main` at 16789d5, before #166 touched the loader and the
/// searcher. v2 results carry the hashed doc_id, v3 the original one; the
/// rankings and scores are the same because both versions encode identically.
const V3_SEARCH_RESULTS: [[(&str, u32); 6]; 3] = [
    [
        ("doc-03", 0x44fcd3b2),
        ("doc-04", 0xc378a641),
        ("doc-02", 0xc3848bf9),
        ("doc-01", 0xc4ca3bfa),
        ("doc-05", 0xc4de006f),
        ("doc-00", 0xc4de49ec),
    ],
    [
        ("doc-03", 0x43c7d880),
        ("doc-04", 0x43a856ca),
        ("doc-02", 0xc4ca6ab7),
        ("doc-01", 0xc4e70f73),
        ("doc-00", 0xc4e7f8e7),
        ("doc-05", 0xc538a7d3),
    ],
    [
        ("doc-04", 0x4557d573),
        ("doc-03", 0x450a8af8),
        ("doc-05", 0x44d8cdfe),
        ("doc-01", 0xc30c7b56),
        ("doc-02", 0xc386b82a),
        ("doc-00", 0xc435ce38),
    ],
];
const V2_SEARCH_RESULTS: [[(&str, u32); 6]; 3] = [
    [
        ("10108727749282401313", 0x44fcd3b2),
        ("10108728848794029524", 0xc378a641),
        ("10108726649770773102", 0xc3848bf9),
        ("10108725550259144891", 0xc4ca3bfa),
        ("10108729948305657735", 0xc4de006f),
        ("10108724450747516680", 0xc4de49ec),
    ],
    [
        ("10108727749282401313", 0x43c7d880),
        ("10108728848794029524", 0x43a856ca),
        ("10108726649770773102", 0xc4ca6ab7),
        ("10108725550259144891", 0xc4e70f73),
        ("10108724450747516680", 0xc4e7f8e7),
        ("10108729948305657735", 0xc538a7d3),
    ],
    [
        ("10108728848794029524", 0x4557d573),
        ("10108727749282401313", 0x450a8af8),
        ("10108729948305657735", 0x44d8cdfe),
        ("10108725550259144891", 0xc30c7b56),
        ("10108726649770773102", 0xc386b82a),
        ("10108724450747516680", 0xc435ce38),
    ],
];

#[test]
fn legacy_centroid_table_bytes_are_pinned() {
    let bytes = CentroidTable::generate(512, 4, 7).to_bytes();
    assert_eq!(sha256_hex(&bytes), LEGACY_CENTROIDS_SHA256);
}

#[test]
fn legacy_projection_matrix_bytes_are_pinned() {
    let bytes = ProjectionMatrix::generate(512, 512, 11).to_bytes();
    assert_eq!(sha256_hex(&bytes), LEGACY_PROJECTION_SHA256);
}

#[test]
fn v3_release_bytes_and_release_id_are_pinned() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("release");
    let (chunks, embeddings) = fixture();

    let manifest = StaticReleaseBuilder::new(StaticReleaseFormat::V3)
        .build_release(
            &output,
            &chunks,
            &embeddings,
            &EmbeddingProfile {
                model_id: "jina-v5-nano/512".to_string(),
                dim: 512,
            },
            &ReleaseSource {
                kind: "lance".to_string(),
                dataset_path: "/data/golden.lance".to_string(),
                table_version: 3,
                table_row_count: chunks.len() as u64,
                corpus_type: CorpusType::Legal,
            },
        )
        .unwrap();

    assert_eq!(manifest.release_id, V3_RELEASE_ID);
    assert_eq!(file_digests(&output), pinned(V3_RELEASE_FILES));
}

#[test]
fn v2_static_index_bytes_are_pinned() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("static");
    let (chunks, embeddings) = fixture();
    let embeddings: Vec<Option<Vec<f32>>> = embeddings.into_iter().map(Some).collect();

    StaticIndexBuilder::new()
        .build(
            &output,
            &chunks,
            &embeddings,
            // Every chunk is pre-embedded, so the generator is never called.
            &FixedEmbeddingGenerator::new(vec![0.0; 512]),
        )
        .unwrap();

    assert_eq!(file_digests(&output), pinned(V2_INDEX_FILES));
}

#[test]
fn v3_search_ranking_and_scores_are_pinned() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("release");
    let (chunks, embeddings) = fixture();

    StaticReleaseBuilder::new(StaticReleaseFormat::V3)
        .build_release(
            &output,
            &chunks,
            &embeddings,
            &EmbeddingProfile {
                model_id: "jina-v5-nano/512".to_string(),
                dim: 512,
            },
            &ReleaseSource {
                kind: "lance".to_string(),
                dataset_path: "/data/golden.lance".to_string(),
                table_version: 3,
                table_row_count: chunks.len() as u64,
                corpus_type: CorpusType::Legal,
            },
        )
        .unwrap();

    assert_eq!(search_results(&output), pinned_results(&V3_SEARCH_RESULTS));
}

#[test]
fn v2_search_ranking_and_scores_are_pinned() {
    let dir = TempDir::new().unwrap();
    let output = dir.path().join("static");
    let (chunks, embeddings) = fixture();
    let embeddings: Vec<Option<Vec<f32>>> = embeddings.into_iter().map(Some).collect();

    StaticIndexBuilder::new()
        .build(
            &output,
            &chunks,
            &embeddings,
            &FixedEmbeddingGenerator::new(vec![0.0; 512]),
        )
        .unwrap();

    assert_eq!(search_results(&output), pinned_results(&V2_SEARCH_RESULTS));
}

/// The full ranking of the index at `dir` for each of [`queries`]: every
/// result's doc_id and the bits of its score.
fn search_results(dir: &Path) -> Vec<Vec<(String, u32)>> {
    let searcher = TurboQuantSearcher::new(Arc::new(MmapIndex::load(dir).unwrap()));
    queries()
        .iter()
        .map(|query| {
            searcher
                .search(&stub_manifest(), query, 6)
                .unwrap()
                .into_iter()
                .map(|result| (result.doc_id, result.score.to_bits()))
                .collect()
        })
        .collect()
}

fn pinned_results(results: &[[(&str, u32); 6]; 3]) -> Vec<Vec<(String, u32)>> {
    results
        .iter()
        .map(|ranking| {
            ranking
                .iter()
                .map(|(doc_id, score_bits)| (doc_id.to_string(), *score_bits))
                .collect()
        })
        .collect()
}

/// Three non-unit queries drawn like the fixture's embeddings, from other
/// SplitMix64 states.
fn queries() -> Vec<Vec<f32>> {
    (0..3u64)
        .map(|index| {
            let mut state = 0xC0DE_0000 + index;
            (0..512)
                .map(|_| (splitmix64(&mut state) >> 40) as f32 / (1u64 << 23) as f32 - 1.0)
                .collect()
        })
        .collect()
}

fn stub_manifest() -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id: 1,
            manifest_path: "m.json".into(),
            updated_at: 0,
        },
        manifest: IndexManifest {
            version_id: 1,
            created_at: 0,
            embedding_dim: 512,
            document_count: 0,
            num_shards: 0,
            shards: Vec::new(),
        },
    }
}

/// Six doc_id-sorted chunks: every corpus type, chunks with and without a
/// title, multi-key metadata, and non-unit embeddings spread over [-1, 1] so
/// every centroid index and both sign bits occur.
fn fixture() -> (Vec<StaticChunk>, Vec<Vec<f32>>) {
    let corpus_types = [
        CorpusType::Legal,
        CorpusType::Contract,
        CorpusType::Rfc,
        CorpusType::Other(9),
    ];
    let mut chunks = Vec::new();
    let mut embeddings = Vec::new();
    for index in 0..6u64 {
        let mut metadata: HashMap<String, Value> = HashMap::new();
        if index % 2 == 0 {
            metadata.insert("title".to_string(), json!(format!("标题 {index}")));
        }
        metadata.insert("section".to_string(), json!(index));
        metadata.insert("tags".to_string(), json!(["a", "b"]));
        chunks.push(StaticChunk {
            doc_id: format!("doc-{index:02}"),
            text: format!("第{index}条 body text"),
            metadata,
            corpus_type: corpus_types[index as usize % corpus_types.len()].clone(),
        });

        let mut state = 0x5EED_0000 + index;
        embeddings.push(
            (0..512)
                .map(|_| (splitmix64(&mut state) >> 40) as f32 / (1u64 << 23) as f32 - 1.0)
                .collect(),
        );
    }
    (chunks, embeddings)
}

/// SplitMix64, so the fixture is deterministic without a `rand` dependency.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `(file name, sha256)` of every file in `dir`, sorted by name.
fn file_digests(dir: &Path) -> Vec<(String, String)> {
    let mut digests: Vec<(String, String)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                sha256_hex(&fs::read(entry.path()).unwrap()),
            )
        })
        .collect();
    digests.sort();
    digests
}

fn pinned(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(name, sha)| (name.to_string(), sha.to_string()))
        .collect()
}
