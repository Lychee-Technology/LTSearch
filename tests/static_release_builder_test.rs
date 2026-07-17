use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::index::{
    sha256_hex, EmbeddingProfile, MmapIndex, ReleaseSource, StaticChunk, StaticReleaseBuilder,
};
use ltsearch::models::{Citation, CorpusType};
use serde_json::{json, Value};

fn temp_dir(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-static-release-{name}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn finite_embedding(seed: f32) -> Vec<f32> {
    (0..512).map(|i| ((i as f32) * 0.001 + seed).sin()).collect()
}

fn citation_metadata(title: &str, resource_id: &str) -> HashMap<String, Value> {
    let mut metadata = HashMap::new();
    metadata.insert("title".to_string(), json!(title));
    metadata.insert("resource_id".to_string(), json!(resource_id));
    metadata.insert("source_type".to_string(), json!("statute"));
    metadata.insert("source_ref".to_string(), json!("第一条"));
    metadata.insert("url".to_string(), json!("https://example.com/law"));
    metadata.insert("section".to_string(), json!("总则"));
    metadata
}

fn sample_profile() -> EmbeddingProfile {
    EmbeddingProfile {
        model_id: "jina-embeddings-v2".to_string(),
        dim: 512,
    }
}

fn sample_source() -> ReleaseSource {
    ReleaseSource {
        kind: "lance".to_string(),
        dataset_path: "/data/corpus.lance".to_string(),
        table_version: 9,
        table_row_count: 2,
        corpus_type: CorpusType::Legal,
    }
}

#[test]
fn release_builder_writes_v3_artifacts_loadable_by_mmap_index() {
    let dir = temp_dir("v3-artifacts");
    let chunks = vec![
        StaticChunk {
            doc_id: "文档-1".to_string(),
            text: "第一条文本".to_string(),
            metadata: citation_metadata("宪法总纲", "res-1"),
            corpus_type: CorpusType::Legal,
        },
        StaticChunk {
            doc_id: "文档-2".to_string(),
            text: "第二条文本".to_string(),
            metadata: citation_metadata("合同法则", "res-2"),
            corpus_type: CorpusType::Contract,
        },
    ];
    let embeddings = vec![finite_embedding(0.1), finite_embedding(0.2)];

    let manifest = StaticReleaseBuilder
        .build_release(&dir, &chunks, &embeddings, &sample_profile(), &sample_source())
        .expect("build_release should succeed");

    assert_eq!(manifest.turbo_version, 3);
    assert!(!manifest.release_id.is_empty(), "release_id must be non-empty");

    let index = MmapIndex::load(&dir).expect("v3 image must load");
    assert_eq!(index.version(), 3);
    assert_eq!(index.record_count(), 2);

    // text / title / corpus_type match v2 semantics.
    assert_eq!(index.text(0), "第一条文本");
    assert_eq!(index.text(1), "第二条文本");
    assert_eq!(index.title(0), Some("宪法总纲"));
    assert_eq!(index.title(1), Some("合同法则"));

    // Original string doc_id round-trips per record.
    assert_eq!(index.original_doc_id(0), Some("文档-1"));
    assert_eq!(index.original_doc_id(1), Some("文档-2"));

    // metadata_json round-trips into a map that rebuilds a Citation.
    for (i, resource_id) in ["res-1", "res-2"].iter().enumerate() {
        let json = index.metadata_json(i).expect("v3 image has metadata_json");
        let map: HashMap<String, Value> = serde_json::from_str(json).expect("valid metadata JSON");
        let citation = Citation::from_metadata(&map).expect("citation rebuildable");
        assert_eq!(citation.resource_id, *resource_id);
        assert_eq!(citation.source_type, "statute");
        assert_eq!(citation.source_ref, "第一条");
        assert_eq!(citation.url.as_deref(), Some("https://example.com/law"));
    }

    // release_manifest.json exists and its output hashes match the files on disk.
    let manifest_path = dir.join("release_manifest.json");
    assert!(manifest_path.exists(), "release_manifest.json must exist");
    assert!(
        !manifest.outputs.iter().any(|o| o.name == "release_manifest.json"),
        "manifest must not list itself as an output"
    );
    assert_eq!(manifest.outputs.len(), 9, "nine .bin outputs expected");
    for output in &manifest.outputs {
        let bytes = fs::read(dir.join(&output.name)).expect("output file must exist on disk");
        assert_eq!(output.size_bytes, bytes.len() as u64, "{} size", output.name);
        assert_eq!(output.sha256, sha256_hex(&bytes), "{} sha256", output.name);
    }
    // outputs are sorted by name ascending.
    let mut sorted = manifest.outputs.clone();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(manifest.outputs, sorted, "outputs must be name-sorted");
}

#[test]
fn release_builder_rejects_non_512_dim() {
    let dir = temp_dir("non-512");
    let chunks = vec![StaticChunk {
        doc_id: "d1".to_string(),
        text: "t".to_string(),
        metadata: HashMap::new(),
        corpus_type: CorpusType::Legal,
    }];
    let embeddings = vec![vec![0.0f32; 511]];
    let profile = EmbeddingProfile {
        model_id: "m".to_string(),
        dim: 511,
    };
    let result = StaticReleaseBuilder.build_release(&dir, &chunks, &embeddings, &profile, &sample_source());
    assert!(result.is_err(), "511-dim embedding must be rejected");
}

#[test]
fn release_builder_rejects_non_finite_embedding() {
    let dir = temp_dir("non-finite");
    let chunks = vec![StaticChunk {
        doc_id: "d1".to_string(),
        text: "t".to_string(),
        metadata: HashMap::new(),
        corpus_type: CorpusType::Legal,
    }];
    let mut embedding = finite_embedding(0.1);
    embedding[7] = f32::NAN;
    let embeddings = vec![embedding];
    let result = StaticReleaseBuilder.build_release(&dir, &chunks, &embeddings, &sample_profile(), &sample_source());
    assert!(result.is_err(), "non-finite embedding must be rejected");
}

#[test]
fn release_builder_rejects_duplicate_doc_id() {
    let dir = temp_dir("dup-doc-id");
    let chunks = vec![
        StaticChunk {
            doc_id: "same".to_string(),
            text: "a".to_string(),
            metadata: HashMap::new(),
            corpus_type: CorpusType::Legal,
        },
        StaticChunk {
            doc_id: "same".to_string(),
            text: "b".to_string(),
            metadata: HashMap::new(),
            corpus_type: CorpusType::Legal,
        },
    ];
    let embeddings = vec![finite_embedding(0.1), finite_embedding(0.2)];
    let result = StaticReleaseBuilder.build_release(&dir, &chunks, &embeddings, &sample_profile(), &sample_source());
    assert!(result.is_err(), "duplicate doc_id must be rejected");
}
