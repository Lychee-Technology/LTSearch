use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use memmap2::Mmap;

use super::assets::{AssetError, CentroidTable, ProjectionMatrix};
use super::header::{KnownRecordLayout, TurboHeader, TurboHeaderError};
use super::meta::{MetaRecord, META_RECORD_SIZE};
use super::record::{TurboRecord512, TurboRecordRef, TurboRecordSlice};

const IMAGE_STATIC_DIR: &str = "/app/static";
static MMAP_INDEX: OnceLock<Result<MmapIndex, String>> = OnceLock::new();

#[derive(Debug)]
pub struct MmapIndex {
    header: TurboHeader,
    layout: KnownRecordLayout,
    bin_mmap: Mmap,
    meta_mmap: Mmap,
    text_mmap: Mmap,
    centroids: CentroidTable,
    projection: ProjectionMatrix,
}

#[derive(Debug)]
pub enum MmapIndexError {
    Io {
        path: String,
        source: std::io::Error,
    },
    Header(TurboHeaderError),
    Asset {
        file: &'static str,
        source: AssetError,
    },
    FileSizeMismatch {
        file: &'static str,
        expected: u64,
        actual: u64,
    },
    MetaCountMismatch {
        expected: u64,
        actual: u64,
    },
    AssetDimensionMismatch {
        file: &'static str,
        expected: u32,
        actual: u32,
    },
}

impl fmt::Display for MmapIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to open {path}: {source}"),
            Self::Header(err) => write!(f, "invalid header: {err}"),
            Self::Asset { file, source } => write!(f, "invalid {file}: {source}"),
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
            Self::AssetDimensionMismatch {
                file,
                expected,
                actual,
            } => write!(
                f,
                "{file} dimension mismatch: expected {expected}, got {actual}"
            ),
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
        let centroids_path = dir.join("centroids.bin");
        let projection_path = dir.join("projection.bin");

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
        let layout = KnownRecordLayout::from_header(&header)?;

        let expected_bin_size =
            TurboHeader::SIZE as u64 + header.record_count() * layout.record_size() as u64;
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

        let centroids_mmap = mmap_file(&centroids_path)?;
        let projection_mmap = mmap_file(&projection_path)?;

        let centroids =
            CentroidTable::from_bytes(&centroids_mmap).map_err(|source| MmapIndexError::Asset {
                file: "centroids.bin",
                source,
            })?;
        if centroids.dim() != header.dim() {
            return Err(MmapIndexError::AssetDimensionMismatch {
                file: "centroids.bin",
                expected: header.dim(),
                actual: centroids.dim(),
            });
        }

        let projection = ProjectionMatrix::from_bytes(&projection_mmap).map_err(|source| {
            MmapIndexError::Asset {
                file: "projection.bin",
                source,
            }
        })?;
        if projection.input_dim() != header.dim() {
            return Err(MmapIndexError::AssetDimensionMismatch {
                file: "projection.bin",
                expected: header.dim(),
                actual: projection.input_dim(),
            });
        }
        let expected_projection_output = header.dim();
        if projection.output_dim() != expected_projection_output {
            return Err(MmapIndexError::AssetDimensionMismatch {
                file: "projection.bin",
                expected: expected_projection_output,
                actual: projection.output_dim(),
            });
        }

        Ok(Self {
            header,
            layout,
            bin_mmap,
            meta_mmap,
            text_mmap,
            centroids,
            projection,
        })
    }

    pub fn dim(&self) -> u32 {
        self.header.dim()
    }

    pub fn load_from_image() -> Result<Self, MmapIndexError> {
        Self::load(Path::new(IMAGE_STATIC_DIR))
    }

    pub fn global_from_image() -> Result<&'static Self, MmapIndexError> {
        Self::global_from_dir_for_tests(Path::new(IMAGE_STATIC_DIR), &MMAP_INDEX).map_err(
            |message| MmapIndexError::Io {
                path: IMAGE_STATIC_DIR.to_string(),
                source: std::io::Error::other(message),
            },
        )
    }

    pub fn record_count(&self) -> u64 {
        self.header.record_count()
    }

    pub fn header(&self) -> &TurboHeader {
        &self.header
    }

    pub fn layout(&self) -> KnownRecordLayout {
        self.layout
    }

    pub fn centroids(&self) -> &CentroidTable {
        &self.centroids
    }

    pub fn projection(&self) -> &ProjectionMatrix {
        &self.projection
    }

    pub fn record(&self, index: u64) -> TurboRecordRef<'_> {
        assert!(
            index < self.header.record_count(),
            "record index {index} out of bounds (count={})",
            self.header.record_count()
        );

        match self.records() {
            TurboRecordSlice::V1Dim512(records) => {
                TurboRecordRef::from_turbo_record_512(&records[index as usize], &self.header)
            }
        }
    }

    pub fn records(&self) -> TurboRecordSlice<'_> {
        match self.layout {
            KnownRecordLayout::V1Dim512 => {
                let bytes = &self.bin_mmap[TurboHeader::SIZE..];
                let ptr = bytes.as_ptr() as *const TurboRecord512;
                let len = self.header.record_count() as usize;
                let records = unsafe { std::slice::from_raw_parts(ptr, len) };
                TurboRecordSlice::V1Dim512(records)
            }
        }
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

    pub(crate) fn global_from_dir_for_tests<'a>(
        dir: &Path,
        cell: &'a OnceLock<Result<MmapIndex, String>>,
    ) -> Result<&'a MmapIndex, String> {
        let value = cell.get_or_init(|| Self::load(dir).map_err(|err| err.to_string()));
        match value {
            Ok(index) => Ok(index),
            Err(error) => Err(error.clone()),
        }
    }
}

