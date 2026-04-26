use std::fs;
use std::path::Path;
use std::slice;

use memmap2::Mmap;

use crate::models::CorpusType;

use super::types::{Centroids, MetaRecord, ProjectionMatrix, TurboRecord};

pub struct MmapIndex {
    // Keep Mmap alive so the memory remains mapped.
    _records_mmap: Mmap,
    _meta_mmap: Mmap,
    _text_mmap: Mmap,
    pub records: &'static [TurboRecord],
    pub meta: &'static [MetaRecord],
    pub text_blob: &'static [u8],
    pub centroids: Centroids,
    pub projection: ProjectionMatrix,
}

impl MmapIndex {
    /// Load index files from the given directory.
    /// Files expected: turbo_static.bin, turbo_static_meta.bin,
    /// turbo_static_text.bin, centroids.bin, projection.bin
    pub fn load_from_dir(dir: &Path) -> anyhow::Result<Self> {
        let records_mmap = open_mmap(&dir.join("turbo_static.bin"))?;
        let meta_mmap = open_mmap(&dir.join("turbo_static_meta.bin"))?;
        let text_mmap = open_mmap(&dir.join("turbo_static_text.bin"))?;

        let records: &[TurboRecord] = cast_slice(&records_mmap);
        let meta: &[MetaRecord] = cast_slice(&meta_mmap);
        let text_blob: &[u8] = &text_mmap[..];

        // Safety: we extend lifetime to 'static because _*_mmap keeps the
        // backing memory alive for the process lifetime when stored in a
        // OnceLock<MmapIndex>.
        let records: &'static [TurboRecord] = unsafe { extend_lifetime(records) };
        let meta: &'static [MetaRecord] = unsafe { extend_lifetime(meta) };
        let text_blob: &'static [u8] = unsafe { extend_lifetime(text_blob) };

        let centroids = load_centroids(&dir.join("centroids.bin"))?;
        let projection = load_projection(&dir.join("projection.bin"))?;

        Ok(Self {
            _records_mmap: records_mmap,
            _meta_mmap: meta_mmap,
            _text_mmap: text_mmap,
            records,
            meta,
            text_blob,
            centroids,
            projection,
        })
    }

    /// Load from the fixed path inside the Docker image.
    pub fn load_from_image() -> anyhow::Result<Self> {
        Self::load_from_dir(Path::new("/app/static"))
    }

    /// Return the raw text for a MetaRecord.
    pub fn text_of(&self, m: &MetaRecord) -> &str {
        let start = m.text_offset as usize;
        let end = start + m.text_len as usize;
        std::str::from_utf8(&self.text_blob[start..end]).unwrap_or("[invalid utf8]")
    }

    /// Map MetaRecord corpus_type byte to the CorpusType enum.
    pub fn corpus_type_of(m: &MetaRecord) -> CorpusType {
        match m.corpus_type {
            0 => CorpusType::Legal,
            1 => CorpusType::Contract,
            2 => CorpusType::Rfc,
            other => CorpusType::Other(other),
        }
    }
}

fn open_mmap(path: &Path) -> anyhow::Result<Mmap> {
    let file =
        fs::File::open(path).map_err(|e| anyhow::anyhow!("failed to open {:?}: {}", path, e))?;
    // Safety: caller must ensure the file is not modified while mapped.
    let mmap = unsafe { Mmap::map(&file) }
        .map_err(|e| anyhow::anyhow!("failed to mmap {:?}: {}", path, e))?;
    Ok(mmap)
}

fn cast_slice<T: Copy>(mmap: &Mmap) -> &[T] {
    let size = std::mem::size_of::<T>();
    assert!(
        mmap.len().is_multiple_of(size),
        "file length {} is not a multiple of record size {}",
        mmap.len(),
        size
    );
    let count = mmap.len() / size;
    // Safety: TurboRecord/MetaRecord are repr(C, packed), all-bits-valid,
    // and the file was written with the same layout.
    unsafe { slice::from_raw_parts(mmap.as_ptr() as *const T, count) }
}

unsafe fn extend_lifetime<T: ?Sized>(r: &T) -> &'static T {
    &*(r as *const T)
}

