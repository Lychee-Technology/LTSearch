use super::header::TurboHeader;

pub struct TurboRecordRef<'a> {
    data: &'a [u8],
    header: &'a TurboHeader,
}

impl<'a> TurboRecordRef<'a> {
    pub fn new(data: &'a [u8], header: &'a TurboHeader) -> Self {
        assert!(
            data.len() >= header.record_stride(),
            "record buffer too small: {} < {}",
            data.len(),
            header.record_stride()
        );

        Self { data, header }
    }

    pub fn doc_id(&self) -> u64 {
        u64::from_le_bytes(self.data[0..8].try_into().unwrap())
    }

    pub fn idx(&self) -> &'a [u8] {
        let start = self.header.idx_offset();
        &self.data[start..start + self.header.idx_size()]
    }

    pub fn qjl(&self) -> &'a [u8] {
        let start = self.header.qjl_offset();
        &self.data[start..start + self.header.qjl_size()]
    }

    pub fn gamma(&self) -> f32 {
        let start = self.header.gamma_offset();
        f32::from_le_bytes(self.data[start..start + 4].try_into().unwrap())
    }
}
