use ltsearch::index::{
    encode_vector, CentroidTable, PreparedTurboQuery, ProjectionMatrix, TurboRecord512,
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
fn encode_vector_packs_centroid_indexes_qjl_bits_and_gamma() {
    let centroids = centroid_table(
        4,
        4,
        &[
            -1.0, 0.0, 1.0, 2.0, // dim 0
            -2.0, -1.0, 0.0, 1.0, // dim 1
            0.0, 1.0, 2.0, 3.0, // dim 2
            -1.0, 0.0, 1.0, 3.0, // dim 3
        ],
    );
    let projection = identity_projection(4);

    let encoded = encode_vector(&[1.2, -1.4, 0.3, 0.9], &centroids, &projection).unwrap();

    assert_eq!(encoded.idx, vec![0x86]);
    assert_eq!(encoded.qjl, vec![0x05]);
    assert!((encoded.gamma - 0.547_722_6).abs() < 1e-6);
}

#[test]
fn encode_vector_rejects_centroid_tables_that_do_not_fit_two_bit_layout() {
    let centroids = centroid_table(4, 8, &[0.0; 32]);
    let projection = identity_projection(4);

    let error = encode_vector(&[0.0; 4], &centroids, &projection).unwrap_err();

    assert!(error.to_string().contains("expected 4"));
}

#[test]
fn encode_vector_rejects_non_square_projection_layout() {
    let centroids = centroid_table(4, 4, &[0.0; 16]);
    let projection = ProjectionMatrix::generate(4, 3, 7);

    let error = encode_vector(&[0.0; 4], &centroids, &projection).unwrap_err();

    assert!(error.to_string().contains("dimension mismatch"));
}

/// The record carries the 4-d encoding from
/// `encode_vector_packs_centroid_indexes_qjl_bits_and_gamma` (idx `[2, 1, 0, 2]`
/// = 0x86, signs `[+, -, +, -]` = 0x05, gamma 0.547_722_6) in dims 0..4; the
/// query is zero in the remaining 508 dims.
fn known_answer_fixture() -> (Vec<f32>, CentroidTable, TurboRecord512) {
    let dim = 512;
    let mut centroid_values = vec![0.0; dim * 4];
    centroid_values[0..16].copy_from_slice(&[
        0.0, 0.0, 1.0, 0.0, // dim 0
        0.0, -1.0, 0.0, 0.0, // dim 1
        0.0, 0.0, 0.0, 0.0, // dim 2
        0.0, 0.0, 1.0, 0.0, // dim 3
    ]);
    let centroids = centroid_table(dim as u32, 4, &centroid_values);
    let mut query = vec![0.0; dim];
    query[..4].copy_from_slice(&[2.0, -1.0, 0.5, 3.0]);

    let mut record = zero_record();
    record.gamma = 0.547_722_6;
    record.idx[0] = 0x86;
    record.qjl[0] = 0x05;

    (query, centroids, record)
}

fn zero_record() -> TurboRecord512 {
    TurboRecord512 {
        doc_id: 1,
        idx: [0; 128],
        qjl: [0; 64],
        gamma: 0.0,
        _reserved: [0; 4],
    }
}

#[test]
fn prepared_query_scores_centroid_dot_plus_gamma_weighted_sign_dot() {
    let (query, centroids, record) = known_answer_fixture();
    let prepared =
        PreparedTurboQuery::prepare(&query, &centroids, &identity_projection(512)).unwrap();

    // 2·1 + (-1)·(-1) + 0.5·0 + 3·1 = 6, plus 0.547_722_6 · (2 + 1 + 0.5 - 3).
    assert!((prepared.score(&record) - 6.273_861_4).abs() < 1e-6);
}

#[test]
fn prepared_query_breakdown_separates_centroid_qjl_and_gamma_terms() {
    let (query, centroids, record) = known_answer_fixture();
    let prepared =
        PreparedTurboQuery::prepare(&query, &centroids, &identity_projection(512)).unwrap();

    let breakdown = prepared.score_breakdown(&record);

    assert!((breakdown.centroid_term - 6.0).abs() < 1e-6);
    assert!((breakdown.qjl_term - 0.5).abs() < 1e-6);
    assert!((breakdown.gamma_multiplier - 0.547_722_6).abs() < 1e-6);
    assert_eq!(
        prepared.score(&record).to_bits(),
        breakdown.total().to_bits()
    );
}

#[test]
fn prepared_query_scores_all_zero_record_as_finite() {
    let centroids = centroid_table(512, 4, &[0.0; 512 * 4]);
    let prepared =
        PreparedTurboQuery::prepare(&[0.0; 512], &centroids, &identity_projection(512)).unwrap();

    assert!(prepared.score(&zero_record()).is_finite());
}

#[test]
fn prepared_query_rejects_query_dimension_mismatch() {
    let centroids = centroid_table(512, 4, &[0.0; 512 * 4]);

    let error = PreparedTurboQuery::prepare(&[0.0; 256], &centroids, &identity_projection(512))
        .unwrap_err();

    assert!(error.to_string().contains("dimension mismatch"));
}

#[test]
fn prepared_query_rejects_centroid_table_of_another_dimension() {
    let centroids = centroid_table(256, 4, &[0.0; 256 * 4]);

    let error = PreparedTurboQuery::prepare(&[0.0; 512], &centroids, &identity_projection(512))
        .unwrap_err();

    assert!(error.to_string().contains("dimension mismatch"));
}

#[test]
fn prepared_query_rejects_centroid_tables_that_do_not_fit_two_bit_layout() {
    let centroids = centroid_table(512, 8, &[0.0; 512 * 8]);

    let error = PreparedTurboQuery::prepare(&[0.0; 512], &centroids, &identity_projection(512))
        .unwrap_err();

    assert!(error.to_string().contains("expected 4"));
}

#[test]
fn prepared_query_rejects_non_square_projection_layout() {
    let centroids = centroid_table(512, 4, &[0.0; 512 * 4]);
    let projection = ProjectionMatrix::generate(512, 256, 7);

    let error = PreparedTurboQuery::prepare(&[0.0; 512], &centroids, &projection).unwrap_err();

    assert!(error.to_string().contains("dimension mismatch"));
}
