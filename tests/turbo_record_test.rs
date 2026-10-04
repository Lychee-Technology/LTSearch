use std::mem::size_of;

use ltsearch::index::{KnownRecordLayout, TurboHeader, TurboRecord512, TurboRecordRef};

const LAYOUT: KnownRecordLayout = KnownRecordLayout::V3Dim512;

fn record_bytes(record: &TurboRecord512) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            record as *const TurboRecord512 as *const u8,
            size_of::<TurboRecord512>(),
        )
    }
}

fn test_record(doc_id: u64, gamma: f32) -> TurboRecord512 {
    TurboRecord512 {
        doc_id,
        idx: [0; 128],
        qjl: [0; 64],
        gamma,
        _reserved: [0; 4],
    }
}

#[test]
fn record_ref_reads_doc_id_and_gamma() {
    let record = test_record(42, 1.5);
    let view = TurboRecordRef::new(record_bytes(&record), LAYOUT);

    assert_eq!(view.doc_id(), 42);
    assert_eq!(view.gamma(), 1.5);
}

#[test]
fn record_ref_reads_the_fields_of_the_typed_record() {
    // Distinct bytes at both ends of each array, so a view that is off by one
    // field or one byte reads something else.
    let mut record = test_record(0x0102_0304_0506_0708, -0.25);
    record.idx[0] = 0xAB;
    record.idx[127] = 0xCD;
    record.qjl[0] = 0xFF;
    record.qjl[63] = 0x7E;

    for layout in [KnownRecordLayout::V2Dim512, KnownRecordLayout::V3Dim512] {
        let view = TurboRecordRef::new(record_bytes(&record), layout);
        assert_eq!(view.doc_id(), record.doc_id);
        assert_eq!(view.idx(), &record.idx);
        assert_eq!(view.qjl(), &record.qjl);
        assert_eq!(view.gamma(), record.gamma);
    }
}

#[test]
#[should_panic(expected = "record buffer too small: 204 < 208")]
fn record_ref_rejects_a_buffer_without_the_reserved_tail() {
    // 204 is the sum of the field sizes; the stored record is 208.
    let record = test_record(1, 0.0);
    TurboRecordRef::new(&record_bytes(&record)[..204], LAYOUT);
}

#[test]
fn records_from_contiguous_buffer() {
    let stride = LAYOUT.record_size();
    assert_eq!(stride, 208);

    let mut buf = Vec::new();
    for i in 0u64..3 {
        buf.extend_from_slice(record_bytes(&test_record(i + 10, i as f32 * 0.5)));
    }
    assert_eq!(
        TurboHeader::new_v3(512, 3).expected_file_size(),
        Ok((TurboHeader::SIZE + buf.len()) as u64)
    );

    let records: Vec<TurboRecordRef<'_>> = buf
        .chunks_exact(stride)
        .map(|chunk| TurboRecordRef::new(chunk, LAYOUT))
        .collect();

    assert_eq!(records.len(), 3);
    assert_eq!(records[0].doc_id(), 10);
    assert_eq!(records[1].doc_id(), 11);
    assert_eq!(records[2].doc_id(), 12);
    assert_eq!(records[2].gamma(), 1.0);
}
