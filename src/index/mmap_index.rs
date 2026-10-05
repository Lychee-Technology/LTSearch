use std::fmt;
use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use super::assets::{AssetError, CentroidTable, ProjectionMatrix};
use super::codebook::{LloydMaxCodebook, CODEBOOK_FILE};
use super::codec_config::{NormPolicy, TurboCodecId, TurboQuantConfig, CODEC_FINGERPRINT_LEN};
use super::header::{KnownRecordLayout, TurboHeader, TurboHeaderError};
use super::meta::{MetaRecord, META_RECORD_SIZE};
use super::meta_ext::{MetaExtRecord, META_EXT_RECORD_SIZE};
use super::qjl::{QjlMatrix, QJL_FILE};
use super::record::{TurboProdRecord512, TurboRecord512, TurboRecordRef, TurboRecordSlice};
use super::rotation::{Rotation, ROTATION_FILE};
use super::turbo_prod::{TurboProdError, TurboQuantProdV1};

const TURBO_STATIC_FILE: &str = "turbo_static.bin";
const META_FILE: &str = "turbo_static_meta.bin";
const META_EXT_FILE: &str = "turbo_static_meta_ext.bin";
const CENTROIDS_FILE: &str = "centroids.bin";
const PROJECTION_FILE: &str = "projection.bin";

/// The codec an index's records are scored with, assembled from the asset
/// files stored next to them.
#[derive(Debug)]
pub enum IndexCodec {
    /// v2 and v3: `Legacy3BitV1`.
    Legacy {
        centroids: CentroidTable,
        projection: ProjectionMatrix,
    },
    /// v4.
    Prod(TurboQuantProdV1),
}

