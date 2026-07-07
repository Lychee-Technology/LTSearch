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

impl MetaRecord {
    pub fn text_from_blob<'a>(&self, blob: &'a [u8]) -> &'a str {
        let start = self.text_offset as usize;
        let end = start + self.text_len as usize;
        std::str::from_utf8(&blob[start..end]).expect("text blob contains invalid UTF-8")
    }

    pub fn title_from_blob<'a>(&self, blob: &'a [u8]) -> Option<&'a str> {
        if self.title_len == 0 {
            return None;
        }
        let start = self.title_offset as usize;
        let end = start + self.title_len as usize;
        Some(std::str::from_utf8(&blob[start..end]).expect("title blob contains invalid UTF-8"))
    }
}
