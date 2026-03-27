use super::types::{Centroids, ProjectionMatrix, TurboRecord};

/// Extract the 2-bit index for dimension `dim` from the packed `idx` array.
/// Each byte holds 4 indices (2 bits each), MSB first.
pub fn get_idx(idx: &[u8; 96], dim: usize) -> usize {
    let byte = idx[dim / 4];
    let shift = 6 - (dim % 4) * 2;
    ((byte >> shift) & 0b11) as usize
}

/// Extract the sign bit for dimension `dim` from the packed `qjl` array.
/// Returns +1.0 if bit is 1, -1.0 if bit is 0.
pub fn get_sign(qjl: &[u8; 48], dim: usize) -> f32 {
    let byte = qjl[dim / 8];
    let bit = (byte >> (7 - dim % 8)) & 1;
    if bit == 1 { 1.0 } else { -1.0 }
}

/// Reconstruct x̃_mse from stored indices and centroids.
/// Returns a 384-dimensional vector.
pub fn reconstruct_mse(idx: &[u8; 96], centroids: &Centroids) -> Vec<f32> {
    (0..384)
        .map(|dim| centroids.values[dim][get_idx(idx, dim)])
        .collect()
}

/// Compute dot product of two equal-length slices.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Compute S^T · sign(qjl): for each output dimension i,
/// result[i] = sum_j S[j][i] * sign(qjl[j])
fn apply_qjl_transpose(qjl: &[u8; 48], projection: &ProjectionMatrix) -> Vec<f32> {
    let mut result = vec![0.0f32; projection.cols];
    for j in 0..projection.rows {
        let sign = get_sign(qjl, j);
        let row = projection.row(j);
        for (i, &s) in row.iter().enumerate() {
            result[i] += sign * s;
        }
    }
    result
}

/// Compute the TurboQuant_prod score between query `y` and a compressed record.
/// score = ⟨y, x̃_mse⟩ + γ · ⟨y, S^T · sign(qjl)⟩
pub fn score(
    y: &[f32],
    record: &TurboRecord,
    centroids: &Centroids,
    projection: &ProjectionMatrix,
) -> f32 {
    // Copy fields out of the packed struct to avoid unaligned reference warnings.
    let idx = record.idx;
    let qjl = record.qjl;
    let gamma = record.gamma;

    let x_mse = reconstruct_mse(&idx, centroids);
    let mse_term = dot(y, &x_mse);

    let qjl_vec = apply_qjl_transpose(&qjl, projection);
    let qjl_term = dot(y, &qjl_vec);

    mse_term + gamma * qjl_term
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_idx_first_dim() {
        let mut idx = [0u8; 96];
        // Set first byte to 0b11_00_00_00 → dim 0 = 3
        idx[0] = 0b1100_0000;
        assert_eq!(get_idx(&idx, 0), 3);
    }

    #[test]
    fn get_idx_second_dim() {
        let mut idx = [0u8; 96];
        // Set first byte to 0b00_10_00_00 → dim 1 = 2
        idx[0] = 0b0010_0000;
        assert_eq!(get_idx(&idx, 1), 2);
    }

    #[test]
    fn get_sign_set_bit() {
        let mut qjl = [0u8; 48];
        qjl[0] = 0b1000_0000;  // dim 0 bit = 1
        assert_eq!(get_sign(&qjl, 0), 1.0);
        assert_eq!(get_sign(&qjl, 1), -1.0);
    }

    #[test]
    fn score_is_finite() {
        let centroids = Centroids {
            values: vec![[-1.0, -0.33, 0.33, 1.0]; 384],
        };
        let projection = ProjectionMatrix {
            values: vec![0.01; 384 * 384],
            rows: 384,
            cols: 384,
        };
        let record = TurboRecord {
            doc_id: 1,
            idx: [0b01_01_01_01; 96],
            qjl: [0b1010_1010; 48],
            gamma: 0.5,
        };
        let y = vec![0.1f32; 384];
        let s = score(&y, &record, &centroids, &projection);
        assert!(s.is_finite(), "score must be finite, got {s}");
    }

    #[test]
    fn score_zero_gamma() {
        // With gamma=0, QJL term vanishes; score = dot(y, x_mse)
        let centroids = Centroids {
            values: vec![[0.0, 1.0, 2.0, 3.0]; 384],
        };
        let projection = ProjectionMatrix {
            values: vec![0.0; 384 * 384],
            rows: 384,
            cols: 384,
        };
        let record = TurboRecord {
            doc_id: 1,
            idx: [0b01_01_01_01; 96],  // all dims → centroid[1] = 1.0
            qjl: [0u8; 48],
            gamma: 0.0,
        };
        let y = vec![1.0f32; 384];
        let s = score(&y, &record, &centroids, &projection);
        // dot([1.0; 384], [1.0; 384]) = 384.0
        assert!((s - 384.0).abs() < 1e-3, "expected ~384.0, got {s}");
    }
}
