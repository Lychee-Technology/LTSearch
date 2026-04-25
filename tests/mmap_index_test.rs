use std::fs;
use std::path::PathBuf;

use ltsearch::index::{
    CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader, META_RECORD_SIZE,
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
    let stride = header.record_stride();

    let mut bin_data = header.to_bytes();
    for &(doc_id, gamma) in records {
        let mut record_buf = vec![0u8; stride];
        record_buf[0..8].copy_from_slice(&doc_id.to_le_bytes());
        let gamma_off = header.gamma_offset();
        record_buf[gamma_off..gamma_off + 4].copy_from_slice(&gamma.to_le_bytes());
        bin_data.extend_from_slice(&record_buf);
    }
    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();

    let mut text_blob = Vec::new();
    let mut text_offsets = Vec::new();
    for text in texts {
        text_offsets.push((text_blob.len() as u64, text.len() as u32));
        text_blob.extend_from_slice(text.as_bytes());
    }
    fs::write(dir.join("turbo_static_text.bin"), &text_blob).unwrap();

    let mut meta_data = Vec::new();
    for (i, &(doc_id, _)) in records.iter().enumerate() {
        let (text_offset, text_len) = text_offsets[i];
        let meta = MetaRecord {
            doc_id,
            corpus_type: meta_corpus_types[i],
            _pad: [0; 3],
            text_offset,
            text_len,
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
    fs::write(dir.join("turbo_static_meta.bin"), &[]).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), &[]).unwrap();
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
    write_test_index(&dir, 384, &[(1, 0.0)], &[0], &["x"]);

    let index = MmapIndex::load(&dir).unwrap();
    assert_eq!(index.dim(), 384);
    assert_eq!(index.record_count(), 1);
    assert_eq!(index.record(0).idx().len(), 96);
    assert_eq!(index.record(0).qjl().len(), 48);
}
