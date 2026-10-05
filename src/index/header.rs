use std::fmt;

use super::codec_config::{TurboCodecId, CODEC_FINGERPRINT_LEN};
use super::record::{TurboProdRecord512, TurboRecord512};

pub const TURBO_MAGIC: [u8; 4] = *b"TQNT";
pub const TURBO_VERSION_V2: u32 = 2;
pub const TURBO_VERSION_V3: u32 = 3;
pub const TURBO_VERSION_V4: u32 = 4;

/// The 32-byte header of `turbo_static.bin`. All integers are little-endian.
///
/// | offset | size | field                                              |
/// |--------|------|----------------------------------------------------|
/// | 0      | 4    | magic `TQNT`                                       |
/// | 4      | 4    | `version` (u32)                                    |
/// | 8      | 4    | `dim` (u32)                                        |
/// | 12     | 8    | `record_count` (u64)                               |
/// | 20     | 4    | v4: [`TurboCodecId::code`] (u32). v2/v3: zero      |
/// | 24     | 8    | v4: the codec fingerprint. v2/v3: zero             |
///
/// v2 and v3 readers never looked at bytes 20..32, so this one doesn't
/// either for those versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurboHeader {
    version: u32,
    dim: u32,
    record_count: u64,
    codec: Option<HeaderCodec>,
}

/// What a v4 header says about the codec its records were encoded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderCodec {
    pub codec_id: TurboCodecId,
    /// [`TurboQuantConfig::fingerprint`](super::TurboQuantConfig::fingerprint)
    /// of the builder's config and asset files.
    pub fingerprint: [u8; CODEC_FINGERPRINT_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurboHeaderError {
    InvalidSize {
        expected: usize,
        actual: usize,
    },
    InvalidMagic {
        actual: [u8; 4],
    },
    InvalidDim,
    UnsupportedVersion {
        version: u32,
    },
    UnsupportedLayout {
        version: u32,
        dim: u32,
    },
    /// `record_count` records don't fit in a `u64` file size.
    RecordCountOverflow {
        record_count: u64,
    },
    /// A v4 header's codec code isn't a [`TurboCodecId`] this build knows.
    UnknownCodec {
        code: u32,
    },
    /// The header names a codec whose records this version doesn't hold.
    UnsupportedCodec {
        version: u32,
        codec_id: TurboCodecId,
    },
}

/// The record layouts this binary can read. A layout names the Rust type the
/// record region is cast to, so the type's size is the only record size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownRecordLayout {
    V2Dim512,
    V3Dim512,
    V4Dim512,
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
                write!(
                    f,
                    "unsupported version: {version} (this build reads versions \
                     {TURBO_VERSION_V2} to {TURBO_VERSION_V4}"
                )?;
                // The static pointer is shared by every reader, so a release
                // in a newer format must not be activated before they can
                // all read it.
                if *version > TURBO_VERSION_V4 {
                    write!(
                        f,
                        "; deploy a build that reads version {version} before activating \
                         this release"
                    )?;
                }
                write!(f, ")")
            }
            Self::UnsupportedLayout { version, dim } => {
                write!(
                    f,
                    "unsupported turbo record layout: version={version}, dim={dim}"
                )
            }
            Self::RecordCountOverflow { record_count } => {
                write!(f, "record count {record_count} overflows the file size")
            }
            Self::UnknownCodec { code } => write!(f, "unknown codec code: {code}"),
            Self::UnsupportedCodec { version, codec_id } => {
                write!(f, "version {version} does not hold {codec_id} records")
            }
        }
    }
}

impl KnownRecordLayout {
    pub fn from_header(header: &TurboHeader) -> Result<Self, TurboHeaderError> {
        match (header.version(), header.dim()) {
            (TURBO_VERSION_V2, 512) => Ok(Self::V2Dim512),
            (TURBO_VERSION_V3, 512) => Ok(Self::V3Dim512),
            (TURBO_VERSION_V4, 512) => match header.codec_id() {
                Some(TurboCodecId::TurboQuantProdV1) => Ok(Self::V4Dim512),
                Some(codec_id) => Err(TurboHeaderError::UnsupportedCodec {
                    version: TURBO_VERSION_V4,
                    codec_id,
                }),
                // `TurboHeader` only holds a v4 header with a codec.
                None => Err(TurboHeaderError::UnknownCodec { code: 0 }),
            },
            (version, dim) => Err(TurboHeaderError::UnsupportedLayout { version, dim }),
        }
    }

