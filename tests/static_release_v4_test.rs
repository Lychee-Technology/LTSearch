//! The v4 static release path end to end: `StaticReleaseBuilder` in the v4
//! format, `verify_release_dir`, `MmapIndex::load` and `TurboQuantSearcher`,
//! and the ways a release that is incomplete, or stitched together from two
//! releases or two versions, is rejected.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use ltsearch::index::{
    derive_release_id, release_output_files, sha256_hex, EmbeddingProfile, EncodedTurboProd,
    IndexCodec, KnownRecordLayout, ManifestCodec, MmapIndex, MmapIndexError, OutputFile,
    ReleaseManifest, ReleaseSource, StaticChunk, StaticReleaseBuilder, StaticReleaseFormat,
    TurboHeader, TurboHeaderError, TurboProdError, TurboQuantConfig, TurboQuantProdV1,
    CODEBOOK_FILE, QJL_FILE, RELEASE_MANIFEST_FILE, ROTATION_FILE, V3_RELEASE_OUTPUT_FILES,
    V4_RELEASE_OUTPUT_FILES,
};
use ltsearch::indexing::{verify_release_dir, StaticActivateError};
use ltsearch::models::{CorpusType, IndexManifest};
use ltsearch::query::{StaticRetriever, TurboQuantSearcher};
use ltsearch::storage::{ActiveManifest, ManifestHead};
use serde_json::{json, Value};
use tempfile::TempDir;

const MODEL_ID: &str = "jina-v5-nano/512";
const DOC_COUNT: u64 = 48;
/// The fixture document whose embedding is the zero vector.
const ZERO_VECTOR_DOC: u64 = 7;
const TURBO_STATIC_FILE: &str = "turbo_static.bin";

fn v4() -> StaticReleaseFormat {
    StaticReleaseFormat::V4(TurboQuantConfig::prod_v1())
}

/// The v4 format with other seeds: the same file set and codebook as
/// [`v4`], but another rotation and another QJL matrix.
fn v4_with_other_seeds() -> StaticReleaseFormat {
    StaticReleaseFormat::V4(TurboQuantConfig {
        mse_seed: 9163,
        qjl_seed: 9165,
        ..TurboQuantConfig::prod_v1()
    })
}

// --- build → verify → load → search ------------------------------------------

#[test]
fn v4_release_builds_verifies_loads_and_searches_like_the_reference_codec() {
    let dir = TempDir::new().unwrap();
    let release = dir.path().join("release");

    let manifest = build(v4(), &release);
    assert_eq!(manifest.turbo_version, 4);
    assert_eq!(output_names(&manifest), V4_RELEASE_OUTPUT_FILES);
    let mut on_disk = file_names(&release);
    on_disk.retain(|name| name != RELEASE_MANIFEST_FILE);
    assert_eq!(on_disk, V4_RELEASE_OUTPUT_FILES);

    let verified = verify_release_dir(&release, Some(MODEL_ID), Some(512)).unwrap();
    assert_eq!(verified, manifest);

    let index = MmapIndex::load(&release).unwrap();
    assert_eq!(index.version(), 4);
    assert_eq!(index.layout(), KnownRecordLayout::V4Dim512);
    assert_eq!(index.record_count(), DOC_COUNT);
    let IndexCodec::Prod(loaded) = index.codec() else {
        panic!("a v4 index loads the TurboQuantProdV1 codec");
    };
    assert_eq!(*loaded.config(), TurboQuantConfig::prod_v1());

    // The reference never touches the release: it is the codec generated
    // from the config, scoring codes it encoded itself.
    let reference = TurboQuantProdV1::generate(TurboQuantConfig::prod_v1()).unwrap();
    let (chunks, embeddings) = fixture();
    let codes: Vec<EncodedTurboProd> = embeddings
        .iter()
        .map(|embedding| reference.encode(embedding).unwrap())
        .collect();

    let searcher = TurboQuantSearcher::new(Arc::new(index));
    for query in queries() {
        let ranking = reference_ranking(&reference, &chunks, &codes, &query);
        for top_k in [1, 10, DOC_COUNT as usize] {
            let results = searcher.search(&stub_manifest(), &query, top_k).unwrap();
            let actual: Vec<(String, u32)> = results
                .iter()
                .map(|result| (result.doc_id.clone(), result.score.to_bits()))
                .collect();
            assert_eq!(actual, ranking[..top_k], "top_k {top_k}");

            // The winners come back with their own sidecar rows.
            for result in &results {
                let chunk = chunks
                    .iter()
                    .find(|chunk| chunk.doc_id == result.doc_id)
                    .unwrap();
                assert_eq!(result.text, chunk.text);
                assert_eq!(result.metadata.as_ref(), Some(&chunk.metadata));
                assert_eq!(
                    result.citation.as_ref().and_then(|c| c.title.as_deref()),
                    chunk.metadata["title"].as_str()
                );
            }
        }
    }
}

