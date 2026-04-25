pub const META_RECORD_SIZE: usize = 32;

pub type CorpusTypeId = u8;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaRecord {
    pub doc_id: u64,
    pub corpus_type: CorpusTypeId,
    pub _pad: [u8; 3],
    pub text_offset: u64,
    pub text_len: u32,
}

impl MetaRecord {
    pub fn text_from_blob<'a>(&self, blob: &'a [u8]) -> &'a str {
        let start = self.text_offset as usize;
        let end = start + self.text_len as usize;
        std::str::from_utf8(&blob[start..end]).expect("text blob contains invalid UTF-8")
    }
}
