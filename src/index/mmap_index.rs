use std::fmt;
use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use super::header::{TurboHeader, TurboHeaderError};
use super::meta::{MetaRecord, META_RECORD_SIZE};
use super::record::TurboRecordRef;

#[derive(Debug)]
pub struct MmapIndex {
    header: TurboHeader,
    bin_mmap: Mmap,
    meta_mmap: Mmap,
    text_mmap: Mmap,
}

#[derive(Debug)]
pub enum MmapIndexError {
    Io {
        path: String,
        source: std::io::Error,
    },
    Header(TurboHeaderError),
    FileSizeMismatch {
        file: &'static str,
        expected: u64,
        actual: u64,
    },
    MetaCountMismatch {
        expected: u64,
        actual: u64,
    },
}

impl fmt::Display for MmapIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to open {path}: {source}"),
            Self::Header(err) => write!(f, "invalid header: {err}"),
            Self::FileSizeMismatch {
                file,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "{file} size mismatch: expected {expected} bytes, got {actual}"
                )
            }
            Self::MetaCountMismatch { expected, actual } => {
                write!(
                    f,
                    "meta record count mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for MmapIndexError {}

impl From<TurboHeaderError> for MmapIndexError {
    fn from(err: TurboHeaderError) -> Self {
        Self::Header(err)
    }
}

impl MmapIndex {
    pub fn load(dir: &Path) -> Result<Self, MmapIndexError> {
        let bin_path = dir.join("turbo_static.bin");
        let meta_path = dir.join("turbo_static_meta.bin");
        let text_path = dir.join("turbo_static_text.bin");

        let bin_mmap = mmap_file(&bin_path)?;
        let meta_mmap = mmap_file(&meta_path)?;
        let text_mmap = mmap_file(&text_path)?;

        if bin_mmap.len() < TurboHeader::SIZE {
            return Err(MmapIndexError::FileSizeMismatch {
                file: "turbo_static.bin",
                expected: TurboHeader::SIZE as u64,
                actual: bin_mmap.len() as u64,
            });
        }

        let header = TurboHeader::from_bytes(&bin_mmap[..TurboHeader::SIZE])?;

        let expected_bin_size = header.expected_file_size();
        if bin_mmap.len() as u64 != expected_bin_size {
            return Err(MmapIndexError::FileSizeMismatch {
                file: "turbo_static.bin",
                expected: expected_bin_size,
                actual: bin_mmap.len() as u64,
            });
        }

        if meta_mmap.len() % META_RECORD_SIZE != 0 {
            return Err(MmapIndexError::MetaCountMismatch {
                expected: header.record_count(),
                actual: meta_mmap.len() as u64 / META_RECORD_SIZE as u64,
            });
        }

        let actual_meta_count = meta_mmap.len() as u64 / META_RECORD_SIZE as u64;
        if actual_meta_count != header.record_count() {
            return Err(MmapIndexError::MetaCountMismatch {
                expected: header.record_count(),
                actual: actual_meta_count,
            });
        }

        Ok(Self {
            header,
            bin_mmap,
            meta_mmap,
            text_mmap,
        })
    }

    pub fn dim(&self) -> u32 {
        self.header.dim()
    }

    pub fn record_count(&self) -> u64 {
        self.header.record_count()
    }

    pub fn header(&self) -> &TurboHeader {
        &self.header
    }

    pub fn record(&self, index: u64) -> TurboRecordRef<'_> {
        assert!(
            index < self.header.record_count(),
            "record index {index} out of bounds (count={})",
            self.header.record_count()
        );

        let stride = self.header.record_stride();
        let offset = TurboHeader::SIZE + index as usize * stride;
        TurboRecordRef::new(&self.bin_mmap[offset..offset + stride], &self.header)
    }

    pub fn meta(&self, index: u64) -> &MetaRecord {
        assert!(
            index < self.header.record_count(),
            "meta index {index} out of bounds (count={})",
            self.header.record_count()
        );

        let offset = index as usize * META_RECORD_SIZE;
        unsafe { &*(self.meta_mmap[offset..].as_ptr() as *const MetaRecord) }
    }

    pub fn text(&self, index: u64) -> &str {
        let meta = self.meta(index);
        meta.text_from_blob(&self.text_mmap)
    }

    pub fn record_data(&self) -> &[u8] {
        &self.bin_mmap[TurboHeader::SIZE..]
    }

    pub fn text_blob(&self) -> &[u8] {
        &self.text_mmap
    }
}

fn mmap_file(path: &Path) -> Result<Mmap, MmapIndexError> {
    let file = File::open(path).map_err(|source| MmapIndexError::Io {
        path: path.display().to_string(),
        source,
    })?;

    unsafe {
        Mmap::map(&file).map_err(|source| MmapIndexError::Io {
            path: path.display().to_string(),
            source,
        })
    }
}
