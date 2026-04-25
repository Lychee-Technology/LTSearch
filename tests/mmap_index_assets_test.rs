use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::index::{
    CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader, TurboRecord512,
    META_RECORD_SIZE,
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
    assert_eq!(dim, 512);

    let mut bin_data = header.to_bytes();
    for &(doc_id, gamma) in records {
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
                std::mem::size_of::<TurboRecord512>(),
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
    write_test_index(&dir, 512, &[(7, 1.25)], &[1], &["asset-backed"]);

    let centroids = CentroidTable::generate(512, 16, 11);
    let projection = ProjectionMatrix::generate(512, 512, 19);
    write_test_assets(&dir, &centroids, &projection);

    let index = MmapIndex::load(&dir).unwrap();

    assert_eq!(index.centroids(), &centroids);
    assert_eq!(index.projection(), &projection);
}

#[test]
fn mmap_index_rejects_projection_input_dim_mismatch() {
    let dir = temp_dir("projection-dim-mismatch");
    write_test_index(&dir, 512, &[(7, 1.25)], &[1], &["asset-backed"]);

    write_test_assets(
        &dir,
        &CentroidTable::generate(512, 16, 11),
        &ProjectionMatrix::generate(511, 512, 19),
    );

    let err = MmapIndex::load(&dir).unwrap_err();

    assert!(err.to_string().contains("projection"));
    assert!(err.to_string().contains("512"));
    assert!(err.to_string().contains("511"));
}

#[test]
fn mmap_index_rejects_projection_output_dim_mismatch() {
    let dir = temp_dir("projection-output-dim-mismatch");
    write_test_index(&dir, 512, &[(7, 1.25)], &[1], &["asset-backed"]);

    write_test_assets(
        &dir,
        &CentroidTable::generate(512, 16, 11),
        &ProjectionMatrix::generate(512, 511, 19),
    );

    let err = MmapIndex::load(&dir).unwrap_err();

    assert!(err.to_string().contains("projection"));
    assert!(err.to_string().contains("512"));
    assert!(err.to_string().contains("511"));
}