#[test]
fn v4_manifest_records_the_full_codec_config() {
    let dir = TempDir::new().unwrap();
    let release = dir.path().join("release");
    let manifest = build(v4(), &release);

    let json: Value =
        serde_json::from_slice(&fs::read(release.join(RELEASE_MANIFEST_FILE)).unwrap()).unwrap();
    assert_eq!(json["manifest_schema_version"], 1);
    assert_eq!(json["turbo_version"], 4);
    assert_eq!(
        json["codec"],
        json!({
            "codec_id": "turbo_quant_prod_v1",
            "dim": 512,
            "mse_bits": 2,
            "qjl_dim": 512,
            "rotation_seed": 163,
            "qjl_seed": 165,
            "generator_version": 1,
            "norm_policy": "normalize_and_store",
        })
    );

    let ManifestCodec::V4(codec) = &manifest.codec else {
        panic!("a v4 manifest has a v4 codec section");
    };
    assert_eq!(
        TurboQuantConfig::from_v4_codec_metadata(codec),
        Ok(TurboQuantConfig::prod_v1())
    );
}

#[test]
fn v4_release_is_byte_identical_across_two_builds() {
    let dir = TempDir::new().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");

    let first_manifest = build(v4(), &first);
    let second_manifest = build(v4(), &second);

    assert_eq!(first_manifest, second_manifest);
    let digests = file_digests(&first);
    assert_eq!(digests.len(), V4_RELEASE_OUTPUT_FILES.len() + 1);
    assert_eq!(digests, file_digests(&second));
}

/// The fixture's v4 bytes, pinned. The release_id covers every file through
/// the manifest's hashes, so it moves with the header, the record layout,
/// an asset format, a generator or the encoder. A release already built
/// keeps its bytes, so a change that moves these needs a new format version
/// or codec id, not new values here.
#[test]
fn v4_release_bytes_are_pinned() {
    let dir = TempDir::new().unwrap();
    let release = dir.path().join("release");
    let manifest = build(v4(), &release);

    let digest = |name: &str| sha256_hex(&fs::read(release.join(name)).unwrap());
    assert_eq!(
        digest(CODEBOOK_FILE),
        "25f9488a86f7aabf6b3bfc4a685f713639e7735d02f55613d8489fe37bb84e7d"
    );
    assert_eq!(
        digest(QJL_FILE),
        "91711f45c09a3780bd5378382032f744ffaab386cc76288fe07e537a67182b6f"
    );
    assert_eq!(
        digest(ROTATION_FILE),
        "31835fa7a5d584316449d4c401985f7df7f11f45948ec714ef4f5a0c10a77a59"
    );
    assert_eq!(
        digest(TURBO_STATIC_FILE),
        "f0fc38cd92f62db4724ef05064c85b64271ca82816ffc08a4029bf10404b635c"
    );
    assert_eq!(
        manifest.release_id,
        "1c59189d1f9b24d62461e54248893240b8006155ff30cc0a9268d9a42ded50a6"
    );
}

#[test]
fn the_codec_seeds_change_the_release_id() {
    let dir = TempDir::new().unwrap();
    let prod = build(v4(), &dir.path().join("prod"));
    let other = build(v4_with_other_seeds(), &dir.path().join("other"));
    let v3 = build(StaticReleaseFormat::V3, &dir.path().join("v3"));

    // Same documents every time: only the format and codec differ.
    assert_eq!(
        prod.input_fingerprint.content_digest,
        other.input_fingerprint.content_digest
    );
    assert_eq!(
        prod.input_fingerprint.content_digest,
        v3.input_fingerprint.content_digest
    );
    assert_ne!(prod.release_id, other.release_id);
    assert_ne!(prod.release_id, v3.release_id);
}

