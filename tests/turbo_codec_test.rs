use ltsearch::index::{
    encode_vector, score_query_against_record, CentroidTable, ProjectionMatrix, TurboHeader,
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
fn score_query_against_record_uses_centroid_dot_plus_gamma_weighted_sign_dot() {
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
    let header = TurboHeader::new(4, 1);
    let query = [2.0, -1.0, 0.5, 3.0];
    let encoded_query = encode_vector(&query, &centroids, &projection).unwrap();
    let record = record_bytes(&header, &[0x86], &[0x05], 0.547_722_6);

    let score = score_query_against_record(
        &query,
        &encoded_query,
        &record,
        &header,
        &centroids,
        &projection,
    )
    .unwrap();

    assert!((score - 6.273_861_4).abs() < 1e-6);
}

#[test]
fn score_query_against_record_rejects_invalid_encoded_query_layout() {
    let centroids = centroid_table(4, 4, &[0.0; 16]);
    let projection = identity_projection(4);
    let header = TurboHeader::new(4, 1);
    let record = record_bytes(&header, &[0], &[0], 0.0);

    let error = score_query_against_record(
        &[0.0; 4],
        &Default::default(),
        &record,
        &header,
        &centroids,
        &projection,
    )
    .unwrap_err();

    assert!(error.to_string().contains("expected 1"));
}

#[test]
fn score_query_against_record_rejects_truncated_record_layout() {
    let centroids = centroid_table(4, 4, &[0.0; 16]);
    let projection = identity_projection(4);
    let header = TurboHeader::new(4, 1);
    let encoded = encode_vector(&[0.0; 4], &centroids, &projection).unwrap();

    let error = score_query_against_record(
        &[0.0; 4],
        &encoded,
        &[0u8; 1],
        &header,
        &centroids,
        &projection,
    )
    .unwrap_err();

    assert!(error.to_string().contains("expected"));
}

#[test]
fn encode_vector_rejects_non_square_projection_layout() {
    let centroids = centroid_table(4, 4, &[0.0; 16]);
    let projection = ProjectionMatrix::generate(4, 3, 7);

    let error = encode_vector(&[0.0; 4], &centroids, &projection).unwrap_err();

    assert!(error.to_string().contains("dimension mismatch"));
}
