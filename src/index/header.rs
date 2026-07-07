use std::fmt;

use super::record::TurboRecord512;

pub const TURBO_MAGIC: [u8; 4] = *b"TQNT";
const TURBO_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurboHeader {
    dim: u32,
    record_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurboHeaderError {
    InvalidSize { expected: usize, actual: usize },
    InvalidMagic { actual: [u8; 4] },
    InvalidDim,
    UnsupportedVersion { version: u32 },
    UnsupportedLayout { version: u32, dim: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownRecordLayout {
    V2Dim512,
}

impl fmt::Display for TurboHeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize { expected, actual } => {
                write!(f, "header size mismatch: expected {expected}, got {actual}")
            }
            Self::InvalidMagic { actual } => {
                write!(f, "invalid magic bytes: {actual:?}")
            }
            Self::InvalidDim => write!(f, "dim must be positive"),
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported version: {version}")
            }
            Self::UnsupportedLayout { version, dim } => {
                write!(
                    f,
                    "unsupported turbo record layout: version={version}, dim={dim}"
                )
            }
        }
    }
}

impl KnownRecordLayout {
    pub fn from_header(header: &TurboHeader) -> Result<Self, TurboHeaderError> {
        match (header.version(), header.dim()) {
            (TURBO_VERSION, 512) => Ok(Self::V2Dim512),
            (version, dim) => Err(TurboHeaderError::UnsupportedLayout { version, dim }),
        }
    }

    pub fn record_size(&self) -> usize {
        match self {
            Self::V2Dim512 => std::mem::size_of::<TurboRecord512>(),
        }
    }
}

impl std::error::Error for TurboHeaderError {}

impl TurboHeader {
    pub const SIZE: usize = 32;

    pub fn new(dim: u32, record_count: u64) -> Self {
        assert!(dim > 0, "dim must be positive");
        Self { dim, record_count }
    }

    pub fn magic(&self) -> [u8; 4] {
        TURBO_MAGIC
    }

    pub fn version(&self) -> u32 {
        TURBO_VERSION
    }

    pub fn dim(&self) -> u32 {
        self.dim
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    pub fn idx_size(&self) -> usize {
        (self.dim as usize * 2).div_ceil(8)
    }

    pub fn qjl_size(&self) -> usize {
        (self.dim as usize).div_ceil(8)
    }

    pub fn record_stride(&self) -> usize {
        8 + self.idx_size() + self.qjl_size() + 4
    }

    pub fn expected_file_size(&self) -> u64 {
        Self::SIZE as u64 + self.record_count * self.record_stride() as u64
    }

    pub fn idx_offset(&self) -> usize {
        8
    }

    pub fn qjl_offset(&self) -> usize {
        8 + self.idx_size()
    }

    pub fn gamma_offset(&self) -> usize {
        8 + self.idx_size() + self.qjl_size()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&TURBO_MAGIC);
        buf[4..8].copy_from_slice(&TURBO_VERSION.to_le_bytes());
        buf[8..12].copy_from_slice(&self.dim.to_le_bytes());
        buf[12..20].copy_from_slice(&self.record_count.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, TurboHeaderError> {
        if buf.len() < Self::SIZE {
            return Err(TurboHeaderError::InvalidSize {
                expected: Self::SIZE,
                actual: buf.len(),
            });
        }

        let mut magic = [0u8; 4];
        magic.copy_from_slice(&buf[0..4]);
        if magic != TURBO_MAGIC {
            return Err(TurboHeaderError::InvalidMagic { actual: magic });
        }

        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if version != TURBO_VERSION {
            return Err(TurboHeaderError::UnsupportedVersion { version });
        }

        let dim = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if dim == 0 {
            return Err(TurboHeaderError::InvalidDim);
        }

        let record_count = u64::from_le_bytes(buf[12..20].try_into().unwrap());

        Ok(Self { dim, record_count })
    }
}
