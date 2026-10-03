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

use ltsearch::embedding::FixedEmbeddingGenerator;
use ltsearch::index::{
    sha256_hex, CentroidTable, EmbeddingProfile, ProjectionMatrix, ReleaseSource, StaticChunk,
    StaticIndexBuilder, StaticReleaseBuilder,
};
use ltsearch::models::CorpusType;
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

    let manifest = StaticReleaseBuilder
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