    pub const fn record_size(self) -> usize {
        match self {
            Self::V2Dim512 | Self::V3Dim512 => std::mem::size_of::<TurboRecord512>(),
            Self::V4Dim512 => std::mem::size_of::<TurboProdRecord512>(),
        }
    }
}

impl std::error::Error for TurboHeaderError {}

impl TurboHeader {
    pub const SIZE: usize = 32;

    pub fn new(dim: u32, record_count: u64) -> Self {
        assert!(dim > 0, "dim must be positive");
        Self {
            version: TURBO_VERSION_V2,
            dim,
            record_count,
            codec: None,
        }
    }

    pub fn new_v3(dim: u32, record_count: u64) -> Self {
        assert!(dim > 0, "dim must be positive");
        Self {
            version: TURBO_VERSION_V3,
            dim,
            record_count,
            codec: None,
        }
    }

    pub fn new_v4(dim: u32, record_count: u64, codec: HeaderCodec) -> Self {
        assert!(dim > 0, "dim must be positive");
        Self {
            version: TURBO_VERSION_V4,
            dim,
            record_count,
            codec: Some(codec),
        }
    }

    pub fn magic(&self) -> [u8; 4] {
        TURBO_MAGIC
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn dim(&self) -> u32 {
        self.dim
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// The codec a v4 header names. `None` for v2 and v3, whose only codec
    /// is `Legacy3BitV1`.
    pub fn codec_id(&self) -> Option<TurboCodecId> {
        self.codec.map(|codec| codec.codec_id)
    }

    /// The codec fingerprint a v4 header stores; `None` for v2 and v3.
    pub fn codec_fingerprint(&self) -> Option<[u8; CODEC_FINGERPRINT_LEN]> {
        self.codec.map(|codec| codec.fingerprint)
    }

    /// The size of a `turbo_static.bin` with this header: the header plus
    /// `record_count` records of the header's [`KnownRecordLayout`]. Fails for
    /// a header with no known layout, or a `record_count` too large for a
    /// `u64` size.
    pub fn expected_file_size(&self) -> Result<u64, TurboHeaderError> {
        let layout = KnownRecordLayout::from_header(self)?;
        self.record_count
            .checked_mul(layout.record_size() as u64)
            .and_then(|records| records.checked_add(Self::SIZE as u64))
            .ok_or(TurboHeaderError::RecordCountOverflow {
                record_count: self.record_count,
            })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&TURBO_MAGIC);
        buf[4..8].copy_from_slice(&self.version.to_le_bytes());
        buf[8..12].copy_from_slice(&self.dim.to_le_bytes());
        buf[12..20].copy_from_slice(&self.record_count.to_le_bytes());
        if let Some(codec) = self.codec {
            buf[20..24].copy_from_slice(&codec.codec_id.code().to_le_bytes());
            buf[24..32].copy_from_slice(&codec.fingerprint);
        }
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
        if !matches!(
            version,
            TURBO_VERSION_V2 | TURBO_VERSION_V3 | TURBO_VERSION_V4
        ) {
            return Err(TurboHeaderError::UnsupportedVersion { version });
        }

        let dim = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if dim == 0 {
            return Err(TurboHeaderError::InvalidDim);
        }

        let record_count = u64::from_le_bytes(buf[12..20].try_into().unwrap());

        let codec = if version == TURBO_VERSION_V4 {
            let code = u32::from_le_bytes(buf[20..24].try_into().unwrap());
            let codec_id =
                TurboCodecId::from_code(code).ok_or(TurboHeaderError::UnknownCodec { code })?;
            Some(HeaderCodec {
                codec_id,
                fingerprint: buf[24..32].try_into().unwrap(),
            })
        } else {
            None
        };

        Ok(Self {
            version,
            dim,
            record_count,
            codec,
        })
    }
}