// --- incomplete releases -----------------------------------------------------

/// Every v4 file, cut short the ways an interrupted write or upload leaves
/// it. `verify_release_dir` rejects each one on its size. So does
/// `MmapIndex::load`, with an error about the file that was cut, except for
/// the text and title blobs, whose row offsets it does not check against the
/// blob length (#175); for those two verify is the only gate.
#[test]
fn a_truncated_v4_file_is_rejected() {
    const NOT_BOUNDS_CHECKED_AT_LOAD: [&str; 2] =
        ["turbo_static_text.bin", "turbo_static_title.bin"];

    let dir = TempDir::new().unwrap();
    let pristine = dir.path().join("pristine");
    build(v4(), &pristine);
    assert!(MmapIndex::load(&pristine).is_ok());

    let mut cases: Vec<(&str, u64)> = Vec::new();
    for name in V4_RELEASE_OUTPUT_FILES {
        let len = fs::metadata(pristine.join(name)).unwrap().len();
        assert!(len > 1, "{name} is too small to truncate");
        cases.extend([(name, 0), (name, len / 2), (name, len - 1)]);
        if name == TURBO_STATIC_FILE {
            let header = TurboHeader::SIZE as u64;
            // Inside the header, the header alone, and one whole record short.
            cases.extend([(name, header / 2), (name, header), (name, len - 208)]);
        }
    }

    for (index, (name, len)) in cases.into_iter().enumerate() {
        let damaged = dir.path().join(format!("truncated-{index}"));
        copy_release(&pristine, &damaged);
        fs::OpenOptions::new()
            .write(true)
            .open(damaged.join(name))
            .unwrap()
            .set_len(len)
            .unwrap();

        let message = verify_error(&damaged);
        assert!(
            message.contains(&format!("output {name} size mismatch")),
            "verify of {name} truncated to {len} bytes: {message}"
        );
        if !NOT_BOUNDS_CHECKED_AT_LOAD.contains(&name) {
            let error = MmapIndex::load(&damaged).unwrap_err();
            assert!(
                blames(&error, name),
                "load of {name} truncated to {len} bytes: {error}"
            );
        }
    }
}

#[test]
fn a_missing_v4_file_is_rejected_at_load_and_verify() {
    let dir = TempDir::new().unwrap();
    let pristine = dir.path().join("pristine");
    build(v4(), &pristine);

    for name in V4_RELEASE_OUTPUT_FILES {
        let damaged = dir.path().join(format!("without-{name}"));
        copy_release(&pristine, &damaged);
        fs::remove_file(damaged.join(name)).unwrap();

        assert!(
            matches!(MmapIndex::load(&damaged), Err(MmapIndexError::Io { .. })),
            "load accepted a release without {name}"
        );
        assert!(
            verify_release_dir(&damaged, None, None).is_err(),
            "verify accepted a release without {name}"
        );
    }
}

/// A doc_id or metadata sidecar whose bytes aren't UTF-8, in a release whose
/// manifest hashes match them. `MmapIndex::load` accepts it, because it checks
/// the sidecar ranges and leaves UTF-8 to each read; `verify_release_dir`
/// checks every entry and rejects it before activation.
#[test]
fn a_sidecar_that_is_not_utf8_loads_but_is_rejected_at_verify() {
    let dir = TempDir::new().unwrap();
    for (version, format) in [("v3", StaticReleaseFormat::V3), ("v4", v4())] {
        let release = dir.path().join(version);
        build(format, &release);
        for blob in ["docid", "meta_json"] {
            let damaged = dir.path().join(format!("{version}-{blob}"));
            copy_release(&release, &damaged);
            let path = damaged.join(format!("turbo_static_{blob}.bin"));
            let len = fs::metadata(&path).unwrap().len() as usize;
            fs::write(&path, vec![0xFF; len]).unwrap();
            forge_manifest(&damaged, |_| {});

            let index = MmapIndex::load(&damaged).unwrap();
            let err = index.check_sidecar_utf8().unwrap_err();
            assert!(
                matches!(err, MmapIndexError::MetaExtBlobInvalidUtf8 { index: 0, blob: b } if b == blob),
                "{version} {blob}: {err:?}"
            );
            let message = verify_error(&damaged);
            assert!(
                message.contains(&format!("meta ext {blob} blob contains invalid UTF-8")),
                "{version} {blob}: {message}"
            );
        }
    }
}

