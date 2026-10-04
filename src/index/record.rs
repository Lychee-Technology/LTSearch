use std::mem::{align_of, offset_of, size_of};
use std::ops::Range;

use super::header::KnownRecordLayout;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurboRecord512 {
    pub doc_id: u64,
    pub idx: [u8; 128],
    pub qjl: [u8; 64],
    pub gamma: f32,
    pub _reserved: [u8; 4],
}

// The loader casts the mmapped record region to this type, so its size and
// alignment are part of the file format.
const _: () = assert!(size_of::<TurboRecord512>() == 208 && align_of::<TurboRecord512>() == 8);

#[derive(Debug, Clone, Copy)]
pub enum TypedTurboRecordRef<'a> {
    V2Dim512(&'a TurboRecord512),
}

#[derive(Debug, Clone, Copy)]
pub enum TurboRecordSlice<'a> {
    V2Dim512(&'a [TurboRecord512]),
}

/// Where a layout's typed record keeps each field.
struct FieldRanges {
    doc_id: usize,
    idx: Range<usize>,
    qjl: Range<usize>,
    gamma: usize,
}

const fn field_ranges(layout: KnownRecordLayout) -> FieldRanges {
    match layout {
        KnownRecordLayout::V2Dim512 | KnownRecordLayout::V3Dim512 => {
            let idx = offset_of!(TurboRecord512, idx);
            let qjl = offset_of!(TurboRecord512, qjl);
            FieldRanges {
                doc_id: offset_of!(TurboRecord512, doc_id),
                idx: idx..idx + 128,
                qjl: qjl..qjl + 64,
                gamma: offset_of!(TurboRecord512, gamma),
            }
        }
    }
}

/// One record as bytes, read at the field offsets of its layout's typed
/// record.
pub struct TurboRecordRef<'a> {
    data: &'a [u8],
    layout: KnownRecordLayout,
}

impl<'a> TurboRecordRef<'a> {
    /// Panics if `data` is shorter than one record of `layout`.
    pub fn new(data: &'a [u8], layout: KnownRecordLayout) -> Self {
        assert!(
            data.len() >= layout.record_size(),
            "record buffer too small: {} < {}",
            data.len(),
            layout.record_size()
        );

        Self { data, layout }
    }

    pub fn doc_id(&self) -> u64 {
        let start = field_ranges(self.layout).doc_id;
        u64::from_le_bytes(self.data[start..start + 8].try_into().unwrap())
    }

    pub fn idx(&self) -> &'a [u8] {
        &self.data[field_ranges(self.layout).idx]
    }

    pub fn qjl(&self) -> &'a [u8] {
        &self.data[field_ranges(self.layout).qjl]
    }

    pub fn gamma(&self) -> f32 {
        let start = field_ranges(self.layout).gamma;
        f32::from_le_bytes(self.data[start..start + 4].try_into().unwrap())
    }
}
