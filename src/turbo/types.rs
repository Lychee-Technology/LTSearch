/// One compressed vector entry. repr(C, packed) ensures stable binary layout.
/// Total size: 8 + 96 + 48 + 4 = 156 bytes.
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct TurboRecord {
    pub doc_id: u64,
    pub idx: [u8; 96],   // 384 dims × 2 bits packed
    pub qjl: [u8; 48],   // 384 dims × 1 bit packed
    pub gamma: f32,
}

/// Per-chunk metadata. repr(C, packed) ensures stable binary layout.
/// Total size: 8 + 1 + 3 + 8 + 4 = 24 bytes.
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct MetaRecord {
    pub doc_id: u64,
    pub corpus_type: u8,  // 0=Legal, 1=Contract, 2=RFC, 3=Other
    pub _pad: [u8; 3],
    pub text_offset: u64,
    pub text_len: u32,
}

/// Per-dimension MSE quantization centroids.
/// centroids[dim] = [c0, c1, c2, c3] for 2-bit (4 levels).
pub struct Centroids {
    pub values: Vec<[f32; 4]>,  // len == num_dims (384)
}

/// QJL projection matrix S, stored row-major.
pub struct ProjectionMatrix {
    pub values: Vec<f32>,  // len == rows * cols
    pub rows: usize,
    pub cols: usize,
}

impl ProjectionMatrix {
    /// Returns row `i` as a slice of length `cols`.
    pub fn row(&self, i: usize) -> &[f32] {
        &self.values[i * self.cols..(i + 1) * self.cols]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn turbo_record_size_is_156() {
        assert_eq!(mem::size_of::<TurboRecord>(), 156);
    }

    #[test]
    fn meta_record_size_is_24() {
        assert_eq!(mem::size_of::<MetaRecord>(), 24);
    }

    #[test]
    fn projection_matrix_row() {
        let m = ProjectionMatrix {
            values: vec![1.0, 2.0, 3.0, 4.0],
            rows: 2,
            cols: 2,
        };
        assert_eq!(m.row(0), &[1.0f32, 2.0]);
        assert_eq!(m.row(1), &[3.0f32, 4.0]);
    }
}