// --- records and assets of different builds ----------------------------------

#[test]
fn v4_records_with_another_releases_assets_are_rejected_at_load() {
    let dir = TempDir::new().unwrap();
    let ours = dir.path().join("ours");
    let theirs = dir.path().join("theirs");
    build(v4(), &ours);
    build(v4_with_other_seeds(), &theirs);

    // Each of the other release's seeded assets alone, then all of them.
    for swapped in [
        &[ROTATION_FILE][..],
        &[QJL_FILE][..],
        &[CODEBOOK_FILE, QJL_FILE, ROTATION_FILE][..],
    ] {
        let mixed = dir.path().join(format!("mixed-{}", swapped.join("-")));
        copy_release(&ours, &mixed);
        for name in swapped {
            fs::copy(theirs.join(name), mixed.join(name)).unwrap();
        }

        let error = MmapIndex::load(&mixed).unwrap_err();
        assert!(
            matches!(error, MmapIndexError::CodecFingerprintMismatch { .. }),
            "swapped {swapped:?}: {error}"
        );
        assert!(
            error.to_string().contains("not from the same build"),
            "swapped {swapped:?}: {error}"
        );
    }

    // The other way round: their records over our assets.
    let mixed = dir.path().join("mixed-records");
    copy_release(&ours, &mixed);
    fs::copy(
        theirs.join(TURBO_STATIC_FILE),
        mixed.join(TURBO_STATIC_FILE),
    )
    .unwrap();
    assert!(matches!(
        MmapIndex::load(&mixed),
        Err(MmapIndexError::CodecFingerprintMismatch { .. })
    ));
}

#[test]
fn a_v4_asset_with_one_changed_value_is_rejected_at_load() {
    let dir = TempDir::new().unwrap();
    let pristine = dir.path().join("pristine");
    build(v4(), &pristine);

    // The lowest bit of each asset's last f32: the smallest change a value
    // can have. It is past every header field, so the file still parses and
    // still has the shape and seed its header names.
    let load_with_changed = |name: &str| {
        let damaged = dir.path().join(format!("changed-{name}"));
        copy_release(&pristine, &damaged);
        let path = damaged.join(name);
        let mut bytes = fs::read(&path).unwrap();
        let last_f32 = bytes.len() - 4;
        bytes[last_f32] ^= 0x01;
        fs::write(&path, bytes).unwrap();
        MmapIndex::load(&damaged).unwrap_err()
    };

    // Nothing but the header fingerprint tells a changed matrix from the
    // one the records were encoded with.
    for name in [QJL_FILE, ROTATION_FILE] {
        let error = load_with_changed(name);
        assert!(
            matches!(error, MmapIndexError::CodecFingerprintMismatch { .. }),
            "{name}: {error}"
        );
    }
    // The codebook is refused earlier: the codec only takes the committed one.
    let error = load_with_changed(CODEBOOK_FILE);
    assert!(
        matches!(
            error,
            MmapIndexError::Codec(TurboProdError::NotTheCommittedCodebook)
        ),
        "{CODEBOOK_FILE}: {error}"
    );
}

#[test]
fn a_v4_asset_with_a_bad_header_is_rejected_at_load() {
    let dir = TempDir::new().unwrap();
    let pristine = dir.path().join("pristine");
    build(v4(), &pristine);

    for name in [CODEBOOK_FILE, QJL_FILE, ROTATION_FILE] {
        let damaged = dir.path().join(format!("bad-magic-{name}"));
        copy_release(&pristine, &damaged);
        let path = damaged.join(name);
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 0xFF;
        fs::write(&path, bytes).unwrap();

        match MmapIndex::load(&damaged) {
            Err(MmapIndexError::Asset { file, .. }) => assert_eq!(file, name),
            other => panic!("{name}: expected an asset error, got {other:?}"),
        }
    }
}

