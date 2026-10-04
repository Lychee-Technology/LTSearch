use ltsearch::index::{
    HeaderCodec, KnownRecordLayout, TurboCodecId, TurboHeader, TurboHeaderError,
    TurboProdRecord512, TurboRecord512, TURBO_MAGIC,
};

const FINGERPRINT: [u8; 8] = [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18];

fn v4_header(record_count: u64) -> TurboHeader {
    TurboHeader::new_v4(
        512,
        record_count,
        HeaderCodec {
            codec_id: TurboCodecId::TurboQuantProdV1,
            fingerprint: FINGERPRINT,
        },
    )
}

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
    for header in [
        TurboHeader::new(512, 100),
        TurboHeader::new_v3(512, 100),
        v4_header(100),
    ] {
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
fn header_accepts_versions_2_to_4_and_rejects_every_other() {
    // A v4 header carries a valid codec, so only the version decides.
    let template = v4_header(1).to_bytes();
    for version in [0u32, 1, 5, 6, u32::MAX] {
        let mut bytes = template.clone();
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
        assert_eq!(
            TurboHeader::from_bytes(&bytes),
            Err(TurboHeaderError::UnsupportedVersion { version }),
        );
    }
    for version in [2u32, 3, 4] {
        let mut bytes = template.clone();
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
        assert_eq!(TurboHeader::from_bytes(&bytes).unwrap().version(), version);
    }
}

#[test]
fn a_newer_version_is_reported_as_a_deploy_ordering_problem() {
    // A release in a format newer than the reader was activated before the
    // reader was deployed; the message has to say so.
    let newer = TurboHeaderError::UnsupportedVersion { version: 5 }.to_string();
    assert_eq!(
        newer,
        "unsupported version: 5 (this build reads versions 2 to 4; deploy a build that reads \
         version 5 before activating this release)"
    );
    let older = TurboHeaderError::UnsupportedVersion { version: 1 }.to_string();
    assert_eq!(
        older,
        "unsupported version: 1 (this build reads versions 2 to 4)"
    );
}

#[test]
fn v4_header_roundtrips_its_codec_and_fingerprint() {
    let header = v4_header(3);
    assert_eq!(header.version(), 4);

    let bytes = header.to_bytes();
    assert_eq!(bytes.len(), TurboHeader::SIZE);
    assert_eq!(bytes[20..24], 2u32.to_le_bytes());
    assert_eq!(bytes[24..32], FINGERPRINT);

    let parsed = TurboHeader::from_bytes(&bytes).unwrap();
    assert_eq!(parsed, header);
    assert_eq!(parsed.codec_id(), Some(TurboCodecId::TurboQuantProdV1));
    assert_eq!(parsed.codec_fingerprint(), Some(FINGERPRINT));
    assert_eq!(
        KnownRecordLayout::from_header(&parsed),
        Ok(KnownRecordLayout::V4Dim512)
    );
    assert_eq!(
        parsed.expected_file_size(),
        Ok(TurboHeader::SIZE as u64 + 3 * 208)
    );
}

#[test]
fn v2_and_v3_headers_write_zeros_and_ignore_bytes_20_to_32() {
    for header in [TurboHeader::new(512, 1), TurboHeader::new_v3(512, 1)] {
        let mut bytes = header.to_bytes();
        assert_eq!(bytes[20..32], [0u8; 12]);

        // No v2 or v3 reader ever looked at these bytes, so an image with
        // anything in them still has to load.
        bytes[20..32].fill(0xFF);
        let parsed = TurboHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(parsed.codec_id(), None);
        assert_eq!(parsed.codec_fingerprint(), None);
    }
}

#[test]
fn v4_header_rejects_a_codec_code_it_does_not_know() {
    for code in [0u32, 3, u32::MAX] {
        let mut bytes = v4_header(1).to_bytes();
        bytes[20..24].copy_from_slice(&code.to_le_bytes());
        assert_eq!(
            TurboHeader::from_bytes(&bytes),
            Err(TurboHeaderError::UnknownCodec { code }),
        );
    }
}

#[test]
fn v4_header_naming_the_legacy_codec_has_no_layout() {
    // The code is a known codec, so the header parses, but v4 records are
    // not legacy codes: there is no record type to read them as.
    let mut bytes = v4_header(1).to_bytes();
    bytes[20..24].copy_from_slice(&TurboCodecId::Legacy3BitV1.code().to_le_bytes());
    let header = TurboHeader::from_bytes(&bytes).unwrap();

    let unsupported = TurboHeaderError::UnsupportedCodec {
        version: 4,
        codec_id: TurboCodecId::Legacy3BitV1,
    };
    assert_eq!(
        KnownRecordLayout::from_header(&header),
        Err(unsupported.clone())
    );
    assert_eq!(header.expected_file_size(), Err(unsupported));
}

#[test]
fn v4_header_with_an_unsupported_dim_has_no_layout() {
    let mut bytes = v4_header(1).to_bytes();
    bytes[8..12].copy_from_slice(&384u32.to_le_bytes());
    let header = TurboHeader::from_bytes(&bytes).unwrap();
    assert_eq!(
        KnownRecordLayout::from_header(&header),
        Err(TurboHeaderError::UnsupportedLayout {
            version: 4,
            dim: 384
        })
    );
}

#[test]
fn layout_v3_dim512_record_size_matches_v2() {
    let header = TurboHeader::new_v3(512, 1);
    let layout = KnownRecordLayout::from_header(&header).unwrap();
    assert_eq!(layout.record_size(), std::mem::size_of::<TurboRecord512>());
}

#[test]
fn layout_v4_dim512_record_size_is_the_prod_records() {
    let layout = KnownRecordLayout::from_header(&v4_header(1)).unwrap();
    assert_eq!(
        layout.record_size(),
        std::mem::size_of::<TurboProdRecord512>()
    );
}