fn load_centroids(path: &Path) -> anyhow::Result<Centroids> {
    let bytes = fs::read(path).map_err(|e| anyhow::anyhow!("failed to read {:?}: {}", path, e))?;
    // File format: flat f32 values, 4 per dimension, little-endian.
    assert!(bytes.len() % (4 * 4) == 0, "centroids.bin size mismatch");
    let values: Vec<[f32; 4]> = bytes
        .chunks_exact(4 * 4)
        .map(|chunk| {
            let c0 = f32::from_le_bytes(chunk[0..4].try_into().unwrap());
            let c1 = f32::from_le_bytes(chunk[4..8].try_into().unwrap());
            let c2 = f32::from_le_bytes(chunk[8..12].try_into().unwrap());
            let c3 = f32::from_le_bytes(chunk[12..16].try_into().unwrap());
            [c0, c1, c2, c3]
        })
        .collect();
    Ok(Centroids { values })
}

fn load_projection(path: &Path) -> anyhow::Result<ProjectionMatrix> {
    let bytes = fs::read(path).map_err(|e| anyhow::anyhow!("failed to read {:?}: {}", path, e))?;
    // File format: header [rows: u32 LE, cols: u32 LE] then flat f32 LE values.
    anyhow::ensure!(bytes.len() >= 8, "projection.bin too small");
    let rows = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let cols = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let float_bytes = &bytes[8..];
    anyhow::ensure!(
        float_bytes.len() == rows * cols * 4,
        "projection.bin size mismatch: expected {} bytes, got {}",
        rows * cols * 4,
        float_bytes.len()
    );
    let values: Vec<f32> = float_bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    Ok(ProjectionMatrix { values, rows, cols })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_records(dir: &Path, records: &[TurboRecord]) {
        let bytes = unsafe {
            slice::from_raw_parts(
                records.as_ptr() as *const u8,
                std::mem::size_of_val(records),
            )
        };
        fs::write(dir.join("turbo_static.bin"), bytes).unwrap();
    }

    fn write_meta(dir: &Path, meta: &[MetaRecord]) {
        let bytes = unsafe {
            slice::from_raw_parts(meta.as_ptr() as *const u8, std::mem::size_of_val(meta))
        };
        fs::write(dir.join("turbo_static_meta.bin"), bytes).unwrap();
    }

    fn write_centroids(dir: &Path, dims: usize) {
        let data: Vec<f32> = vec![0.0; dims * 4];
        let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
        fs::write(dir.join("centroids.bin"), bytes).unwrap();
    }

    fn write_projection(dir: &Path, rows: usize, cols: usize) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(rows as u32).to_le_bytes());
        bytes.extend_from_slice(&(cols as u32).to_le_bytes());
        let data: Vec<f32> = vec![0.0; rows * cols];
        for v in &data {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        fs::write(dir.join("projection.bin"), bytes).unwrap();
    }

    #[test]
    fn load_and_read_records() {
        let dir = TempDir::new().unwrap();
        let record = TurboRecord {
            doc_id: 42,
            idx: [1u8; 96],
            qjl: [0xFFu8; 48],
            gamma: 1.5,
        };
        write_records(dir.path(), &[record]);
        let meta = MetaRecord {
            doc_id: 42,
            corpus_type: 0,
            _pad: [0; 3],
            text_offset: 0,
            text_len: 5,
        };
        write_meta(dir.path(), &[meta]);
        fs::write(dir.path().join("turbo_static_text.bin"), b"hello").unwrap();
        write_centroids(dir.path(), 384);
        write_projection(dir.path(), 384, 384);

        let index = MmapIndex::load_from_dir(dir.path()).unwrap();
        assert_eq!(index.records.len(), 1);
        // Read doc_id safely from packed struct
        let doc_id = { index.records[0].doc_id };
        assert_eq!(doc_id, 42);
        assert_eq!(index.text_of(&index.meta[0]), "hello");
    }

    #[test]
    fn corpus_type_mapping() {
        let cases = [
            (0, CorpusType::Legal),
            (1, CorpusType::Contract),
            (2, CorpusType::Rfc),
            (99, CorpusType::Other(99)),
        ];
        for (byte, expected) in cases {
            let m = MetaRecord {
                doc_id: 0,
                corpus_type: byte,
                _pad: [0; 3],
                text_offset: 0,
                text_len: 0,
            };
            assert_eq!(MmapIndex::corpus_type_of(&m), expected);
        }
    }
}
