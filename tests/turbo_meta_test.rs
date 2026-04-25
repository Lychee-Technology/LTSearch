use ltsearch::index::{MetaRecord, META_RECORD_SIZE};

#[test]
fn meta_record_size_is_32_bytes() {
    assert_eq!(std::mem::size_of::<MetaRecord>(), META_RECORD_SIZE);
    assert_eq!(META_RECORD_SIZE, 32);
}

#[test]
fn meta_record_alignment_matches_repr_c() {
    assert_eq!(std::mem::align_of::<MetaRecord>(), 8);
}

#[test]
fn meta_record_roundtrip_through_bytes() {
    let record = MetaRecord {
        doc_id: 42,
        corpus_type: 1,
        _pad: [0; 3],
        text_offset: 1024,
        text_len: 256,
    };

    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(&record as *const MetaRecord as *const u8, META_RECORD_SIZE)
    };

    let restored: &MetaRecord = unsafe { &*(bytes.as_ptr() as *const MetaRecord) };
    assert_eq!(restored.doc_id, 42);
    assert_eq!(restored.corpus_type, 1);
    assert_eq!(restored.text_offset, 1024);
    assert_eq!(restored.text_len, 256);
}

#[test]
fn meta_records_from_contiguous_buffer() {
    let records = [
        MetaRecord {
            doc_id: 1,
            corpus_type: 0,
            _pad: [0; 3],
            text_offset: 0,
            text_len: 100,
        },
        MetaRecord {
            doc_id: 2,
            corpus_type: 2,
            _pad: [0; 3],
            text_offset: 100,
            text_len: 200,
        },
    ];

    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            records.as_ptr() as *const u8,
            records.len() * META_RECORD_SIZE,
        )
    };

    let restored: &[MetaRecord] =
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const MetaRecord, 2) };
    assert_eq!(restored[0].doc_id, 1);
    assert_eq!(restored[0].text_len, 100);
    assert_eq!(restored[1].doc_id, 2);
    assert_eq!(restored[1].corpus_type, 2);
    assert_eq!(restored[1].text_offset, 100);
}

#[test]
fn meta_record_text_range_returns_correct_slice() {
    let record = MetaRecord {
        doc_id: 5,
        corpus_type: 0,
        _pad: [0; 3],
        text_offset: 10,
        text_len: 5,
    };
    let blob = b"__________hello__extra";
    let text = record.text_from_blob(blob);
    assert_eq!(text, "hello");
}
