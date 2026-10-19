use std::str::Utf8Error;

pub const META_RECORD_SIZE: usize = 40;

pub type CorpusTypeId = u8;

// Field order is chosen so the three `u64`s are grouped: this packs the record
// to exactly 40 bytes under `repr(C)`. Appending the title fields after the
// existing tail would pad to 48 bytes because of the trailing `u64` alignment.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaRecord {
    pub doc_id: u64,
    pub text_offset: u64,
    pub title_offset: u64,
    pub text_len: u32,
    pub title_len: u32,
    pub corpus_type: CorpusTypeId,
    pub _pad: [u8; 7],
}

// The loader casts the mmapped `turbo_static_meta.bin` to this type.
const _: () = assert!(
    std::mem::size_of::<MetaRecord>() == META_RECORD_SIZE
        && std::mem::align_of::<MetaRecord>() == 8
);

// As for `MetaExtRecord`, the accessors index the blob directly because
// `MmapIndex::load` has checked that every record's ranges lie inside it, and
// UTF-8 is checked on read so that a load doesn't read the whole text blob.
impl MetaRecord {
    pub fn text_from_blob<'a>(&self, blob: &'a [u8]) -> Result<&'a str, Utf8Error> {
        let start = self.text_offset as usize;
        let end = start + self.text_len as usize;
        std::str::from_utf8(&blob[start..end])
    }

    pub fn title_from_blob<'a>(&self, blob: &'a [u8]) -> Result<Option<&'a str>, Utf8Error> {
        if self.title_len == 0 {
            return Ok(None);
        }
        let start = self.title_offset as usize;
        let end = start + self.title_len as usize;
        std::str::from_utf8(&blob[start..end]).map(Some)
    }
}
