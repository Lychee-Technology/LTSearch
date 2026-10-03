use super::{AssetError, CentroidTable, ProjectionMatrix, TurboQuantConfig, TurboRecord512};

const LEGACY: TurboQuantConfig = TurboQuantConfig::legacy_v1();
const IDX_BITS_PER_DIM: usize = LEGACY.mse_bits as usize;
const IDX_MASK: u8 = (1 << IDX_BITS_PER_DIM) - 1;
const EXPECTED_CENTROIDS_PER_DIM: usize = LEGACY.centroids_per_dim() as usize;
// `write_idx`/`read_idx` assume an index never straddles a byte boundary.
const _: () = assert!(8 % IDX_BITS_PER_DIM == 0);
/// Dimension of the only typed record layout, [`TurboRecord512`].
const RECORD_512_DIM: usize = 512;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EncodedTurboVector {
    pub idx: Vec<u8>,
    pub qjl: Vec<u8>,
    pub gamma: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TurboScoreBreakdown {
    pub centroid_term: f32,
    pub qjl_term: f32,
    pub gamma_multiplier: f32,
}

impl TurboScoreBreakdown {
    pub fn total(self) -> f32 {
        self.centroid_term + self.gamma_multiplier * self.qjl_term
    }
}

/// A query prepared for scoring legacy [`TurboRecord512`] records.
///
/// Everything that depends only on the query (validation against the assets,
/// the query × centroid products, and the projection `S·q`) is computed once
/// in [`prepare`](Self::prepare), so [`score`](Self::score) is infallible and
/// does no allocation, matvec, or dequantization per record. Scoring keeps the
/// legacy formula `Σ_d q_d·c_d[idx_d] + γ·Σ_j sign_j·(S·q)_j` with the same
/// multiplications and summation order as the per-record scorer it replaced,
/// so scores are bit-identical to it.
#[derive(Debug, Clone)]
pub struct PreparedTurboQuery {
    /// `centroid_lut[d][k] = q_d · c_d[k]`.
    centroid_lut: [[f32; EXPECTED_CENTROIDS_PER_DIM]; RECORD_512_DIM],
    /// `S·q`.
    projected_query: [f32; RECORD_512_DIM],
}

impl PreparedTurboQuery {
    pub fn prepare(
        query: &[f32],
        centroids: &CentroidTable,
        projection: &ProjectionMatrix,
    ) -> Result<Self, AssetError> {
        if query.len() != RECORD_512_DIM {
            return Err(AssetError::DimensionMismatch {
                expected: RECORD_512_DIM,
                actual: query.len(),
            });
        }
        validate_codec_inputs(query.len(), centroids, projection)?;

        let mut centroid_lut = [[0.0; EXPECTED_CENTROIDS_PER_DIM]; RECORD_512_DIM];
        for (dim, products) in centroid_lut.iter_mut().enumerate() {
            for (centroid_index, product) in products.iter_mut().enumerate() {
                *product = query[dim] * centroid_value(centroids, dim, centroid_index);
            }
        }

        let projected_query =
            projection
                .project_checked(query)?
                .try_into()
                .map_err(|projected: Vec<f32>| AssetError::DimensionMismatch {
                    expected: RECORD_512_DIM,
                    actual: projected.len(),
                })?;

        Ok(Self {
            centroid_lut,
            projected_query,
        })
    }

    pub fn score(&self, record: &TurboRecord512) -> f32 {
        self.score_breakdown(record).total()
    }

    pub fn score_breakdown(&self, record: &TurboRecord512) -> TurboScoreBreakdown {
        let centroid_term = self
            .centroid_lut
            .iter()
            .enumerate()
            .map(|(dim, products)| products[read_idx(&record.idx, dim) as usize])
            .sum::<f32>();

        let qjl_term = self
            .projected_query
            .iter()
            .enumerate()
            .map(|(dim, value)| {
                value
                    * if read_sign_bit(&record.qjl, dim) {
                        1.0
                    } else {
                        -1.0
                    }
            })
            .sum::<f32>();

        TurboScoreBreakdown {
            centroid_term,
            qjl_term,
            gamma_multiplier: record.gamma,
        }
    }
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
    (bytes[byte_offset] >> shift) & IDX_MASK
}

fn write_sign_bit(out: &mut [u8], dim: usize, is_non_negative: bool) {
    if is_non_negative {
        out[dim / 8] |= 1 << (dim % 8);
    }
}

fn read_sign_bit(bytes: &[u8], dim: usize) -> bool {
    (bytes[dim / 8] >> (dim % 8)) & 1 == 1
}