#[test]
fn a_v4_image_with_an_unknown_header_version_is_rejected() {
    let dir = TempDir::new().unwrap();
    let pristine = dir.path().join("pristine");
    build(v4(), &pristine);

    for version in [0u32, 1, 5] {
        let damaged = dir.path().join(format!("version-{version}"));
        copy_release(&pristine, &damaged);
        let path = damaged.join(TURBO_STATIC_FILE);
        let mut bytes = fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
        fs::write(&path, bytes).unwrap();

        assert!(
            matches!(
                MmapIndex::load(&damaged),
                Err(MmapIndexError::Header(TurboHeaderError::UnsupportedVersion { version: found }))
                    if found == version
            ),
            "load accepted header version {version}"
        );

        // With the manifest's hashes brought in line, verify still refuses
        // the image, for the loader's reason.
        forge_manifest(&damaged, |_| {});
        let message = verify_error(&damaged);
        assert!(
            message.contains(&format!("unsupported version: {version}")),
            "{message}"
        );
    }
}

// --- manifests and files of different versions -------------------------------

#[test]
fn a_v3_manifest_over_v4_files_is_rejected() {
    let dir = TempDir::new().unwrap();
    let v3_release = dir.path().join("v3");
    let v4_release = dir.path().join("v4");
    let v3_manifest = build(StaticReleaseFormat::V3, &v3_release);
    build(v4(), &v4_release);

    // The manifest file alone, dropped over a v4 release.
    let swapped = dir.path().join("swapped");
    copy_release(&v4_release, &swapped);
    fs::copy(
        v3_release.join(RELEASE_MANIFEST_FILE),
        swapped.join(RELEASE_MANIFEST_FILE),
    )
    .unwrap();
    verify_error(&swapped);

    // A forged one: every file a v3 manifest lists is present and hashes to
    // its entry, and the release_id is the derived one. Only the image
    // itself says it is a v4 release.
    let forged = dir.path().join("forged");
    copy_release(&v4_release, &forged);
    for name in ["centroids.bin", "projection.bin"] {
        fs::copy(v3_release.join(name), forged.join(name)).unwrap();
    }
    forge_manifest(&forged, |manifest| *manifest = v3_manifest.clone());
    let message = verify_error(&forged);
    assert!(
        message.contains("loaded image version 4 != manifest turbo_version 3"),
        "{message}"
    );
}

#[test]
fn a_v4_manifest_over_v3_files_is_rejected() {
    let dir = TempDir::new().unwrap();
    let v3_release = dir.path().join("v3");
    let v4_release = dir.path().join("v4");
    build(StaticReleaseFormat::V3, &v3_release);
    let v4_manifest = build(v4(), &v4_release);

    let swapped = dir.path().join("swapped");
    copy_release(&v3_release, &swapped);
    fs::copy(
        v4_release.join(RELEASE_MANIFEST_FILE),
        swapped.join(RELEASE_MANIFEST_FILE),
    )
    .unwrap();
    verify_error(&swapped);

    let forged = dir.path().join("forged");
    copy_release(&v3_release, &forged);
    for name in [CODEBOOK_FILE, QJL_FILE, ROTATION_FILE] {
        fs::copy(v4_release.join(name), forged.join(name)).unwrap();
    }
    forge_manifest(&forged, |manifest| *manifest = v4_manifest.clone());
    let message = verify_error(&forged);
    assert!(
        message.contains("loaded image version 3 != manifest turbo_version 4"),
        "{message}"
    );
}

#[test]
fn a_codec_section_of_the_other_version_is_rejected() {
    let dir = TempDir::new().unwrap();
    let v3_release = dir.path().join("v3");
    let v4_release = dir.path().join("v4");
    let v3_manifest = build(StaticReleaseFormat::V3, &v3_release);
    let v4_manifest = build(v4(), &v4_release);

    // The codec section is hashed into the release_id as whichever shape it
    // has, so both of these manifests are self-consistent.
    forge_manifest(&v4_release, |manifest| {
        manifest.codec = v3_manifest.codec.clone()
    });
    let message = verify_error(&v4_release);
    assert!(
        message.contains("turbo_version 4 has a v3 codec section"),
        "{message}"
    );

    forge_manifest(&v3_release, |manifest| {
        manifest.codec = v4_manifest.codec.clone()
    });
    let message = verify_error(&v3_release);
    assert!(
        message.contains("turbo_version 3 has a v4 codec section"),
        "{message}"
    );
}

