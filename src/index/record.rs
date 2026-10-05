use std::mem::{align_of, offset_of, size_of};
use std::ops::Range;

use super::header::KnownRecordLayout;
use super::turbo_prod::{EncodedTurboProd, TurboProdCode, TurboQuantProdV1};

/// A v2/v3 record: a `Legacy3BitV1` code.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurboRecord512 {
    pub doc_id: u64,
    pub idx: [u8; 128],
    pub qjl: [u8; 64],
    pub gamma: f32,
    pub _reserved: [u8; 4],
}

/// A v4 record: a `TurboQuantProdV1` code. It is the size of
/// [`TurboRecord512`], with ‖x‖ where that has four reserved bytes, but a
/// separate type: its `idx` indexes the shared Lloyd-Max codebook in the
/// rotated space, not per-dimension centroids, so scoring one as the other
/// would return numbers that mean nothing.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurboProdRecord512 {
    pub doc_id: u64,
    /// [`EncodedTurboProd::idx`].
    pub idx: [u8; 128],
    /// [`EncodedTurboProd::signs`].
    pub signs: [u8; 64],
    /// γ = ‖r′‖.
    pub gamma: f32,
    /// ‖x‖.
    pub norm: f32,
}

// The loader casts the mmapped record region to these types, so their size
// and alignment are part of the file format.
const _: () = assert!(size_of::<TurboRecord512>() == 208 && align_of::<TurboRecord512>() == 8);
const _: () =
    assert!(size_of::<TurboProdRecord512>() == 208 && align_of::<TurboProdRecord512>() == 8);

impl TurboProdRecord512 {
    /// Whether `codec`'s codes are exactly this record's `idx` and `signs`
    /// fields: d = 512 with 2-bit indices, and 505..=512 sign bits.
    pub fn holds_codes_of(codec: &TurboQuantProdV1) -> bool {
        codec.idx_len() == size_of::<[u8; 128]>() && codec.signs_len() == size_of::<[u8; 64]>()
    }

    /// `None` if the code's index or sign bytes aren't the length of the
    /// record's fields.
    pub fn new(doc_id: u64, encoded: &EncodedTurboProd) -> Option<Self> {
        Some(Self {
            doc_id,
            idx: encoded.idx.as_slice().try_into().ok()?,
            signs: encoded.signs.as_slice().try_into().ok()?,
            gamma: encoded.gamma,
            norm: encoded.norm,
        })
    }

    /// The record as its file bytes.
    pub fn as_bytes(&self) -> &[u8] {
        // Safety: `repr(C)`, and the fields' sizes add up to the 208 bytes
        // asserted above, so there is no padding byte to read.
        unsafe { std::slice::from_raw_parts((self as *const Self).cast::<u8>(), size_of::<Self>()) }
    }

    /// The code to score, borrowed from the record.
    pub fn code(&self) -> TurboProdCode<'_> {
        TurboProdCode {
            idx: &self.idx,
            signs: &self.signs,
            gamma: self.gamma,
            norm: self.norm,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TypedTurboRecordRef<'a> {
    V2Dim512(&'a TurboRecord512),
    V4Dim512(&'a TurboProdRecord512),
}

/// The record region as its typed records. v2 and v3 share a record type.
#[derive(Debug, Clone, Copy)]
pub enum TurboRecordSlice<'a> {
    V2Dim512(&'a [TurboRecord512]),
    V4Dim512(&'a [TurboProdRecord512]),
}

/// Where a layout's typed record keeps each field.
struct FieldRanges {
    doc_id: usize,
    idx: Range<usize>,
    qjl: Range<usize>,
    gamma: usize,
    norm: Option<usize>,
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
                norm: None,
            }
        }
        KnownRecordLayout::V4Dim512 => {
            let idx = offset_of!(TurboProdRecord512, idx);
            let signs = offset_of!(TurboProdRecord512, signs);
            FieldRanges {
                doc_id: offset_of!(TurboProdRecord512, doc_id),
                idx: idx..idx + 128,
                qjl: signs..signs + 64,
                gamma: offset_of!(TurboProdRecord512, gamma),
                norm: Some(offset_of!(TurboProdRecord512, norm)),
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

    /// The QJL sign bits: `qjl` of a v2/v3 record, `signs` of a v4 one.
    pub fn qjl(&self) -> &'a [u8] {
        &self.data[field_ranges(self.layout).qjl]
    }

    pub fn gamma(&self) -> f32 {
        let start = field_ranges(self.layout).gamma;
        f32::from_le_bytes(self.data[start..start + 4].try_into().unwrap())
    }

    /// The stored ‖x‖. `None` for v2 and v3, which store no norm.
    pub fn norm(&self) -> Option<f32> {
        let start = field_ranges(self.layout).norm?;
        Some(f32::from_le_bytes(
            self.data[start..start + 4].try_into().unwrap(),
        ))
    }
}
