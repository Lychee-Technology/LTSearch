//! Plumbing tests for the `Legacy3BitV1` assets, `CentroidTable` and
//! `ProjectionMatrix`: seeded generation, the byte format, and the public
//! surface of the v2 static builder.
//!
//! Like every `legacy_plumbing_*` test, it uses hand-written centroid tables
//! and identity projections wherever it needs assets with known values, so
//! expected results can be worked out by hand. No release uses such assets,
//! so nothing here is evidence about codec quality:
//! `turbo_prod_statistics_test.rs` checks the production codec's bias and
//! distortion, and `turbo_bench` (#168) its retrieval quality.

use ltsearch::index::{
    encode_vector, CentroidTable, PreparedTurboQuery, ProjectionMatrix, StaticChunk,
    StaticIndexBuildResult, StaticIndexBuilder, StaticSourceConfig, TurboBuildConfig,
    TurboRecord512,
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

    assert!(encoded.gamma.is_finite());

    let centroids = CentroidTable::generate(512, 4, 7);
    let prepared =
        PreparedTurboQuery::prepare(&[0.1; 512], &centroids, &identity_projection(512)).unwrap();
    let record = TurboRecord512 {
        doc_id: 1,
        idx: [0; 128],
        qjl: [0; 64],
        gamma: encoded.gamma,
        _reserved: [0; 4],
    };
    let score = prepared.score(&record);

    assert!(score.is_finite());
}
