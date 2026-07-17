use std::fs;
use std::mem::{align_of, size_of};
use std::path::PathBuf;

use ltsearch::index::mmap_index::MmapIndexError;
use ltsearch::index::{
    CentroidTable, MetaExtRecord, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader,
    TurboRecord512, TurboRecordSlice, META_EXT_RECORD_SIZE, META_RECORD_SIZE,
};

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-mmap-{name}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_test_index(
    dir: &std::path::Path,
    dim: u32,
    records: &[(u64, f32)],
    meta_corpus_types: &[u8],
    texts: &[&str],
) {
    let header = TurboHeader::new(dim, records.len() as u64);

    let mut bin_data = header.to_bytes();
    for &(doc_id, gamma) in records {
        assert_eq!(dim, 512);
        let record = TurboRecord512 {
            doc_id,
            idx: [0; 128],
            qjl: [0; 64],
            gamma,
            _reserved: [0; 4],
        };
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &record as *const TurboRecord512 as *const u8,
                size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);
    }
    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();

    let mut text_blob = Vec::new();
    let mut text_offsets = Vec::new();
    for text in texts {
        text_offsets.push((text_blob.len() as u64, text.len() as u32));
        text_blob.extend_from_slice(text.as_bytes());
    }
    fs::write(dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(dir.join("turbo_static_title.bin"), []).unwrap();

    let mut meta_data = Vec::new();
    for (i, &(doc_id, _)) in records.iter().enumerate() {
        let (text_offset, text_len) = text_offsets[i];
        let meta = MetaRecord {
            doc_id,
            corpus_type: meta_corpus_types[i],
            _pad: [0; 7],
            text_offset,
            text_len,
            title_offset: 0,
            title_len: 0,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }
    fs::write(dir.join("turbo_static_meta.bin"), &meta_data).unwrap();

    fs::write(
        dir.join("centroids.bin"),
        CentroidTable::generate(dim, 16, 7).to_bytes(),
    )
    .unwrap();
    fs::write(
        dir.join("projection.bin"),
        ProjectionMatrix::generate(dim, dim, 11).to_bytes(),
    )
    .unwrap();
}

fn write_unknown_layout_index(dir: &std::path::Path, dim: u32, record_count: u64) {
    let header = TurboHeader::new(dim, record_count);
    let stride = header.record_stride();
    let mut bin_data = header.to_bytes();
    bin_data.resize(TurboHeader::SIZE + stride * record_count as usize, 0);
    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(
        dir.join("turbo_static_meta.bin"),
        vec![0u8; META_RECORD_SIZE * record_count as usize],
    )
    .unwrap();
    fs::write(dir.join("turbo_static_text.bin"), []).unwrap();
    fs::write(dir.join("turbo_static_title.bin"), []).unwrap();
    fs::write(
        dir.join("centroids.bin"),
        CentroidTable::generate(dim, 16, 7).to_bytes(),
    )
    .unwrap();
    fs::write(
        dir.join("projection.bin"),
        ProjectionMatrix::generate(dim, dim, 11).to_bytes(),
    )
    .unwrap();
}

#[test]
fn mmap_index_loads_and_reads_records() {
    let dir = temp_dir("load-records");
    write_test_index(
        &dir,
        512,
        &[(1, 0.5), (2, 1.5), (3, 2.5)],
        &[0, 1, 2],
        &["hello", "world", "test"],
    );

    let index = MmapIndex::load(&dir).unwrap();

    assert_eq!(index.dim(), 512);
    assert_eq!(index.record_count(), 3);
    assert_eq!(index.record(0).doc_id(), 1);
    assert!((index.record(0).gamma() - 0.5).abs() < f32::EPSILON);
    assert_eq!(index.record(2).doc_id(), 3);
}

#[test]
fn mmap_index_reads_metadata_and_text() {
    let dir = temp_dir("load-meta-text");
    write_test_index(
        &dir,
        512,
        &[(10, 0.0), (20, 0.0)],
        &[0, 2],
        &["legal doc", "rfc spec"],
    );

    let index = MmapIndex::load(&dir).unwrap();

    assert_eq!(index.meta(0).doc_id, 10);
    assert_eq!(index.meta(0).corpus_type, 0);
    assert_eq!(index.text(0), "legal doc");

    assert_eq!(index.meta(1).doc_id, 20);
    assert_eq!(index.meta(1).corpus_type, 2);
    assert_eq!(index.text(1), "rfc spec");
}

#[test]
fn mmap_index_rejects_missing_files() {
    let dir = temp_dir("missing-files");
    let err = MmapIndex::load(&dir).unwrap_err();
    assert!(err.to_string().contains("turbo_static.bin"));
}

#[test]
fn mmap_index_rejects_truncated_bin_file() {
    let dir = temp_dir("truncated-bin");
    let header = TurboHeader::new(512, 10);
    fs::write(dir.join("turbo_static.bin"), header.to_bytes()).unwrap();
    fs::write(dir.join("turbo_static_meta.bin"), []).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), []).unwrap();
    fs::write(dir.join("turbo_static_title.bin"), []).unwrap();
    fs::write(
        dir.join("centroids.bin"),
        CentroidTable::generate(512, 16, 7).to_bytes(),
    )
    .unwrap();
    fs::write(
        dir.join("projection.bin"),
        ProjectionMatrix::generate(512, 512, 11).to_bytes(),
    )
    .unwrap();

    let err = MmapIndex::load(&dir).unwrap_err();
    assert!(err.to_string().contains("size"));
}