#[derive(Debug)]
pub struct MmapIndex {
    header: TurboHeader,
    layout: KnownRecordLayout,
    bin_mmap: Mmap,
    meta_mmap: Mmap,
    text_mmap: Mmap,
    title_mmap: Mmap,
    // Sidecars carrying the original string doc_id and canonicalized metadata
    // JSON. `None` for v2 images, which have no such files.
    meta_ext_mmap: Option<Mmap>,
    docid_mmap: Option<Mmap>,
    meta_json_mmap: Option<Mmap>,
    codec: IndexCodec,
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
    /// Returned by the sidecar accessors and [`MmapIndex::check_sidecar_utf8`],
    /// not by `load`, which checks the blob ranges but not their UTF-8.
    MetaExtBlobInvalidUtf8 {
        index: u64,
        blob: &'static str,
    },
    AssetDimensionMismatch {
        file: &'static str,
        expected: u32,
        actual: u32,
    },
    /// A legacy asset has a well-formed header but a shape the legacy codec
    /// can't score.
    UnsupportedLegacyAsset {
        file: &'static str,
        field: &'static str,
        expected: u32,
        actual: u32,
    },
    /// The mapped bytes don't start at an address the typed records can be
    /// read from.
    MisalignedRecords {
        file: &'static str,
        align: usize,
    },
    /// The v4 asset files parse, but aren't the assets of one
    /// `TurboQuantProdV1` codec for the header's dim.
    Codec(TurboProdError),
    /// The v4 assets are a valid codec whose codes aren't the size of the
    /// record's `idx` and `signs` fields.
    UnsupportedCodeLayout {
        idx_len: usize,
        signs_len: usize,
    },
    /// The asset files aren't the ones the v4 records were encoded with.
    CodecFingerprintMismatch {
        /// From the `turbo_static.bin` header.
        expected: [u8; CODEC_FINGERPRINT_LEN],
        /// Of the asset files found.
        actual: [u8; CODEC_FINGERPRINT_LEN],
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
            Self::UnsupportedLegacyAsset {
                file,
                field,
                expected,
                actual,
            } => write!(
                f,
                "{file} has {field}={actual}, but the legacy codec only scores {field}={expected}"
            ),
            Self::MisalignedRecords { file, align } => write!(
                f,
                "{file} records are not mapped at a {align}-byte aligned address"
            ),
            Self::Codec(err) => write!(f, "invalid codec assets: {err}"),
            Self::UnsupportedCodeLayout { idx_len, signs_len } => write!(
                f,
                "the codec's codes ({idx_len} index bytes, {signs_len} sign bytes) are not the \
                 128 and 64 bytes a v4 record holds"
            ),
            Self::CodecFingerprintMismatch { expected, actual } => write!(
                f,
                "codec fingerprint mismatch: {TURBO_STATIC_FILE} was encoded with {}, the asset \
                 files are {}; the records and the assets are not from the same build",
                hex::encode(expected),
                hex::encode(actual)
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
        let bin_path = dir.join(TURBO_STATIC_FILE);
        let meta_path = dir.join(META_FILE);
        let text_path = dir.join("turbo_static_text.bin");
        let title_path = dir.join("turbo_static_title.bin");

        // Parse the header (and reject unsupported versions/layouts) before
        // touching the other blobs, so a legacy v1 image — which has no
        // `turbo_static_title.bin` — fails through `TurboHeader::from_bytes`
        // with `UnsupportedVersion`, not an I/O error on the missing title file.
        let bin_mmap = mmap_file(&bin_path)?;
        if bin_mmap.len() < TurboHeader::SIZE {
            return Err(MmapIndexError::FileSizeMismatch {
                file: TURBO_STATIC_FILE,
                expected: TurboHeader::SIZE as u64,
                actual: bin_mmap.len() as u64,
            });
        }

        let header = TurboHeader::from_bytes(&bin_mmap[..TurboHeader::SIZE])?;
        let layout = KnownRecordLayout::from_header(&header)?;

        let meta_mmap = mmap_file(&meta_path)?;
        let text_mmap = mmap_file(&text_path)?;
        let title_mmap = mmap_file(&title_path)?;

        let expected_bin_size = header.expected_file_size()?;
        if bin_mmap.len() as u64 != expected_bin_size {
            return Err(MmapIndexError::FileSizeMismatch {
                file: TURBO_STATIC_FILE,
                expected: expected_bin_size,
                actual: bin_mmap.len() as u64,
            });
        }
        match layout {
            KnownRecordLayout::V2Dim512 | KnownRecordLayout::V3Dim512 => {
                ensure_aligned::<TurboRecord512>(TURBO_STATIC_FILE, &bin_mmap[TurboHeader::SIZE..])?
            }
            KnownRecordLayout::V4Dim512 => ensure_aligned::<TurboProdRecord512>(
                TURBO_STATIC_FILE,
                &bin_mmap[TurboHeader::SIZE..],
            )?,
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
        ensure_aligned::<MetaRecord>(META_FILE, &meta_mmap)?;

        // v3 and v4 images ship three additional sidecars carrying the original
        // string doc_id and canonicalized metadata JSON. v2 images have none of
        // these files, so the branch keeps the legacy load path byte-for-byte.
        let has_doc_sidecars = matches!(
            layout,
            KnownRecordLayout::V3Dim512 | KnownRecordLayout::V4Dim512
        );
        let (meta_ext_mmap, docid_mmap, meta_json_mmap) = if has_doc_sidecars {
            let meta_ext_path = dir.join(META_EXT_FILE);
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

            ensure_aligned::<MetaExtRecord>(META_EXT_FILE, &meta_ext_mmap)?;

            // Check every record's blob ranges at load time so the accessors
            // (`original_doc_id` / `metadata_json`) can index the sidecars
            // without an out-of-bounds panic. This reads only the meta_ext
            // records. The accessors check UTF-8 when they read a range:
            // checking it here would read every doc_id and metadata byte at
            // each load, where a query reads only its top-K.
            let docid_blob_len = docid_mmap.len();
            let meta_json_blob_len = meta_json_mmap.len();
            for i in 0..actual_meta_ext_count as usize {
                let offset = i * META_EXT_RECORD_SIZE;
                // Safety: the sidecar length was validated above to be exactly
                // `record_count * META_EXT_RECORD_SIZE`, its start was checked
                // to be aligned for `MetaExtRecord`, and `offset` is a multiple
                // of the record size.
                let ext = unsafe { &*(meta_ext_mmap[offset..].as_ptr() as *const MetaExtRecord) };

                check_ext_blob_bounds(
                    docid_blob_len,
                    ext.docid_offset,
                    ext.docid_len,
                    i as u64,
                    "docid",
                )?;
                check_ext_blob_bounds(
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

        let codec = match layout {
            KnownRecordLayout::V2Dim512 | KnownRecordLayout::V3Dim512 => {
                load_legacy_codec(dir, &header)?
            }
            KnownRecordLayout::V4Dim512 => load_prod_codec(dir, &header)?,
        };

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
            codec,
        })
    }

    pub fn dim(&self) -> u32 {
        self.header.dim()
    }

    pub fn version(&self) -> u32 {
        self.header.version()
    }

    /// The original string doc_id for record `i`, or `None` for v2 images (which
    /// carry no doc_id sidecar) or an out-of-range index. Fails with
    /// [`MmapIndexError::MetaExtBlobInvalidUtf8`] if the stored bytes aren't
    /// UTF-8.
    pub fn original_doc_id(&self, i: usize) -> Result<Option<&str>, MmapIndexError> {
        let (Some(ext), Some(blob)) = (self.meta_ext_record(i), self.docid_mmap.as_ref()) else {
            return Ok(None);
        };
        ext.doc_id_from_blob(blob)
            .map(Some)
            .map_err(|_| MmapIndexError::MetaExtBlobInvalidUtf8 {
                index: i as u64,
                blob: "docid",
            })
    }

    /// The canonicalized metadata JSON for record `i`, or `None` for v2 images
    /// (which carry no metadata sidecar) or an out-of-range index. Fails with
    /// [`MmapIndexError::MetaExtBlobInvalidUtf8`] if the stored bytes aren't
    /// UTF-8.
    pub fn metadata_json(&self, i: usize) -> Result<Option<&str>, MmapIndexError> {
        let (Some(ext), Some(blob)) = (self.meta_ext_record(i), self.meta_json_mmap.as_ref())
        else {
            return Ok(None);
        };
        ext.metadata_json_from_blob(blob).map(Some).map_err(|_| {
            MmapIndexError::MetaExtBlobInvalidUtf8 {
                index: i as u64,
                blob: "meta_json",
            }
        })
    }

    /// Checks that every record's doc_id and metadata JSON are UTF-8, which
    /// `load` leaves to the accessors. This reads every sidecar byte, so it
    /// belongs where the whole release is read anyway, such as verification
    /// before activation, not on a reader's load path.
    pub fn check_sidecar_utf8(&self) -> Result<(), MmapIndexError> {
        for i in 0..self.record_count() as usize {
            self.original_doc_id(i)?;
            self.metadata_json(i)?;
        }
        Ok(())
    }

    fn meta_ext_record(&self, i: usize) -> Option<&MetaExtRecord> {
        let mmap = self.meta_ext_mmap.as_ref()?;
        if i >= self.header.record_count() as usize {
            return None;
        }
        let offset = i * META_EXT_RECORD_SIZE;
        // Safety: `load` validated the sidecar length to be exactly
        // `record_count * META_EXT_RECORD_SIZE` and its start to be aligned
        // for `MetaExtRecord`.
        Some(unsafe { &*(mmap[offset..].as_ptr() as *const MetaExtRecord) })
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

    pub fn codec(&self) -> &IndexCodec {
        &self.codec
    }

    /// Whether the image carries the original doc_id and metadata JSON
    /// sidecars: v3 and v4 do, v2 doesn't.
    pub fn has_doc_sidecars(&self) -> bool {
        self.meta_ext_mmap.is_some()
    }

    pub fn record(&self, index: u64) -> TurboRecordRef<'_> {
        assert!(
            index < self.header.record_count(),
            "record index {index} out of bounds (count={})",
            self.header.record_count()
        );

        let record_size = self.layout.record_size();
        let start = index as usize * record_size;
        TurboRecordRef::new(&self.record_data()[start..start + record_size], self.layout)
    }

    pub fn records(&self) -> TurboRecordSlice<'_> {
        match self.layout {
            KnownRecordLayout::V2Dim512 | KnownRecordLayout::V3Dim512 => {
                let bytes = &self.bin_mmap[TurboHeader::SIZE..];
                let ptr = bytes.as_ptr() as *const TurboRecord512;
                let len = self.header.record_count() as usize;
                // Safety: `load` validated that the region holds exactly
                // `record_count` records and starts at an address aligned for
                // `TurboRecord512`, which has no invalid bit patterns.
                let records = unsafe { std::slice::from_raw_parts(ptr, len) };
                TurboRecordSlice::V2Dim512(records)
            }
            KnownRecordLayout::V4Dim512 => {
                let bytes = &self.bin_mmap[TurboHeader::SIZE..];
                let ptr = bytes.as_ptr() as *const TurboProdRecord512;
                let len = self.header.record_count() as usize;
                // Safety: as above, for `TurboProdRecord512`.
                let records = unsafe { std::slice::from_raw_parts(ptr, len) };
                TurboRecordSlice::V4Dim512(records)
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
        // Safety: `load` validated that the file holds exactly `record_count`
        // records and starts at an address aligned for `MetaRecord`.
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
}

#[cfg(test)]
impl MmapIndex {
    pub(crate) fn load_from_dir_for_tests(dir: &Path) -> Result<Self, MmapIndexError> {
        Self::load(dir)
    }

    pub(crate) fn global_from_dir_for_tests<'a>(
        dir: &Path,
        cell: &'a std::sync::OnceLock<Result<MmapIndex, String>>,
    ) -> Result<&'a MmapIndex, String> {
        let value = cell.get_or_init(|| Self::load(dir).map_err(|err| err.to_string()));
        match value {
            Ok(index) => Ok(index),
            Err(error) => Err(error.clone()),
        }
    }
}

/// Loads the legacy codec's `centroids.bin` and `projection.bin`.
fn load_legacy_codec(dir: &Path, header: &TurboHeader) -> Result<IndexCodec, MmapIndexError> {
    let centroids_mmap = mmap_file(&dir.join(CENTROIDS_FILE))?;
    let projection_mmap = mmap_file(&dir.join(PROJECTION_FILE))?;

    let centroids =
        CentroidTable::from_bytes(&centroids_mmap).map_err(|source| MmapIndexError::Asset {
            file: CENTROIDS_FILE,
            source,
        })?;
    if centroids.dim() != header.dim() {
        return Err(MmapIndexError::AssetDimensionMismatch {
            file: CENTROIDS_FILE,
            expected: header.dim(),
            actual: centroids.dim(),
        });
    }
    // A legacy record stores a 2-bit centroid index per dimension, and the
    // scorer reads exactly that many centroids. A larger table would load
    // and then be scored against the wrong centroids.
    let legacy_centroids_per_dim = TurboQuantConfig::legacy_v1().centroids_per_dim();
    if centroids.centroids_per_dim() != legacy_centroids_per_dim {
        return Err(MmapIndexError::UnsupportedLegacyAsset {
            file: CENTROIDS_FILE,
            field: "centroids_per_dim",
            expected: legacy_centroids_per_dim,
            actual: centroids.centroids_per_dim(),
        });
    }

    let projection =
        ProjectionMatrix::from_bytes(&projection_mmap).map_err(|source| MmapIndexError::Asset {
            file: PROJECTION_FILE,
            source,
        })?;
    if projection.input_dim() != header.dim() {
        return Err(MmapIndexError::AssetDimensionMismatch {
            file: PROJECTION_FILE,
            expected: header.dim(),
            actual: projection.input_dim(),
        });
    }
    let expected_projection_output = header.dim();
    if projection.output_dim() != expected_projection_output {
        return Err(MmapIndexError::AssetDimensionMismatch {
            file: PROJECTION_FILE,
            expected: expected_projection_output,
            actual: projection.output_dim(),
        });
    }

    Ok(IndexCodec::Legacy {
        centroids,
        projection,
    })
}

/// Loads the `TurboQuantProdV1` codec of a v4 image from `rotation.bin`,
/// `codebook.bin` and `qjl.bin`.
///
/// The loader never reads the release manifest: the query side maps a
/// directory of files, and the manifest belongs to the publish path. The
/// codec config is therefore read back from the assets' own headers, and two
/// checks stand in for the manifest. [`TurboQuantProdV1::from_assets`]
/// requires the three assets to agree with each other and with the header's
/// dim. The header's fingerprint then requires them to be, byte for byte,
/// the assets the records were encoded with.
fn load_prod_codec(dir: &Path, header: &TurboHeader) -> Result<IndexCodec, MmapIndexError> {
    let rotation_mmap = mmap_file(&dir.join(ROTATION_FILE))?;
    let codebook_mmap = mmap_file(&dir.join(CODEBOOK_FILE))?;
    let qjl_mmap = mmap_file(&dir.join(QJL_FILE))?;

    let asset_error = |file| move |source| MmapIndexError::Asset { file, source };
    let rotation = Rotation::from_bytes(&rotation_mmap).map_err(asset_error(ROTATION_FILE))?;
    let codebook =
        LloydMaxCodebook::from_bytes(&codebook_mmap).map_err(asset_error(CODEBOOK_FILE))?;
    let qjl = QjlMatrix::from_bytes(&qjl_mmap).map_err(asset_error(QJL_FILE))?;

    let config = TurboQuantConfig {
        codec_id: TurboCodecId::TurboQuantProdV1,
        dim: header.dim(),
        mse_bits: codebook.bits(),
        qjl_dim: qjl.qjl_dim(),
        mse_seed: rotation.seed(),
        qjl_seed: qjl.seed(),
        generator_version: rotation.generator_version(),
        norm_policy: NormPolicy::NormalizeAndStore,
    };
    let codec = TurboQuantProdV1::from_assets(config, rotation, codebook, qjl)
        .map_err(MmapIndexError::Codec)?;
    if !TurboProdRecord512::holds_codes_of(&codec) {
        return Err(MmapIndexError::UnsupportedCodeLayout {
            idx_len: codec.idx_len(),
            signs_len: codec.signs_len(),
        });
    }

    let actual = config.fingerprint(&[
        (ROTATION_FILE, &rotation_mmap),
        (CODEBOOK_FILE, &codebook_mmap),
        (QJL_FILE, &qjl_mmap),
    ]);
    match header.codec_fingerprint() {
        Some(expected) if expected == actual => Ok(IndexCodec::Prod(codec)),
        expected => Err(MmapIndexError::CodecFingerprintMismatch {
            // `TurboHeader` only holds a v4 header with a fingerprint.
            expected: expected.unwrap_or_default(),
            actual,
        }),
    }
}

/// Verifies that `offset + len` stays within a blob of `blob_len` bytes.
fn check_ext_blob_bounds(
    blob_len: usize,
    offset: u64,
    len: u32,
    index: u64,
    blob_name: &'static str,
) -> Result<(), MmapIndexError> {
    // Convert the on-disk `u64` offset to `usize` fallibly: on a 32-bit target a
    // bare `as usize` would truncate, so a forged huge offset could wrap into
    // bounds and pass the check below. `try_from` rejects any offset that does
    // not fit the address space up front. `len` is `u32`, infallible into `usize`
    // on every target we build for, but convert it via `try_from` too for symmetry.
    let start = usize::try_from(offset).map_err(|_| MmapIndexError::MetaExtBlobOutOfBounds {
        index,
        blob: blob_name,
    })?;
    let len = usize::try_from(len).map_err(|_| MmapIndexError::MetaExtBlobOutOfBounds {
        index,
        blob: blob_name,
    })?;
    match start.checked_add(len) {
        Some(end) if end <= blob_len => Ok(()),
        _ => Err(MmapIndexError::MetaExtBlobOutOfBounds {
            index,
            blob: blob_name,
        }),
    }
}

/// Rejects a region that the accessors would cast to `&T` at an address `T`
/// can't be read from. A file mapping starts on a page boundary, so this only
/// fails if a region's offset into its file stops being a multiple of `T`'s
/// alignment.
fn ensure_aligned<T>(file: &'static str, bytes: &[u8]) -> Result<(), MmapIndexError> {
    if bytes.as_ptr().cast::<T>().is_aligned() {
        Ok(())
    } else {
        Err(MmapIndexError::MisalignedRecords {
            file,
            align: std::mem::align_of::<T>(),
        })
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
            CentroidTable::generate(512, 4, 7).to_bytes(),
        )
        .unwrap();
        fs::write(
            dir.join("projection.bin"),
            ProjectionMatrix::generate(512, 512, 11).to_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn load_from_dir_returns_index() {
        let dir = temp_dir("load-from-dir");
        write_test_index(&dir);

        let index = MmapIndex::load_from_dir_for_tests(&dir).unwrap();
        assert_eq!(index.record_count(), 1);
        assert_eq!(index.dim(), 512);
    }

    #[test]
    fn load_rejects_a_centroid_table_the_legacy_record_cannot_index() {
        let dir = temp_dir("sixteen-centroids");
        write_test_index(&dir);
        fs::write(
            dir.join("centroids.bin"),
            CentroidTable::generate(512, 16, 7).to_bytes(),
        )
        .unwrap();

        let err = MmapIndex::load_from_dir_for_tests(&dir).unwrap_err();
        assert!(
            matches!(
                err,
                super::MmapIndexError::UnsupportedLegacyAsset {
                    file: "centroids.bin",
                    field: "centroids_per_dim",
                    expected: 4,
                    actual: 16,
                }
            ),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            "centroids.bin has centroids_per_dim=16, but the legacy codec only scores \
             centroids_per_dim=4"
        );
    }

    #[test]
    fn ensure_aligned_rejects_a_region_at_an_odd_address() {
        // `u64` storage is 8-aligned, so one byte in is misaligned for every
        // record type and eight bytes in is aligned again.
        let storage = [0u64; 64];
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), size_of_val(&storage))
        };

        super::ensure_aligned::<TurboRecord512>("turbo_static.bin", bytes).unwrap();
        super::ensure_aligned::<TurboRecord512>("turbo_static.bin", &bytes[8..]).unwrap();
        let err = super::ensure_aligned::<TurboRecord512>("turbo_static.bin", &bytes[1..])
            .expect_err("a record region one byte off alignment must be rejected");
        assert!(
            matches!(
                err,
                super::MmapIndexError::MisalignedRecords {
                    file: "turbo_static.bin",
                    align: 8,
                }
            ),
            "{err}"
        );
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

    #[test]
    fn check_ext_blob_bounds_rejects_huge_offset_without_wrapping() {
        // A forged offset near `u64::MAX` must be rejected. On 64-bit targets the
        // value fits `usize` so the `checked_add`/bounds path catches it; on 32-bit
        // targets it would not fit `usize` and the new `usize::try_from` guard
        // catches it before any truncating cast could wrap it into bounds. Either
        // way the result must be the out-of-bounds error, never a false pass.
        let err = super::check_ext_blob_bounds(8, u64::MAX - 3, 4, 0, "docid")
            .expect_err("huge offset must be rejected as out of bounds");
        assert!(matches!(
            err,
            super::MmapIndexError::MetaExtBlobOutOfBounds { blob: "docid", .. }
        ));
    }
}
