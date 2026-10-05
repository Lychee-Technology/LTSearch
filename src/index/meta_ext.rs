use std::str::Utf8Error;

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

// The loader casts the mmapped `turbo_static_meta_ext.bin` to this type.
const _: () = assert!(
    std::mem::size_of::<MetaExtRecord>() == META_EXT_RECORD_SIZE
        && std::mem::align_of::<MetaExtRecord>() == 8
);

// The accessors index the blob directly: `MmapIndex::load` has checked that
// every record's ranges lie inside it. UTF-8 is checked here, on read, because
// checking it at load would read every sidecar byte each time a release loads.
impl MetaExtRecord {
    pub fn doc_id_from_blob<'a>(&self, blob: &'a [u8]) -> Result<&'a str, Utf8Error> {
        let start = self.docid_offset as usize;
        let end = start + self.docid_len as usize;
        std::str::from_utf8(&blob[start..end])
    }

    pub fn metadata_json_from_blob<'a>(&self, blob: &'a [u8]) -> Result<&'a str, Utf8Error> {
        let start = self.meta_json_offset as usize;
        let end = start + self.meta_json_len as usize;
        std::str::from_utf8(&blob[start..end])
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
        let record = MetaExtRecord {
            docid_offset: 5,
            docid_len: 5,
            meta_json_offset: 7,
            meta_json_len: 7,
        };
        assert_eq!(record.doc_id_from_blob(docid_blob), Ok("doc-2"));
        assert_eq!(record.metadata_json_from_blob(json_blob), Ok(r#"{"b":2}"#));
    }

    #[test]
    fn meta_ext_reports_invalid_utf8_instead_of_panicking() {
        let record = MetaExtRecord {
            docid_offset: 0,
            docid_len: 2,
            meta_json_offset: 1,
            meta_json_len: 1,
        };
        assert!(record.doc_id_from_blob(&[b'a', 0xFF]).is_err());
        assert!(record.metadata_json_from_blob(&[b'{', 0xC3]).is_err());
    }
}