#[test]
fn mmap_index_rejects_mismatched_meta_count() {
    let dir = temp_dir("mismatched-meta");
    write_test_index(&dir, 512, &[(1, 0.0), (2, 0.0)], &[0, 1], &["a", "b"]);
    let meta_path = dir.join("turbo_static_meta.bin");
    let meta_data = fs::read(&meta_path).unwrap();
    fs::write(&meta_path, &meta_data[..META_RECORD_SIZE]).unwrap();

    let err = MmapIndex::load(&dir).unwrap_err();
    assert!(err.to_string().contains("record count mismatch"));
}

#[test]
fn mmap_index_header_returns_correct_info() {
    let dir = temp_dir("header-info");
    write_test_index(&dir, 512, &[(1, 0.0)], &[0], &["x"]);

    let index = MmapIndex::load(&dir).unwrap();
    assert_eq!(index.dim(), 512);
    assert_eq!(index.record_count(), 1);
    assert_eq!(index.layout(), ltsearch::index::KnownRecordLayout::V2Dim512);
}

#[test]
fn turbo_record_512_has_expected_aligned_layout() {
    assert_eq!(size_of::<TurboRecord512>(), 208);
    assert_eq!(align_of::<TurboRecord512>(), 8);

    let record = TurboRecord512 {
        doc_id: 7,
        idx: [0; 128],
        qjl: [0; 64],
        gamma: 1.5,
        _reserved: [0; 4],
    };

    assert_eq!(record.idx.len(), 128);
    assert_eq!(record.qjl.len(), 64);
    assert_eq!(record.doc_id, 7);
}

#[test]
fn mmap_index_accepts_known_v1_dim_512_layout() {
    let dir = temp_dir("known-layout-512");
    write_test_index(&dir, 512, &[(1, 0.5)], &[0], &["hello"]);

    let index = MmapIndex::load(&dir).unwrap();
    assert_eq!(index.dim(), 512);
    assert_eq!(index.record_count(), 1);
}

#[test]
fn mmap_index_rejects_legacy_v1_image_before_touching_title_blob() {
    // A real pre-title (v1) image on disk has turbo_static.bin with version=1
    // and no turbo_static_title.bin. Load must fail through the header version
    // check, not with an I/O error on the missing title blob.
    let dir = temp_dir("legacy-v1-image");
    let mut header_bytes = TurboHeader::new(512, 1).to_bytes();
    header_bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    fs::write(dir.join("turbo_static.bin"), &header_bytes).unwrap();
    // Deliberately omit turbo_static_title.bin (and the other blobs): the header
    // rejection must happen first.

    let err = MmapIndex::load(&dir).unwrap_err();
    assert!(
        err.to_string().contains("unsupported version"),
        "expected an unsupported-version header error, got: {err}"
    );
}

#[test]
fn mmap_index_rejects_unknown_record_layout() {
    let dir = temp_dir("unknown-layout");
    write_unknown_layout_index(&dir, 384, 1);

    let error = MmapIndex::load(&dir).unwrap_err();
    assert!(error.to_string().contains("unsupported"));
}

