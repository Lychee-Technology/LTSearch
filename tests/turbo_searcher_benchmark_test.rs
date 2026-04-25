use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader,
    META_RECORD_SIZE,
};
use ltsearch::query::TurboQuantSearcher;

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-turbo-bench-{name}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn centroid_table(dim: u32, centroids_per_dim: u32, values: &[f32]) -> CentroidTable {
    let mut bytes = Vec::with_capacity(8 + values.len() * 4);
    bytes.extend_from_slice(&dim.to_le_bytes());
    bytes.extend_from_slice(&centroids_per_dim.to_le_bytes());
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    CentroidTable::from_bytes(&bytes).unwrap()
}

fn identity_projection(dim: usize) -> ProjectionMatrix {
    let mut rows = Vec::with_capacity(dim);
    for row_index in 0..dim {
        let mut row = vec![0.0; dim];
        row[row_index] = 1.0;
        rows.push(row);
    }
    ProjectionMatrix::from_rows(rows)
}

fn write_benchmark_index(dir: &Path, doc_count: u64) {
    let dim = 4;
    let centroids = centroid_table(
        dim,
        4,
        &[
            -1.0, 0.0, 1.0, 2.0, -2.0, -1.0, 0.0, 1.0, 0.0, 1.0, 2.0, 3.0, -1.0, 0.0, 1.0, 3.0,
        ],
    );
    let projection = identity_projection(dim as usize);
    let header = TurboHeader::new(dim, doc_count);
    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();

    for doc_id in 0..doc_count {
        let embedding = [1.0, 0.5, (doc_id % 7) as f32 * 0.1, -0.25];
        let encoded = encode_vector(&embedding, &centroids, &projection).unwrap();

        let mut record = vec![0u8; header.record_stride()];
        record[0..8].copy_from_slice(&(doc_id + 1).to_le_bytes());
        let idx_offset = header.idx_offset();
        record[idx_offset..idx_offset + encoded.idx.len()].copy_from_slice(&encoded.idx);
        let qjl_offset = header.qjl_offset();
        record[qjl_offset..qjl_offset + encoded.qjl.len()].copy_from_slice(&encoded.qjl);
        let gamma_offset = header.gamma_offset();
        record[gamma_offset..gamma_offset + 4].copy_from_slice(&encoded.gamma.to_le_bytes());
        bin_data.extend_from_slice(&record);

        let text = format!("document {doc_id}");
        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(text.as_bytes());
        let meta = MetaRecord {
            doc_id: doc_id + 1,
            corpus_type: (doc_id % 3) as u8,
            _pad: [0; 3],
            text_offset,
            text_len: text.len() as u32,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }

    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(dir.join("projection.bin"), projection.to_bytes()).unwrap();
}

#[test]
#[ignore = "benchmark-style smoke test"]
fn turbo_searcher_benchmark_smoke_compiles() {
    let dir = temp_dir("smoke");
    write_benchmark_index(&dir, 2_000);

    let index = Box::new(MmapIndex::load(&dir).unwrap());
    let searcher = TurboQuantSearcher::new(Box::leak(index));

    let results = searcher.search(&[1.0, 0.5, 0.0, -0.25], 10).unwrap();

    assert_eq!(results.len(), 10);
}
