use ltsearch::index::{TurboHeader, TurboRecordRef};

fn make_test_record(header: &TurboHeader, doc_id: u64, gamma: f32) -> Vec<u8> {
    let stride = header.record_stride();
    let mut buf = vec![0u8; stride];
    buf[0..8].copy_from_slice(&doc_id.to_le_bytes());
    let gamma_off = header.gamma_offset();
    buf[gamma_off..gamma_off + 4].copy_from_slice(&gamma.to_le_bytes());
    buf
}

#[test]
fn record_ref_reads_doc_id_and_gamma() {
    let header = TurboHeader::new(512, 1);
    let buf = make_test_record(&header, 42, 1.5);
    let record = TurboRecordRef::new(&buf, &header);

    assert_eq!(record.doc_id(), 42);
    assert!((record.gamma() - 1.5).abs() < f32::EPSILON);
}

#[test]
fn record_ref_reads_idx_bytes() {
    let header = TurboHeader::new(512, 1);
    let mut buf = make_test_record(&header, 1, 0.0);
    buf[header.idx_offset()] = 0xAB;
    let record = TurboRecordRef::new(&buf, &header);

    let idx = record.idx();
    assert_eq!(idx.len(), 128);
    assert_eq!(idx[0], 0xAB);
    assert_eq!(idx[1], 0x00);
}

#[test]
fn record_ref_reads_qjl_bytes() {
    let header = TurboHeader::new(512, 1);
    let mut buf = make_test_record(&header, 1, 0.0);
    buf[header.qjl_offset()] = 0xFF;
    let record = TurboRecordRef::new(&buf, &header);

    let qjl = record.qjl();
    assert_eq!(qjl.len(), 64);
    assert_eq!(qjl[0], 0xFF);
    assert_eq!(qjl[1], 0x00);
}

#[test]
fn record_ref_works_with_384_dim() {
    let header = TurboHeader::new(384, 1);
    let buf = make_test_record(&header, 99, 2.5);
    let record = TurboRecordRef::new(&buf, &header);

    assert_eq!(record.doc_id(), 99);
    assert_eq!(record.idx().len(), 96);
    assert_eq!(record.qjl().len(), 48);
    assert!((record.gamma() - 2.5).abs() < f32::EPSILON);
}

#[test]
fn records_from_contiguous_buffer() {
    let header = TurboHeader::new(512, 3);
    let stride = header.record_stride();
    let mut buf = vec![0u8; stride * 3];

    for i in 0u64..3 {
        let offset = i as usize * stride;
        buf[offset..offset + 8].copy_from_slice(&(i + 10).to_le_bytes());
        let gamma_off = offset + header.gamma_offset();
        buf[gamma_off..gamma_off + 4].copy_from_slice(&(i as f32 * 0.5).to_le_bytes());
    }

    let records: Vec<TurboRecordRef<'_>> = (0..3)
        .map(|i| {
            let offset = i * stride;
            TurboRecordRef::new(&buf[offset..offset + stride], &header)
        })
        .collect();

    assert_eq!(records[0].doc_id(), 10);
    assert_eq!(records[1].doc_id(), 11);
    assert_eq!(records[2].doc_id(), 12);
    assert!((records[2].gamma() - 1.0).abs() < f32::EPSILON);
}
