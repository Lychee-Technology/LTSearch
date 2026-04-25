use super::header::TurboHeader;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurboRecord512 {
    pub doc_id: u64,
    pub idx: [u8; 128],
    pub qjl: [u8; 64],
    pub gamma: f32,
    pub _reserved: [u8; 4],
}

#[derive(Debug, Clone, Copy)]
pub enum TypedTurboRecordRef<'a> {
    V1Dim512(&'a TurboRecord512),
}

#[derive(Debug, Clone, Copy)]
pub enum TurboRecordSlice<'a> {
    V1Dim512(&'a [TurboRecord512]),
}

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

    pub fn from_turbo_record_512(record: &'a TurboRecord512, header: &'a TurboHeader) -> Self {
        let data = unsafe {
            std::slice::from_raw_parts(
                record as *const TurboRecord512 as *const u8,
                std::mem::size_of::<TurboRecord512>(),
            )
        };

        Self { data, header }
    }
}