#[cfg(test)]
impl MmapIndex {
    pub(crate) fn load_from_dir_for_tests(dir: &Path) -> Result<Self, MmapIndexError> {
        Self::load(dir)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use crate::index::{CentroidTable, ProjectionMatrix, TurboHeader, TurboRecord512, META_RECORD_SIZE};

    use super::MmapIndex;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ltsearch-mmap-index-unit-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_test_index(dir: &Path) {
        let header = TurboHeader::new(512, 1);
        let mut bin_data = header.to_bytes();
        let record = TurboRecord512 {
            doc_id: 1,
            idx: [0; 128],
            qjl: [0; 64],
            gamma: 0.5,
            _reserved: [0; 4],
        };
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &record as *const TurboRecord512 as *const u8,
                std::mem::size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);
        fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();
        fs::write(dir.join("turbo_static_meta.bin"), vec![0u8; META_RECORD_SIZE]).unwrap();
        fs::write(dir.join("turbo_static_text.bin"), []).unwrap();
        fs::write(
            dir.join("centroids.bin"),
            CentroidTable::generate(512, 16, 7).to_bytes(),
        )
        .unwrap();
        fs::write(
            dir.join("projection.bin"),
            ProjectionMatrix::generate(512, 512, 11).to_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn load_from_image_dir_returns_index() {
        let dir = temp_dir("load-from-dir");
        write_test_index(&dir);

        let index = MmapIndex::load_from_dir_for_tests(&dir).unwrap();
        assert_eq!(index.record_count(), 1);
        assert_eq!(index.dim(), 512);
    }

    #[test]
    fn global_from_dir_returns_same_instance() {
        static TEST_INDEX: OnceLock<Result<MmapIndex, String>> = OnceLock::new();

        let dir = temp_dir("global-from-dir");
        write_test_index(&dir);

        let first = MmapIndex::global_from_dir_for_tests(&dir, &TEST_INDEX).unwrap() as *const MmapIndex;
        let second = MmapIndex::global_from_dir_for_tests(&dir, &TEST_INDEX).unwrap() as *const MmapIndex;
        assert_eq!(first, second);
    }
}
