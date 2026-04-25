use ltsearch::index::{
    encode_vector, score_query_against_record, CentroidTable, EncodedTurboVector, ProjectionMatrix,
    StaticChunk, StaticIndexBuildResult, StaticIndexBuilder, StaticSourceConfig, TurboBuildConfig,
    TurboHeader,
};

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

fn record_bytes(header: &TurboHeader, idx: &[u8], qjl: &[u8], gamma: f32) -> Vec<u8> {
    let mut record = vec![0u8; header.record_stride()];
    let idx_offset = header.idx_offset();
    record[idx_offset..idx_offset + idx.len()].copy_from_slice(idx);
    let qjl_offset = header.qjl_offset();
    record[qjl_offset..qjl_offset + qjl.len()].copy_from_slice(qjl);
    let gamma_offset = header.gamma_offset();
    record[gamma_offset..gamma_offset + 4].copy_from_slice(&gamma.to_le_bytes());
    record
}

#[test]
fn centroid_table_generation_is_deterministic() {
    let first = CentroidTable::generate(384, 16, 7);
    let second = CentroidTable::generate(384, 16, 7);
    let different = CentroidTable::generate(384, 16, 8);

    assert_eq!(first, second);
    assert_ne!(first, different);
    assert_eq!(first.dim(), 384);
    assert_eq!(first.centroids_per_dim(), 16);
    assert_eq!(first.values().len(), 384 * 16);
}

#[test]
fn centroid_table_roundtrips_through_bytes() {
    let table = CentroidTable::generate(512, 8, 13);

    let bytes = table.to_bytes();
    let restored = CentroidTable::from_bytes(&bytes).unwrap();

    assert_eq!(restored, table);
}

#[test]
fn projection_matrix_generation_is_deterministic() {
    let first = ProjectionMatrix::generate(384, 48, 99);
    let second = ProjectionMatrix::generate(384, 48, 99);
    let different = ProjectionMatrix::generate(384, 48, 100);

    assert_eq!(first, second);
    assert_ne!(first, different);
    assert_eq!(first.input_dim(), 384);
    assert_eq!(first.output_dim(), 48);
    assert_eq!(first.values().len(), 384 * 48);
}

#[test]
fn projection_matrix_roundtrips_through_bytes() {
    let matrix = ProjectionMatrix::generate(512, 64, 23);

    let bytes = matrix.to_bytes();
    let restored = ProjectionMatrix::from_bytes(&bytes).unwrap();

    assert_eq!(restored, matrix);
}

#[test]
fn projection_matrix_projects_vector() {
    let matrix = ProjectionMatrix::from_rows(vec![vec![1.0, 2.0, 3.0], vec![-1.0, 0.5, 4.0]]);
    let projected = matrix.project(&[2.0, -1.0, 0.5]);

    assert_eq!(projected.len(), 2);
    assert!((projected[0] - 1.5).abs() < 1e-6);
    assert!((projected[1] - (-0.5)).abs() < 1e-6);
}

#[test]
fn projection_matrix_rejects_invalid_bytes() {
    let err = ProjectionMatrix::from_bytes(&[0u8; 8]).unwrap_err();
    assert!(err.to_string().contains("size"));
}

#[test]
fn projection_matrix_rejects_dimension_mismatch() {
    let matrix = ProjectionMatrix::from_rows(vec![vec![1.0, 2.0, 3.0]]);
    let err = matrix.project_checked(&[1.0, 2.0]).unwrap_err();
    assert!(err.to_string().contains("dimension"));
}

#[test]
fn phase_two_public_surface_compiles() {
    let _chunk = StaticChunk::default();
    let _result = StaticIndexBuildResult::default();
    let _builder = StaticIndexBuilder::<()>::new();
    let _source = StaticSourceConfig::default();
    let _build = TurboBuildConfig::default();

    let centroids = centroid_table(2, 4, &[0.0, 1.0, 2.0, 3.0, -2.0, -1.0, 0.0, 1.0]);
    let projection = identity_projection(2);
    let encoded = encode_vector(&[0.2, -0.1], &centroids, &projection).unwrap();

    let header = TurboHeader::new(2, 1);
    let record = record_bytes(&header, &encoded.idx, &encoded.qjl, encoded.gamma);
    let score = score_query_against_record(
        &[0.2, -0.1],
        &encoded,
        &record,
        &header,
        &centroids,
        &projection,
    )
    .unwrap();

    assert!(score.is_finite());
}

#[test]
fn turbo_codec_requires_query_encoding_to_match_layout() {
    let centroids = centroid_table(8, 4, &[0.0; 32]);
    let projection = identity_projection(8);
    let header = TurboHeader::new(8, 1);
    let record = record_bytes(&header, &[0, 0], &[0], 0.0);

    let error = score_query_against_record(
        &[0.0; 8],
        &EncodedTurboVector::default(),
        &record,
        &header,
        &centroids,
        &projection,
    )
    .unwrap_err();

    assert!(error.to_string().contains("expected"));
}
