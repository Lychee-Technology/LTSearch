use ltsearch::index::{
    KnownRecordLayout, TurboHeader, TurboHeaderError, TurboRecord512, TURBO_MAGIC,
};

#[test]
fn header_roundtrip_for_512_dim() {
    let header = TurboHeader::new(512, 1000);
    assert_eq!(header.magic(), TURBO_MAGIC);
    assert_eq!(header.version(), 2);
    assert_eq!(header.dim(), 512);
    assert_eq!(header.record_count(), 1000);

    let bytes = header.to_bytes();
    assert_eq!(bytes.len(), TurboHeader::SIZE);

    let parsed = TurboHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.dim(), 512);
    assert_eq!(parsed.record_count(), 1000);
}

#[test]
fn every_known_layout_has_208_byte_records() {
    for header in [TurboHeader::new(512, 100), TurboHeader::new_v3(512, 100)] {
        let layout = KnownRecordLayout::from_header(&header).unwrap();
        assert_eq!(layout.record_size(), 208, "{layout:?}");
    }
}

#[test]
fn header_without_a_known_layout_has_no_file_size() {
    // A 384-dim header parses, but no record type exists for it, so there is
    // no record size to compute a file size from.
    let header = TurboHeader::new(384, 50);
    assert_eq!(
        header.expected_file_size(),
        Err(TurboHeaderError::UnsupportedLayout {
            version: 2,
            dim: 384
        })
    );
}

#[test]
fn header_rejects_legacy_v1_image() {
    // Craft a header that is valid except for the old version tag: a pre-title
    // (v1) static image must fail loudly rather than be misread under the 40B
    // MetaRecord layout.
    let mut bytes = TurboHeader::new(512, 10).to_bytes();
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    let err = TurboHeader::from_bytes(&bytes).unwrap_err();
    assert!(
        err.to_string().contains("unsupported version"),
        "expected an unsupported-version error, got: {err}"
    );
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
    let expected = TurboHeader::SIZE as u64 + 1000 * 208;
    assert_eq!(header.expected_file_size(), Ok(expected));
}

#[test]
fn header_expected_file_size_rejects_a_record_count_that_overflows() {
    // (2^60 + 1) * 208 wraps to 208, the size of a one-record body.
    for record_count in [(1 << 60) + 1, u64::MAX, u64::MAX / 208] {
        let header = TurboHeader::new(512, record_count);
        assert_eq!(
            header.expected_file_size(),
            Err(TurboHeaderError::RecordCountOverflow { record_count }),
        );
    }
    let largest = (u64::MAX - TurboHeader::SIZE as u64) / 208;
    assert_eq!(
        TurboHeader::new(512, largest).expected_file_size(),
        Ok(TurboHeader::SIZE as u64 + largest * 208)
    );
}

#[test]
fn header_roundtrips_v3_version() {
    let header = TurboHeader::new_v3(512, 3);
    assert_eq!(header.version(), 3);
    let parsed = TurboHeader::from_bytes(&header.to_bytes()).unwrap();
    assert_eq!(parsed.version(), 3);
    assert_eq!(parsed.dim(), 512);
    assert_eq!(parsed.record_count(), 3);
}

#[test]
fn header_rejects_unknown_version() {
    let mut bytes = TurboHeader::new_v3(512, 1).to_bytes();
    bytes[4..8].copy_from_slice(&4u32.to_le_bytes());
    assert!(matches!(
        TurboHeader::from_bytes(&bytes),
        Err(TurboHeaderError::UnsupportedVersion { version: 4 })
    ));
}

#[test]
fn layout_v3_dim512_record_size_matches_v2() {
    let header = TurboHeader::new_v3(512, 1);
    let layout = KnownRecordLayout::from_header(&header).unwrap();
    assert_eq!(layout.record_size(), std::mem::size_of::<TurboRecord512>());
}
