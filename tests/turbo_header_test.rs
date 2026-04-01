use ltsearch::index::{TurboHeader, TURBO_MAGIC};

#[test]
fn header_roundtrip_for_512_dim() {
    let header = TurboHeader::new(512, 1000);
    assert_eq!(header.magic(), TURBO_MAGIC);
    assert_eq!(header.version(), 1);
    assert_eq!(header.dim(), 512);
    assert_eq!(header.record_count(), 1000);

    let bytes = header.to_bytes();
    assert_eq!(bytes.len(), TurboHeader::SIZE);

    let parsed = TurboHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.dim(), 512);
    assert_eq!(parsed.record_count(), 1000);
}

#[test]
fn header_computes_stride_for_512_dim() {
    let header = TurboHeader::new(512, 100);
    assert_eq!(header.idx_size(), 128);
    assert_eq!(header.qjl_size(), 64);
    assert_eq!(header.record_stride(), 204);
}

#[test]
fn header_computes_stride_for_384_dim() {
    let header = TurboHeader::new(384, 50);
    assert_eq!(header.idx_size(), 96);
    assert_eq!(header.qjl_size(), 48);
    assert_eq!(header.record_stride(), 156);
}

#[test]
fn header_rejects_bad_magic() {
    let mut bytes = TurboHeader::new(512, 10).to_bytes();
    bytes[0] = b'X';
    let err = TurboHeader::from_bytes(&bytes).unwrap_err();
    assert!(err.to_string().contains("magic"));
}

#[test]
fn header_rejects_zero_dim() {
    let mut bytes = TurboHeader::new(512, 10).to_bytes();
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    let err = TurboHeader::from_bytes(&bytes).unwrap_err();
    assert!(err.to_string().contains("dim"));
}

#[test]
fn header_rejects_short_buffer() {
    let err = TurboHeader::from_bytes(&[0u8; 16]).unwrap_err();
    assert!(err.to_string().contains("size"));
}

#[test]
fn header_expected_file_size_matches_data_region() {
    let header = TurboHeader::new(512, 1000);
    let expected = TurboHeader::SIZE as u64 + 1000 * 204;
    assert_eq!(header.expected_file_size(), expected);
}
