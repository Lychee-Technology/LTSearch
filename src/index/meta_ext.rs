pub const META_EXT_RECORD_SIZE: usize = 24;

// Field order places u64s first to avoid tail padding (see meta.rs:5-7 for design rationale).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaExtRecord {
    pub docid_offset: u64,
    pub meta_json_offset: u64,
    pub docid_len: u32,
    pub meta_json_len: u32,
}

impl MetaExtRecord {
    pub fn doc_id_from_blob<'a>(&self, blob: &'a [u8]) -> &'a str {
        let start = self.docid_offset as usize;
        let end = start + self.docid_len as usize;
        std::str::from_utf8(&blob[start..end]).expect("docid blob contains invalid UTF-8")
    }

    pub fn metadata_json_from_blob<'a>(&self, blob: &'a [u8]) -> &'a str {
        let start = self.meta_json_offset as usize;
        let end = start + self.meta_json_len as usize;
        std::str::from_utf8(&blob[start..end]).expect("metadata_json blob contains invalid UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_ext_record_has_fixed_size() {
        assert_eq!(std::mem::size_of::<MetaExtRecord>(), META_EXT_RECORD_SIZE);
    }

    #[test]
    fn meta_ext_reads_docid_and_json_from_blob() {
        let docid_blob = b"doc-1doc-2";
        let json_blob = br#"{"a":1}{"b":2}"#;
        let record = MetaExtRecord { docid_offset: 5, docid_len: 5, meta_json_offset: 7, meta_json_len: 7 };
        assert_eq!(record.doc_id_from_blob(docid_blob), "doc-2");
        assert_eq!(record.metadata_json_from_blob(json_blob), r#"{"b":2}"#);
    }
}