#[test]
fn a_turbo_version_without_a_release_format_is_rejected() {
    let dir = TempDir::new().unwrap();
    let release = dir.path().join("release");
    build(v4(), &release);

    for turbo_version in [0, 2, 5] {
        forge_manifest(&release, |manifest| manifest.turbo_version = turbo_version);
        let message = verify_error(&release);
        assert!(
            message.contains(&format!("turbo_version {turbo_version} is not")),
            "{message}"
        );
    }
}

#[test]
fn a_v4_manifest_whose_codec_is_not_the_assets_codec_is_rejected() {
    let dir = TempDir::new().unwrap();
    let pristine = dir.path().join("pristine");
    build(v4(), &pristine);

    let edit_codec = |name: &str, edit: fn(&mut ltsearch::index::V4CodecMetadata)| {
        let forged = dir.path().join(name);
        copy_release(&pristine, &forged);
        forge_manifest(&forged, |manifest| {
            let ManifestCodec::V4(codec) = &mut manifest.codec else {
                panic!("a v4 manifest has a v4 codec section");
            };
            edit(codec);
        });
        verify_error(&forged)
    };

    // The loader reads the codec from the asset files, so a manifest that
    // names another seed describes a codec the release doesn't have.
    let message = edit_codec("rotation-seed", |codec| codec.rotation_seed += 1);
    assert!(message.contains("the loaded assets' codec"), "{message}");
    let message = edit_codec("qjl-seed", |codec| codec.qjl_seed += 1);
    assert!(message.contains("the loaded assets' codec"), "{message}");

    // Names and values this build doesn't know are refused by name.
    let message = edit_codec("codec-id", |codec| {
        codec.codec_id = "turbo_quant_prod_v2".to_string()
    });
    assert!(message.contains("turbo_quant_prod_v2"), "{message}");
    let message = edit_codec("norm-policy", |codec| {
        codec.norm_policy = "renormalize".to_string()
    });
    assert!(message.contains("renormalize"), "{message}");
    let message = edit_codec("legacy-codec", |codec| {
        codec.codec_id = "legacy_3bit_v1".to_string()
    });
    assert!(message.contains("invalid codec section"), "{message}");
}

// --- helpers -----------------------------------------------------------------

fn build(format: StaticReleaseFormat, output_dir: &Path) -> ReleaseManifest {
    let (chunks, embeddings) = fixture();
    StaticReleaseBuilder::new(format)
        .build_release(
            output_dir,
            &chunks,
            &embeddings,
            &EmbeddingProfile {
                model_id: MODEL_ID.to_string(),
                dim: 512,
            },
            &ReleaseSource {
                kind: "lance".to_string(),
                dataset_path: "/data/v4.lance".to_string(),
                table_version: 3,
                table_row_count: chunks.len() as u64,
                corpus_type: CorpusType::Legal,
            },
        )
        .unwrap()
}

/// `DOC_COUNT` doc_id-sorted chunks. The embeddings are not unit vectors and
/// their norms grow with the index, so a reader that drops or misreads the
/// stored norm ranks them differently; one is the zero vector, which the
/// codec stores with norm 0.
fn fixture() -> (Vec<StaticChunk>, Vec<Vec<f32>>) {
    let mut chunks = Vec::new();
    let mut embeddings = Vec::new();
    for index in 0..DOC_COUNT {
        let mut metadata: HashMap<String, Value> = HashMap::new();
        metadata.insert("title".to_string(), json!(format!("标题 {index}")));
        metadata.insert("section".to_string(), json!(index));
        chunks.push(StaticChunk {
            doc_id: format!("doc-{index:02}"),
            text: format!("第{index}条 body text"),
            metadata,
            corpus_type: CorpusType::Legal,
        });

        embeddings.push(if index == ZERO_VECTOR_DOC {
            vec![0.0; 512]
        } else {
            let scale = 0.25 + index as f32 * 0.125;
            let mut state = 0x5EED_0000 + index;
            (0..512).map(|_| scale * signed_unit(&mut state)).collect()
        });
    }
    (chunks, embeddings)
}

/// Three non-unit queries drawn like the fixture's embeddings.
fn queries() -> Vec<Vec<f32>> {
    (0..3u64)
        .map(|index| {
            let mut state = 0xC0DE_0000 + index;
            (0..512).map(|_| signed_unit(&mut state)).collect()
        })
        .collect()
}

