use super::scorer::dot;
use super::types::{Centroids, ProjectionMatrix, TurboRecord};

/// Pack a 2-bit index value into `idx` at position `dim`.
/// Layout matches get_idx: 4 indices per byte, MSB first.
pub fn set_idx(idx: &mut [u8; 96], dim: usize, value: u8) {
    debug_assert!(value < 4);
    let byte_pos = dim / 4;
    let shift = 6 - (dim % 4) * 2;
    idx[byte_pos] &= !(0b11 << shift);
    idx[byte_pos] |= (value & 0b11) << shift;
}

/// Pack a sign bit into `qjl` at position `dim`.
/// bit=1 means positive (+1), bit=0 means negative (-1).
/// Layout matches get_sign: 8 bits per byte, MSB first.
pub fn set_sign(qjl: &mut [u8; 48], dim: usize, positive: bool) {
    let byte_pos = dim / 8;
    let bit_pos = 7 - (dim % 8);
    if positive {
        qjl[byte_pos] |= 1 << bit_pos;
    } else {
        qjl[byte_pos] &= !(1 << bit_pos);
    }
}

/// Apply projection matrix to vector x (matrix is row-major).
/// Returns a vector of length `pi.rows`.
pub fn rotate(x: &[f32], pi: &ProjectionMatrix) -> Vec<f32> {
    (0..pi.rows).map(|i| dot(pi.row(i), x)).collect()
}

/// Find the nearest centroid index (0..4) for a scalar value.
fn nearest_centroid(value: f32, centroids: &[f32; 4]) -> u8 {
    let mut best_idx = 0u8;
    let mut best_dist = f32::MAX;
    for (i, &c) in centroids.iter().enumerate() {
        let d = (value - c).abs();
        if d < best_dist {
            best_dist = d;
            best_idx = i as u8;
        }
    }
    best_idx
}

/// Compress a float32 vector into a TurboRecord.
///
/// # Arguments
/// - `x`: input embedding (length must equal `centroids.values.len()`)
/// - `doc_id`: identifier for this chunk
/// - `pi`: random rotation matrix (same for all records, deterministic seed)
/// - `centroids`: per-dimension MSE centroids
/// - `s`: QJL projection matrix
pub fn compress(
    x: &[f32],
    doc_id: u64,
    pi: &ProjectionMatrix,
    centroids: &Centroids,
    s: &ProjectionMatrix,
) -> TurboRecord {
    let dims = centroids.values.len();
    assert_eq!(x.len(), dims);

    // Stage 1: rotate and 2-bit MSE-quantize.
    let x_rot = rotate(x, pi);
    let mut idx = [0u8; 96];
    let mut x_mse = vec![0.0f32; dims];
    for d in 0..dims {
        let ci = nearest_centroid(x_rot[d], &centroids.values[d]);
        set_idx(&mut idx, d, ci);
        x_mse[d] = centroids.values[d][ci as usize];
    }

    // Stage 2: residual and QJL sign quantization.
    let residual: Vec<f32> = x_rot.iter().zip(x_mse.iter()).map(|(a, b)| a - b).collect();
    let gamma = residual.iter().map(|v| v * v).sum::<f32>().sqrt();

    let mut qjl = [0u8; 48];
    for j in 0..s.rows {
        let proj = dot(s.row(j), &residual);
        set_sign(&mut qjl, j, proj >= 0.0);
    }

    TurboRecord {
        doc_id,
        idx,
        qjl,
        gamma,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turbo::scorer;

    fn identity_matrix(dims: usize) -> ProjectionMatrix {
        let mut values = vec![0.0f32; dims * dims];
        for i in 0..dims {
            values[i * dims + i] = 1.0;
        }
        ProjectionMatrix {
            values,
            rows: dims,
            cols: dims,
        }
    }

    #[test]
    fn set_get_idx_roundtrip() {
        let mut idx = [0u8; 96];
        for dim in 0..384 {
            let val = (dim % 4) as u8;
            set_idx(&mut idx, dim, val);
            assert_eq!(scorer::get_idx(&idx, dim), val as usize, "dim={dim}");
        }
    }

    #[test]
    fn set_get_sign_roundtrip() {
        let mut qjl = [0u8; 48];
        for dim in 0..384 {
            let positive = dim % 2 == 0;
            set_sign(&mut qjl, dim, positive);
            let expected = if positive { 1.0f32 } else { -1.0f32 };
            assert_eq!(scorer::get_sign(&qjl, dim), expected, "dim={dim}");
        }
    }

    #[test]
    fn compress_with_identity_rotation() {
        let dims = 384;
        let pi = identity_matrix(dims);
        let centroids = Centroids {
            values: vec![[-1.5, -0.5, 0.5, 1.5]; dims],
        };
        let s = identity_matrix(dims);
        // 0.6 is nearest to centroid index 2 (value 0.5) in [-1.5, -0.5, 0.5, 1.5]
        let x = vec![0.6f32; dims];

        let record = compress(&x, 99, &pi, &centroids, &s);
        assert_eq!({ record.doc_id }, 99);
        assert!({ record.gamma }.is_finite());
        assert!({ record.gamma } >= 0.0);

        for dim in 0..dims {
            assert_eq!(scorer::get_idx(&record.idx, dim), 2, "dim={dim}");
        }
    }
}
