use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use memmap2::Mmap;

use super::assets::{AssetError, CentroidTable, ProjectionMatrix};
use super::header::{KnownRecordLayout, TurboHeader, TurboHeaderError, TURBO_VERSION_V3};
use super::meta::{MetaRecord, META_RECORD_SIZE};
use super::meta_ext::{MetaExtRecord, META_EXT_RECORD_SIZE};
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
    title_mmap: Mmap,
    // v3-only sidecars carrying the original string doc_id and canonicalized
    // metadata JSON. `None` for v2 images, which have no such files.
    meta_ext_mmap: Option<Mmap>,
    docid_mmap: Option<Mmap>,
    meta_json_mmap: Option<Mmap>,
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
    MetaExtCountMismatch {
        expected: u64,
        actual: u64,
    },
    MetaExtBlobOutOfBounds {
        index: u64,
        blob: &'static str,
    },
    MetaExtBlobInvalidUtf8 {
        index: u64,
        blob: &'static str,
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
            Self::MetaExtCountMismatch { expected, actual } => {
                write!(
                    f,
                    "meta ext record count mismatch: expected {expected}, got {actual}"
                )
            }
            Self::MetaExtBlobOutOfBounds { index, blob } => {
                write!(f, "meta ext {blob} blob out of bounds at record {index}")
            }
            Self::MetaExtBlobInvalidUtf8 { index, blob } => {
                write!(
                    f,
                    "meta ext {blob} blob contains invalid UTF-8 at record {index}"
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
        let title_path = dir.join("turbo_static_title.bin");
        let centroids_path = dir.join("centroids.bin");
        let projection_path = dir.join("projection.bin");

        // Parse the header (and reject unsupported versions/layouts) before
        // touching the other blobs, so a legacy v1 image — which has no
        // `turbo_static_title.bin` — fails through `TurboHeader::from_bytes`
        // with `UnsupportedVersion`, not an I/O error on the missing title file.
        let bin_mmap = mmap_file(&bin_path)?;
        if bin_mmap.len() < TurboHeader::SIZE {
            return Err(MmapIndexError::FileSizeMismatch {
                file: "turbo_static.bin",
                expected: TurboHeader::SIZE as u64,
                actual: bin_mmap.len() as u64,
            });
        }

        let header = TurboHeader::from_bytes(&bin_mmap[..TurboHeader::SIZE])?;
        let layout = KnownRecordLayout::from_header(&header)?;

        let meta_mmap = mmap_file(&meta_path)?;
        let text_mmap = mmap_file(&text_path)?;
        let title_mmap = mmap_file(&title_path)?;

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

        // v3 images ship three additional sidecars carrying the original string
        // doc_id and canonicalized metadata JSON. v2 images have none of these
        // files, so the branch keeps the legacy load path byte-for-byte.
        let (meta_ext_mmap, docid_mmap, meta_json_mmap) = if header.version() == TURBO_VERSION_V3 {
            let meta_ext_path = dir.join("turbo_static_meta_ext.bin");
            let docid_path = dir.join("turbo_static_docid.bin");
            let meta_json_path = dir.join("turbo_static_meta_json.bin");

            let meta_ext_mmap = mmap_file(&meta_ext_path)?;
            let docid_mmap = mmap_file(&docid_path)?;
            let meta_json_mmap = mmap_file(&meta_json_path)?;

            if meta_ext_mmap.len() % META_EXT_RECORD_SIZE != 0 {
                return Err(MmapIndexError::MetaExtCountMismatch {
                    expected: header.record_count(),
                    actual: meta_ext_mmap.len() as u64 / META_EXT_RECORD_SIZE as u64,
                });
            }

            let actual_meta_ext_count = meta_ext_mmap.len() as u64 / META_EXT_RECORD_SIZE as u64;
            if actual_meta_ext_count != header.record_count() {
                return Err(MmapIndexError::MetaExtCountMismatch {
                    expected: header.record_count(),
                    actual: actual_meta_ext_count,
                });
            }

            // Validate every record's blob slice at load time so the accessors
            // (`original_doc_id` / `metadata_json`) can index the sidecars
            // without bounds or UTF-8 panics. A corrupt sidecar fails here
            // instead of deep inside a query path.
            let docid_blob_len = docid_mmap.len();
            let meta_json_blob_len = meta_json_mmap.len();
            for i in 0..actual_meta_ext_count as usize {
                let offset = i * META_EXT_RECORD_SIZE;
                // Safety: the sidecar length was validated above to be exactly
                // `record_count * META_EXT_RECORD_SIZE`, and `offset` is a
                // multiple of 8, matching `MetaExtRecord`'s alignment.
                let ext = unsafe { &*(meta_ext_mmap[offset..].as_ptr() as *const MetaExtRecord) };

                validate_ext_blob(
                    &docid_mmap,
                    docid_blob_len,
                    ext.docid_offset,
                    ext.docid_len,
                    i as u64,
                    "docid",
                )?;
                validate_ext_blob(
                    &meta_json_mmap,
                    meta_json_blob_len,
                    ext.meta_json_offset,
                    ext.meta_json_len,
                    i as u64,
                    "meta_json",
                )?;
            }

            (Some(meta_ext_mmap), Some(docid_mmap), Some(meta_json_mmap))
        } else {
            (None, None, None)
        };

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
            title_mmap,
            meta_ext_mmap,
            docid_mmap,
            meta_json_mmap,
            centroids,
            projection,
        })
    }

    pub fn dim(&self) -> u32 {
        self.header.dim()
    }

    pub fn version(&self) -> u32 {
        self.header.version()
    }

    /// The original string doc_id for record `i`, or `None` for v2 images (which
    /// carry no doc_id sidecar) or an out-of-range index.
    pub fn original_doc_id(&self, i: usize) -> Option<&str> {
        let ext = self.meta_ext_record(i)?;
        let blob = self.docid_mmap.as_ref()?;
        Some(ext.doc_id_from_blob(blob))
    }

    /// The canonicalized metadata JSON for record `i`, or `None` for v2 images
    /// (which carry no metadata sidecar) or an out-of-range index.
    pub fn metadata_json(&self, i: usize) -> Option<&str> {
        let ext = self.meta_ext_record(i)?;
        let blob = self.meta_json_mmap.as_ref()?;
        Some(ext.metadata_json_from_blob(blob))
    }

    fn meta_ext_record(&self, i: usize) -> Option<&MetaExtRecord> {
        let mmap = self.meta_ext_mmap.as_ref()?;
        if i >= self.header.record_count() as usize {
            return None;
        }
        let offset = i * META_EXT_RECORD_SIZE;
        // Safety: the sidecar length was validated in `load` to be exactly
        // `record_count * META_EXT_RECORD_SIZE`, and `MetaExtRecord` is
        // `#[repr(C)]` with no alignment above the `u64`-aligned mmap base.
        Some(unsafe { &*(mmap[offset..].as_ptr() as *const MetaExtRecord) })
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
            TurboRecordSlice::V2Dim512(records) => {
                TurboRecordRef::from_turbo_record_512(&records[index as usize], &self.header)
            }
        }
    }

    pub fn records(&self) -> TurboRecordSlice<'_> {
        match self.layout {
            KnownRecordLayout::V2Dim512 | KnownRecordLayout::V3Dim512 => {
                let bytes = &self.bin_mmap[TurboHeader::SIZE..];
                let ptr = bytes.as_ptr() as *const TurboRecord512;
                let len = self.header.record_count() as usize;
                let records = unsafe { std::slice::from_raw_parts(ptr, len) };
                TurboRecordSlice::V2Dim512(records)
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

    pub fn title(&self, index: u64) -> Option<&str> {
        let meta = self.meta(index);
        meta.title_from_blob(&self.title_mmap)
    }

    pub fn record_data(&self) -> &[u8] {
        &self.bin_mmap[TurboHeader::SIZE..]
    }

    pub fn text_blob(&self) -> &[u8] {
        &self.text_mmap
    }

    pub fn title_blob(&self) -> &[u8] {
        &self.title_mmap
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

/// Verifies that `offset + len` stays within `blob` and that the resulting
/// slice is valid UTF-8, mapping failures to the two blob error variants.
fn validate_ext_blob(
    blob: &[u8],
    blob_len: usize,
    offset: u64,
    len: u32,
    index: u64,
    blob_name: &'static str,
) -> Result<(), MmapIndexError> {
    let start = offset as usize;
    let end = start
        .checked_add(len as usize)
        .filter(|end| *end <= blob_len)
        .ok_or(MmapIndexError::MetaExtBlobOutOfBounds {
            index,
            blob: blob_name,
        })?;
    if std::str::from_utf8(&blob[start..end]).is_err() {
        return Err(MmapIndexError::MetaExtBlobInvalidUtf8 {
            index,
            blob: blob_name,
        });
    }
    Ok(())
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

    use crate::index::{
        CentroidTable, ProjectionMatrix, TurboHeader, TurboRecord512, META_RECORD_SIZE,
    };

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
        fs::write(
            dir.join("turbo_static_meta.bin"),
            vec![0u8; META_RECORD_SIZE],
        )
        .unwrap();
        fs::write(dir.join("turbo_static_text.bin"), []).unwrap();
        fs::write(dir.join("turbo_static_title.bin"), []).unwrap();
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

        let first =
            MmapIndex::global_from_dir_for_tests(&dir, &TEST_INDEX).unwrap() as *const MmapIndex;
        let second =
            MmapIndex::global_from_dir_for_tests(&dir, &TEST_INDEX).unwrap() as *const MmapIndex;
        assert_eq!(first, second);
    }
}