fn write_v3_test_index(dir: &std::path::Path, entries: &[(&str, &str)]) {
    let header = TurboHeader::new_v3(512, entries.len() as u64);
    let mut bin_data = header.to_bytes();
    for i in 0..entries.len() {
        let record = TurboRecord512 {
            doc_id: i as u64,
            idx: [0; 128],
            qjl: [0; 64],
            gamma: 0.0,
            _reserved: [0; 4],
        };
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &record as *const TurboRecord512 as *const u8,
                size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);
    }
    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();

    let mut meta_data = Vec::new();
    for i in 0..entries.len() {
        let meta = MetaRecord {
            doc_id: i as u64,
            corpus_type: 0,
            _pad: [0; 7],
            text_offset: 0,
            text_len: 0,
            title_offset: 0,
            title_len: 0,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }
    fs::write(dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), []).unwrap();
    fs::write(dir.join("turbo_static_title.bin"), []).unwrap();
    fs::write(
        dir.join("centroids.bin"),
        CentroidTable::generate(512, 16, 7).to_bytes(),
    )
    .unwrap();
    fs::write(
        dir.join("projection.bin"),
        ProjectionMatrix::generate(512, 512, 11).to_bytes(),
    )
    .unwrap();

    // v3 sidecars: MetaExtRecord array + docid blob + metadata JSON blob.
    let mut docid_blob = Vec::new();
    let mut json_blob = Vec::new();
    let mut ext_data = Vec::new();
    for (doc_id, meta_json) in entries {
        let docid_offset = docid_blob.len() as u64;
        let docid_len = doc_id.len() as u32;
        docid_blob.extend_from_slice(doc_id.as_bytes());
        let meta_json_offset = json_blob.len() as u64;
        let meta_json_len = meta_json.len() as u32;
        json_blob.extend_from_slice(meta_json.as_bytes());
        let ext = MetaExtRecord {
            docid_offset,
            meta_json_offset,
            docid_len,
            meta_json_len,
        };
        let ext_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &ext as *const MetaExtRecord as *const u8,
                META_EXT_RECORD_SIZE,
            )
        };
        ext_data.extend_from_slice(ext_bytes);
    }
    fs::write(dir.join("turbo_static_meta_ext.bin"), &ext_data).unwrap();
    fs::write(dir.join("turbo_static_docid.bin"), &docid_blob).unwrap();
    fs::write(dir.join("turbo_static_meta_json.bin"), &json_blob).unwrap();
}

#[test]
fn mmap_index_loads_v3_and_exposes_original_doc_id_and_metadata_json() {
    let dir = temp_dir("load-v3");
    let doc_id = "doc-α";
    let metadata_json = r#"{"category":"legal","year":2026}"#;
    write_v3_test_index(&dir, &[(doc_id, metadata_json)]);

    let index = MmapIndex::load(&dir).unwrap();

    assert_eq!(index.version(), 3);
    assert_eq!(index.original_doc_id(0), Some(doc_id));

    let json = index
        .metadata_json(0)
        .expect("v3 index exposes metadata json");
    let parsed: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).expect("metadata json parses back to a map");
    assert_eq!(parsed["category"], serde_json::json!("legal"));
    assert_eq!(parsed["year"], serde_json::json!(2026));
}

#[test]
fn mmap_index_still_loads_v2_without_ext_files() {
    let dir = temp_dir("load-v2-no-ext");
    write_test_index(&dir, 512, &[(1, 0.5), (2, 1.5)], &[0, 1], &["a", "b"]);

    let index = MmapIndex::load(&dir).unwrap();

    assert_eq!(index.version(), 2);
    assert_eq!(index.original_doc_id(0), None);
    assert_eq!(index.metadata_json(0), None);
}

#[test]
fn mmap_index_rejects_v3_with_mismatched_ext_count() {
    let dir = temp_dir("v3-mismatch-ext");
    write_v3_test_index(&dir, &[("doc-1", "{}"), ("doc-2", "{}")]);
    // Header declares 2 records; shrink the meta_ext sidecar to a single record.
    let ext_path = dir.join("turbo_static_meta_ext.bin");
    let ext_data = fs::read(&ext_path).unwrap();
    fs::write(&ext_path, &ext_data[..META_EXT_RECORD_SIZE]).unwrap();

    let err = MmapIndex::load(&dir).unwrap_err();
    assert!(
        matches!(
            err,
            MmapIndexError::MetaExtCountMismatch {
                expected: 2,
                actual: 1
            }
        ),
        "expected MetaExtCountMismatch {{ expected: 2, actual: 1 }}, got: {err:?}"
    );
}

#[test]
fn mmap_index_exposes_typed_record_slice() {
    let dir = temp_dir("typed-slice");
    write_test_index(&dir, 512, &[(11, 0.25), (22, 0.75)], &[0, 1], &["a", "b"]);

    let index = MmapIndex::load(&dir).unwrap();

    match index.records() {
        TurboRecordSlice::V2Dim512(records) => {
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].doc_id, 11);
            assert!((records[1].gamma - 0.75).abs() < f32::EPSILON);
        }
    }
}
