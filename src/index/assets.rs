use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use std::fmt;

const ASSET_HEADER_SIZE: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct CentroidTable {
    dim: u32,
    centroids_per_dim: u32,
    values: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionMatrix {
    input_dim: u32,
    output_dim: u32,
    values: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetError {
    InvalidSize {
        minimum: usize,
        actual: usize,
    },
    InvalidLayout {
        expected_values: usize,
        actual_values: usize,
    },
    DimensionMismatch {
        expected: usize,
        actual: usize,
    },
    InvalidDim,
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize { minimum, actual } => {
                write!(
                    f,
                    "asset size mismatch: expected at least {minimum} bytes, got {actual}"
                )
            }
            Self::InvalidLayout {
                expected_values,
                actual_values,
            } => {
                write!(
                    f,
                    "asset value count mismatch: expected {expected_values}, got {actual_values}"
                )
            }
            Self::DimensionMismatch { expected, actual } => {
                write!(f, "dimension mismatch: expected {expected}, got {actual}")
            }
            Self::InvalidDim => write!(f, "dimensions must be positive"),
        }
    }
}

impl std::error::Error for AssetError {}

impl CentroidTable {
    pub fn generate(dim: u32, centroids_per_dim: u32, seed: u64) -> Self {
        assert!(dim > 0, "dimensions must be positive");
        assert!(centroids_per_dim > 0, "dimensions must be positive");

        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let len = dim as usize * centroids_per_dim as usize;
        let mut values = Vec::with_capacity(len);
        for _ in 0..len {
            values.push(rng.gen_range(-1.0f32..=1.0f32));
        }

        Self {
            dim,
            centroids_per_dim,
            values,
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AssetError> {
        if bytes.len() < ASSET_HEADER_SIZE {
            return Err(AssetError::InvalidSize {
                minimum: ASSET_HEADER_SIZE,
                actual: bytes.len(),
            });
        }
        if bytes.len() == ASSET_HEADER_SIZE {
            return Err(AssetError::InvalidSize {
                minimum: ASSET_HEADER_SIZE + 4,
                actual: bytes.len(),
            });
        }

        let dim = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let centroids_per_dim = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let values = parse_values(bytes, dim, centroids_per_dim)?;

        Ok(Self {
            dim,
            centroids_per_dim,
            values,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ASSET_HEADER_SIZE + self.values.len() * 4);
        out.extend_from_slice(&self.dim.to_le_bytes());
        out.extend_from_slice(&self.centroids_per_dim.to_le_bytes());
        write_values(&mut out, &self.values);
        out
    }

    pub fn dim(&self) -> u32 {
        self.dim
    }

    pub fn centroids_per_dim(&self) -> u32 {
        self.centroids_per_dim
    }

    pub fn values(&self) -> &[f32] {
        &self.values
    }
}

impl ProjectionMatrix {
    pub fn generate(input_dim: u32, output_dim: u32, seed: u64) -> Self {
        assert!(input_dim > 0, "dimensions must be positive");
        assert!(output_dim > 0, "dimensions must be positive");

        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let len = output_dim as usize * input_dim as usize;
        let mut values = Vec::with_capacity(len);
        for _ in 0..len {
            values.push(rng.gen_range(-1.0f32..=1.0f32));
        }

        Self {
            input_dim,
            output_dim,
            values,
        }
    }

    pub fn from_rows(rows: Vec<Vec<f32>>) -> Self {
        assert!(!rows.is_empty(), "dimensions must be positive");
        let input_dim = rows[0].len();
        assert!(input_dim > 0, "dimensions must be positive");
        assert!(
            rows.iter().all(|row| row.len() == input_dim),
            "rows must have equal length"
        );

        let output_dim = rows.len() as u32;
        let mut values = Vec::with_capacity(rows.len() * input_dim);
        for row in rows {
            values.extend(row);
        }

        Self {
            input_dim: input_dim as u32,
            output_dim,
            values,
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AssetError> {
        if bytes.len() < ASSET_HEADER_SIZE {
            return Err(AssetError::InvalidSize {
                minimum: ASSET_HEADER_SIZE,
                actual: bytes.len(),
            });
        }
        if bytes.len() == ASSET_HEADER_SIZE {
            return Err(AssetError::InvalidSize {
                minimum: ASSET_HEADER_SIZE + 4,
                actual: bytes.len(),
            });
        }

        let input_dim = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let output_dim = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let values = parse_values(bytes, output_dim, input_dim)?;

        Ok(Self {
            input_dim,
            output_dim,
            values,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ASSET_HEADER_SIZE + self.values.len() * 4);
        out.extend_from_slice(&self.input_dim.to_le_bytes());
        out.extend_from_slice(&self.output_dim.to_le_bytes());
        write_values(&mut out, &self.values);
        out
    }

    pub fn project(&self, input: &[f32]) -> Vec<f32> {
        self.project_checked(input)
            .expect("projection input dimension must match matrix")
    }

    pub fn project_checked(&self, input: &[f32]) -> Result<Vec<f32>, AssetError> {
        if input.len() != self.input_dim as usize {
            return Err(AssetError::DimensionMismatch {
                expected: self.input_dim as usize,
                actual: input.len(),
            });
        }

        let mut out = vec![0.0; self.output_dim as usize];
        for (row_index, slot) in out.iter_mut().enumerate() {
            let start = row_index * self.input_dim as usize;
            let row = &self.values[start..start + self.input_dim as usize];
            *slot = row
                .iter()
                .zip(input.iter())
                .map(|(lhs, rhs)| lhs * rhs)
                .sum();
        }

        Ok(out)
    }

    pub fn input_dim(&self) -> u32 {
        self.input_dim
    }

    pub fn output_dim(&self) -> u32 {
        self.output_dim
    }

    pub fn values(&self) -> &[f32] {
        &self.values
    }
}

fn parse_values(bytes: &[u8], first_dim: u32, second_dim: u32) -> Result<Vec<f32>, AssetError> {
    if first_dim == 0 || second_dim == 0 {
        return Err(AssetError::InvalidDim);
    }

    let data = &bytes[ASSET_HEADER_SIZE..];
    if !data.len().is_multiple_of(4) {
        return Err(AssetError::InvalidLayout {
            expected_values: first_dim as usize * second_dim as usize,
            actual_values: data.len() / 4,
        });
    }

    let actual_values = data.len() / 4;
    let expected_values = first_dim as usize * second_dim as usize;
    if actual_values != expected_values {
        return Err(AssetError::InvalidLayout {
            expected_values,
            actual_values,
        });
    }

    Ok(data
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect())
}

fn write_values(out: &mut Vec<u8>, values: &[f32]) {
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
}
