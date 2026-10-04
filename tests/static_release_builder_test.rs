use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::index::{
    sha256_hex, EmbeddingProfile, MmapIndex, ReleaseSource, StaticChunk, StaticReleaseBuilder,
    StaticReleaseFormat, TurboQuantConfig, V3_RELEASE_OUTPUT_FILES,
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
    (0..512)
        .map(|i| ((i as f32) * 0.001 + seed).sin())
        .collect()
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

/// Both release formats: input validation is shared, so every rejection
/// below must hold for each.
fn formats() -> [StaticReleaseFormat; 2] {
    [
        StaticReleaseFormat::V3,
        StaticReleaseFormat::V4(TurboQuantConfig::prod_v1()),
    ]
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

    let manifest = StaticReleaseBuilder::new(StaticReleaseFormat::V3)
        .build_release(
            &dir,
            &chunks,
            &embeddings,
            &sample_profile(),
            &sample_source(),
        )
        .expect("build_release should succeed");

    assert_eq!(manifest.turbo_version, 3);
    assert!(
        !manifest.release_id.is_empty(),
        "release_id must be non-empty"
    );

    let index = MmapIndex::load(&dir).expect("v3 image must load");
    assert_eq!(index.version(), 3);
    assert_eq!(index.record_count(), 2);

    // The searcher ranks on the record's doc_id without reading the meta file,
    // so both must carry the same hashed id.
    for i in 0..2 {
        assert_eq!(index.record(i).doc_id(), index.meta(i).doc_id);
    }

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
        !manifest
            .outputs
            .iter()
            .any(|o| o.name == "release_manifest.json"),
        "manifest must not list itself as an output"
    );
    assert_eq!(manifest.outputs.len(), 9, "nine .bin outputs expected");
    for output in &manifest.outputs {
        let bytes = fs::read(dir.join(&output.name)).expect("output file must exist on disk");
        assert_eq!(
            output.size_bytes,
            bytes.len() as u64,
            "{} size",
            output.name
        );
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
    for format in formats() {
        let result = StaticReleaseBuilder::new(format).build_release(
            &dir,
            &chunks,
            &embeddings,
            &profile,
            &sample_source(),
        );
        assert!(
            result.is_err(),
            "{format:?}: 511-dim embedding must be rejected"
        );
    }
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
    for format in formats() {
        let result = StaticReleaseBuilder::new(format).build_release(
            &dir,
            &chunks,
            &embeddings,
            &sample_profile(),
            &sample_source(),
        );
        assert!(
            result.is_err(),
            "{format:?}: non-finite embedding must be rejected"
        );
    }
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
    for format in formats() {
        let result = StaticReleaseBuilder::new(format).build_release(
            &dir,
            &chunks,
            &embeddings,
            &sample_profile(),
            &sample_source(),
        );
        assert!(
            result.is_err(),
            "{format:?}: duplicate doc_id must be rejected"
        );
    }
}

#[test]
fn default_release_builder_writes_the_v3_format() {
    // #169 owns the switch to v4; until then a build that doesn't choose a
    // format must produce exactly what it did before v4 existed.
    assert_eq!(StaticReleaseFormat::default(), StaticReleaseFormat::V3);
    assert_eq!(
        StaticReleaseBuilder::default(),
        StaticReleaseBuilder::new(StaticReleaseFormat::V3)
    );

    let dir = temp_dir("default-format");
    let chunks = vec![StaticChunk {
        doc_id: "d1".to_string(),
        text: "t".to_string(),
        metadata: HashMap::new(),
        corpus_type: CorpusType::Legal,
    }];
    let manifest = StaticReleaseBuilder::default()
        .build_release(
            &dir,
            &chunks,
            &[finite_embedding(0.1)],
            &sample_profile(),
            &sample_source(),
        )
        .expect("build_release should succeed");

    assert_eq!(manifest.turbo_version, 3);
    let names: Vec<&str> = manifest.outputs.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, V3_RELEASE_OUTPUT_FILES);
    assert_eq!(MmapIndex::load(&dir).unwrap().version(), 3);
}

#[test]
fn release_builder_rejects_a_v4_codec_config_it_cannot_write() {
    let prod = TurboQuantConfig::prod_v1();
    // Each config with the reason the builder gives for refusing it.
    let unwritable = [
        // Not a v4 codec at all.
        (
            TurboQuantConfig::legacy_v1(),
            "legacy_3bit_v1 is not turbo_quant_prod_v1",
        ),
        // The record's 128 index bytes hold 2-bit indices.
        (
            TurboQuantConfig {
                mse_bits: 3,
                ..prod
            },
            "got mse_bits 3",
        ),
        // 256 sign bits are not the record's 64 sign bytes.
        (
            TurboQuantConfig {
                qjl_dim: 256,
                ..prod
            },
            "the codec config produces 128 and 32",
        ),
        // Only the current generator can be run.
        (
            TurboQuantConfig {
                generator_version: prod.generator_version + 1,
                ..prod
            },
            "cannot generate generator_version 2 assets",
        ),
    ];

    let chunks = vec![StaticChunk {
        doc_id: "d1".to_string(),
        text: "t".to_string(),
        metadata: HashMap::new(),
        corpus_type: CorpusType::Legal,
    }];
    for (config, reason) in unwritable {
        // The builder stages next to the output directory, so whatever a
        // failed build leaves behind is an entry of `parent`.
        let parent = temp_dir("unwritable-v4-config");
        let error = StaticReleaseBuilder::new(StaticReleaseFormat::V4(config))
            .build_release(
                &parent.join("release"),
                &chunks,
                &[finite_embedding(0.1)],
                &sample_profile(),
                &sample_source(),
            )
            .expect_err("a config the v4 format cannot write must be rejected")
            .to_string();
        assert!(error.contains(reason), "{config:?}: {error}");
        assert_eq!(
            fs::read_dir(&parent).unwrap().count(),
            0,
            "{config:?} must not leave files behind"
        );
    }
}
