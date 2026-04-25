use super::{AssetError, CentroidTable, ProjectionMatrix, TurboHeader, TurboRecord512};

const IDX_BITS_PER_DIM: usize = 2;
const EXPECTED_CENTROIDS_PER_DIM: usize = 1 << IDX_BITS_PER_DIM;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EncodedTurboVector {
    pub idx: Vec<u8>,
    pub qjl: Vec<u8>,
    pub gamma: f32,
}

pub fn encode_vector(
    vector: &[f32],
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) -> Result<EncodedTurboVector, AssetError> {
    validate_codec_inputs(vector.len(), centroids, projection)?;

    let mut idx = vec![0; idx_len(vector.len())];
    let mut residual = vec![0.0; vector.len()];

    for dim in 0..vector.len() {
        let (centroid_index, centroid_value) = nearest_centroid(vector[dim], centroids, dim);
        write_idx(&mut idx, dim, centroid_index as u8);
        residual[dim] = vector[dim] - centroid_value;
    }

    let projected = projection.project_checked(&residual)?;
    let mut qjl = vec![0; qjl_len(projected.len())];
    for (dim, value) in projected.iter().enumerate() {
        write_sign_bit(&mut qjl, dim, *value >= 0.0);
    }

    let gamma = residual
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();

    Ok(EncodedTurboVector { idx, qjl, gamma })
}

pub fn score_query_against_record(
    query: &[f32],
    encoded: &EncodedTurboVector,
    record: &[u8],
    header: &TurboHeader,
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) -> Result<f32, AssetError> {
    if query.len() != header.dim() as usize {
        return Err(AssetError::DimensionMismatch {
            expected: header.dim() as usize,
            actual: query.len(),
        });
    }

    validate_codec_inputs(query.len(), centroids, projection)?;
    validate_encoded_vector(encoded, query.len())?;

    if record.len() < header.record_stride() {
        return Err(AssetError::InvalidSize {
            minimum: header.record_stride(),
            actual: record.len(),
        });
    }

    let idx = &record[header.idx_offset()..header.idx_offset() + header.idx_size()];
    let qjl = &record[header.qjl_offset()..header.qjl_offset() + header.qjl_size()];
    let gamma_start = header.gamma_offset();
    let gamma = f32::from_le_bytes(record[gamma_start..gamma_start + 4].try_into().unwrap());

    let centroid_score = (0..query.len())
        .map(|dim| query[dim] * centroid_value(centroids, dim, read_idx(idx, dim) as usize))
        .sum::<f32>();

    let projected_query = projection.project_checked(query)?;
    let qjl_score = projected_query
        .iter()
        .enumerate()
        .map(|(dim, value)| value * if read_sign_bit(qjl, dim) { 1.0 } else { -1.0 })
        .sum::<f32>();

    Ok(centroid_score + gamma * qjl_score)
}

pub fn score_query_against_record_512(
    query: &[f32],
    encoded: &EncodedTurboVector,
    record: &TurboRecord512,
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) -> Result<f32, AssetError> {
    validate_codec_inputs(query.len(), centroids, projection)?;
    validate_encoded_vector(encoded, query.len())?;

    let centroid_score = (0..query.len())
        .map(|dim| query[dim] * centroid_value(centroids, dim, read_idx(&record.idx, dim) as usize))
        .sum::<f32>();

    let projected_query = projection.project_checked(query)?;
    let qjl_score = projected_query
        .iter()
        .enumerate()
        .map(|(dim, value)| value * if read_sign_bit(&record.qjl, dim) { 1.0 } else { -1.0 })
        .sum::<f32>();

    Ok(centroid_score + record.gamma * qjl_score)
}

fn validate_codec_inputs(
    vector_dim: usize,
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) -> Result<(), AssetError> {
    if centroids.dim() as usize != vector_dim {
        return Err(AssetError::DimensionMismatch {
            expected: vector_dim,
            actual: centroids.dim() as usize,
        });
    }

    if centroids.centroids_per_dim() as usize != EXPECTED_CENTROIDS_PER_DIM {
        return Err(AssetError::InvalidLayout {
            expected_values: EXPECTED_CENTROIDS_PER_DIM,
            actual_values: centroids.centroids_per_dim() as usize,
        });
    }

    if projection.input_dim() as usize != vector_dim {
        return Err(AssetError::DimensionMismatch {
            expected: vector_dim,
            actual: projection.input_dim() as usize,
        });
    }

    if projection.output_dim() as usize != vector_dim {
        return Err(AssetError::DimensionMismatch {
            expected: vector_dim,
            actual: projection.output_dim() as usize,
        });
    }

    Ok(())
}

fn validate_encoded_vector(encoded: &EncodedTurboVector, dim: usize) -> Result<(), AssetError> {
    let expected_idx_len = idx_len(dim);
    if encoded.idx.len() != expected_idx_len {
        return Err(AssetError::InvalidLayout {
            expected_values: expected_idx_len,
            actual_values: encoded.idx.len(),
        });
    }

    let expected_qjl_len = qjl_len(dim);
    if encoded.qjl.len() != expected_qjl_len {
        return Err(AssetError::InvalidLayout {
            expected_values: expected_qjl_len,
            actual_values: encoded.qjl.len(),
        });
    }

    Ok(())
}

fn idx_len(dim: usize) -> usize {
    (dim * IDX_BITS_PER_DIM).div_ceil(8)
}

fn qjl_len(dim: usize) -> usize {
    dim.div_ceil(8)
}

fn nearest_centroid(value: f32, centroids: &CentroidTable, dim: usize) -> (usize, f32) {
    let start = dim * EXPECTED_CENTROIDS_PER_DIM;
    let values = &centroids.values()[start..start + EXPECTED_CENTROIDS_PER_DIM];

    let mut best_index = 0;
    let mut best_value = values[0];
    let mut best_distance = (value - best_value).abs();

    for (index, candidate) in values.iter().copied().enumerate().skip(1) {
        let distance = (value - candidate).abs();
        if distance < best_distance {
            best_index = index;
            best_value = candidate;
            best_distance = distance;
        }
    }

    (best_index, best_value)
}

fn centroid_value(centroids: &CentroidTable, dim: usize, centroid_index: usize) -> f32 {
    centroids.values()[dim * EXPECTED_CENTROIDS_PER_DIM + centroid_index]
}

fn write_idx(out: &mut [u8], dim: usize, index: u8) {
    let bit_offset = dim * IDX_BITS_PER_DIM;
    let byte_offset = bit_offset / 8;
    let shift = bit_offset % 8;
    out[byte_offset] |= index << shift;
}

fn read_idx(bytes: &[u8], dim: usize) -> u8 {
    let bit_offset = dim * IDX_BITS_PER_DIM;
    let byte_offset = bit_offset / 8;
    let shift = bit_offset % 8;
    (bytes[byte_offset] >> shift) & 0b11
}

fn write_sign_bit(out: &mut [u8], dim: usize, is_non_negative: bool) {
    if is_non_negative {
        out[dim / 8] |= 1 << (dim % 8);
    }
}

fn read_sign_bit(bytes: &[u8], dim: usize) -> bool {
    (bytes[dim / 8] >> (dim % 8)) & 1 == 1
}
