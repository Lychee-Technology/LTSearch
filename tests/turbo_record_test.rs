use std::mem::{align_of, size_of};

use ltsearch::index::{
    EncodedTurboProd, KnownRecordLayout, TurboHeader, TurboProdRecord512, TurboRecord512,
    TurboRecordRef,
};

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
        // v2 and v3 store no norm: those four bytes are reserved.
        assert_eq!(view.norm(), None);
    }
}

/// A code with distinct bytes at both ends of each array.
fn test_prod_code() -> EncodedTurboProd {
    let mut encoded = EncodedTurboProd {
        idx: vec![0; 128],
        signs: vec![0; 64],
        gamma: -0.25,
        norm: 3.5,
    };
    encoded.idx[0] = 0xAB;
    encoded.idx[127] = 0xCD;
    encoded.signs[0] = 0xFF;
    encoded.signs[63] = 0x7E;
    encoded
}

#[test]
fn prod_record_is_the_size_and_alignment_of_the_legacy_record() {
    assert_eq!(size_of::<TurboProdRecord512>(), 208);
    assert_eq!(align_of::<TurboProdRecord512>(), 8);
    assert_eq!(size_of::<TurboRecord512>(), 208);
    assert_eq!(align_of::<TurboRecord512>(), 8);
    assert_eq!(KnownRecordLayout::V4Dim512.record_size(), 208);
}

#[test]
fn prod_record_stores_the_code_it_is_built_from() {
    let encoded = test_prod_code();
    let record = TurboProdRecord512::new(0x0102_0304_0506_0708, &encoded).unwrap();

    assert_eq!(record.doc_id, 0x0102_0304_0506_0708);
    assert_eq!(record.code(), encoded.code());
}

#[test]
fn prod_record_file_bytes_are_the_documented_layout() {
    let encoded = test_prod_code();
    let record = TurboProdRecord512::new(0x0102_0304_0506_0708, &encoded).unwrap();
    let bytes = record.as_bytes();

    assert_eq!(bytes.len(), 208);
    assert_eq!(bytes[0..8], 0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(bytes[8..136], encoded.idx[..]);
    assert_eq!(bytes[136..200], encoded.signs[..]);
    assert_eq!(bytes[200..204], (-0.25f32).to_le_bytes());
    // The norm is where a v2/v3 record has its reserved bytes.
    assert_eq!(bytes[204..208], 3.5f32.to_le_bytes());

    let view = TurboRecordRef::new(bytes, KnownRecordLayout::V4Dim512);
    assert_eq!(view.doc_id(), record.doc_id);
    assert_eq!(view.idx(), &record.idx);
    assert_eq!(view.qjl(), &record.signs);
    assert_eq!(view.gamma(), record.gamma);
    assert_eq!(view.norm(), Some(3.5));
}

#[test]
fn prod_record_rejects_a_code_of_another_length() {
    let code = test_prod_code();
    for (idx_len, signs_len) in [(127, 64), (129, 64), (128, 63), (128, 65), (0, 0)] {
        let encoded = EncodedTurboProd {
            idx: vec![0; idx_len],
            signs: vec![0; signs_len],
            ..code.clone()
        };
        assert_eq!(
            TurboProdRecord512::new(1, &encoded),
            None,
            "idx {idx_len}, signs {signs_len}"
        );
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
