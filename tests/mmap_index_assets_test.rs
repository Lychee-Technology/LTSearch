use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::index::{
    CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader, META_RECORD_SIZE,
};

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-mmap-assets-{name}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_test_index(
    dir: &Path,
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
}

fn write_test_assets(dir: &Path, centroids: &CentroidTable, projection: &ProjectionMatrix) {
    fs::write(dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(dir.join("projection.bin"), projection.to_bytes()).unwrap();
}

#[test]
fn mmap_index_loads_centroids_and_projection_assets() {
    let dir = temp_dir("load-assets");
    write_test_index(&dir, 384, &[(7, 1.25)], &[1], &["asset-backed"]);

    let centroids = CentroidTable::generate(384, 16, 11);
    let projection = ProjectionMatrix::generate(384, 384, 19);
    write_test_assets(&dir, &centroids, &projection);

    let index = MmapIndex::load(&dir).unwrap();

    assert_eq!(index.centroids(), &centroids);
    assert_eq!(index.projection(), &projection);
}

#[test]
fn mmap_index_rejects_projection_input_dim_mismatch() {
    let dir = temp_dir("projection-dim-mismatch");
    write_test_index(&dir, 384, &[(7, 1.25)], &[1], &["asset-backed"]);

    write_test_assets(
        &dir,
        &CentroidTable::generate(384, 16, 11),
        &ProjectionMatrix::generate(383, 384, 19),
    );

    let err = MmapIndex::load(&dir).unwrap_err();

    assert!(err.to_string().contains("projection"));
    assert!(err.to_string().contains("384"));
    assert!(err.to_string().contains("383"));
}

#[test]
fn mmap_index_rejects_projection_output_dim_mismatch() {
    let dir = temp_dir("projection-output-dim-mismatch");
    write_test_index(&dir, 384, &[(7, 1.25)], &[1], &["asset-backed"]);

    write_test_assets(
        &dir,
        &CentroidTable::generate(384, 16, 11),
        &ProjectionMatrix::generate(384, 383, 19),
    );

    let err = MmapIndex::load(&dir).unwrap_err();

    assert!(err.to_string().contains("projection"));
    assert!(err.to_string().contains("384"));
    assert!(err.to_string().contains("383"));
}