/// Every fixture document as `(doc_id, score bits)`, best first, scored by
/// `codec` alone.
fn reference_ranking(
    codec: &TurboQuantProdV1,
    chunks: &[StaticChunk],
    codes: &[EncodedTurboProd],
    query: &[f32],
) -> Vec<(String, u32)> {
    let prepared = codec.prepare_query(query).unwrap();
    let mut scored: Vec<(&str, f32)> = chunks
        .iter()
        .zip(codes)
        .map(|(chunk, code)| (chunk.doc_id.as_str(), prepared.score(code.code())))
        .collect();
    scored.sort_by(|left, right| right.1.total_cmp(&left.1));

    // The searcher breaks ties on the hashed doc_id; with no ties in the
    // fixture the reference doesn't need to know the hash.
    assert!(scored.windows(2).all(|pair| pair[0].1 != pair[1].1));
    let zero_vector_doc = format!("doc-{ZERO_VECTOR_DOC:02}");
    assert!(scored
        .iter()
        .any(|(doc_id, score)| *doc_id == zero_vector_doc && *score == 0.0));

    scored
        .into_iter()
        .map(|(doc_id, score)| (doc_id.to_string(), score.to_bits()))
        .collect()
}

/// A SplitMix64 draw mapped to [-1, 1), so the fixture is deterministic
/// without a `rand` dependency.
fn signed_unit(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 23) as f32 - 1.0
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

fn output_names(manifest: &ReleaseManifest) -> Vec<&str> {
    manifest
        .outputs
        .iter()
        .map(|output| output.name.as_str())
        .collect()
}

/// The names of the files in `dir`, sorted.
fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `(file name, sha256)` of every file in `dir`, sorted by name.
fn file_digests(dir: &Path) -> Vec<(String, String)> {
    file_names(dir)
        .into_iter()
        .map(|name| {
            let digest = sha256_hex(&fs::read(dir.join(&name)).unwrap());
            (name, digest)
        })
        .collect()
}

fn copy_release(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for name in file_names(from) {
        fs::copy(from.join(&name), to.join(&name)).unwrap();
    }
}

/// Whether a load error is about the release file `name`.
fn blames(error: &MmapIndexError, name: &str) -> bool {
    match error {
        MmapIndexError::FileSizeMismatch { file, .. } | MmapIndexError::Asset { file, .. } => {
            *file == name
        }
        MmapIndexError::MetaCountMismatch { .. } => name == "turbo_static_meta.bin",
        MmapIndexError::MetaExtCountMismatch { .. } => name == "turbo_static_meta_ext.bin",
        MmapIndexError::MetaExtBlobOutOfBounds { blob, .. } => {
            name == format!("turbo_static_{blob}.bin")
        }
        _ => false,
    }
}

/// The message `verify_release_dir` rejects `dir` with.
fn verify_error(dir: &Path) -> String {
    match verify_release_dir(dir, None, None) {
        Err(StaticActivateError::Verify { message }) => message,
        other => panic!("expected a verify error, got {other:?}"),
    }
}

/// Applies `edit` to the manifest at `dir`, then makes it self-consistent
/// again the way a forger would: each file its `turbo_version` lists gets the
/// hash and size of the file on disk, and `release_id` is re-derived. What
/// verify rejects after this, it rejects on substance, not on a stale hash.
fn forge_manifest(dir: &Path, edit: impl FnOnce(&mut ReleaseManifest)) {
    let path = dir.join(RELEASE_MANIFEST_FILE);
    let mut manifest: ReleaseManifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);

    if let Some(names) = release_output_files(manifest.turbo_version) {
        manifest.outputs = names
            .iter()
            .map(|name| {
                let bytes = fs::read(dir.join(name)).unwrap();
                OutputFile {
                    name: name.to_string(),
                    sha256: sha256_hex(&bytes),
                    size_bytes: bytes.len() as u64,
                }
            })
            .collect();
    }
    manifest.release_id = derive_release_id(
        manifest.turbo_version,
        &manifest.embedding_profile,
        &manifest.codec,
        &manifest.input_fingerprint.content_digest,
        &manifest.outputs,
    );
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

// Both file sets are referenced so a rename of either constant is caught here.
const _: () = assert!(V3_RELEASE_OUTPUT_FILES.len() == 9 && V4_RELEASE_OUTPUT_FILES.len() == 10);
