//! Heap-allocation count of `TurboQuantProdV1`'s per-record scoring path.

mod counting_allocator;

use std::hint::black_box;

use counting_allocator::allocations_during;
use ltsearch::index::{
    NormPolicy, TurboCodecId, TurboProdCode, TurboQuantConfig, TurboQuantProdV1,
    GAUSSIAN_GENERATOR_VERSION,
};

const DIM: usize = 512;
const RECORD_COUNT: usize = 10_000;

/// Deterministic xorshift64 stream; the content only has to vary.
fn pseudo_random_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x0165_a110_c000_0001u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[test]
fn prepared_query_scores_ten_thousand_records_without_allocating() {
    let codec = TurboQuantProdV1::generate(TurboQuantConfig {
        codec_id: TurboCodecId::TurboQuantProdV1,
        mse_seed: 163,
        qjl_seed: 165,
        generator_version: GAUSSIAN_GENERATOR_VERSION,
        norm_policy: NormPolicy::NormalizeAndStore,
        ..TurboQuantConfig::legacy_v1()
    })
    .unwrap();
    let query = (0..DIM)
        .map(|dim| ((dim % 17) as f32 - 8.0) / 64.0)
        .collect::<Vec<_>>();
    // Records stored back to back, as a scan reads them.
    let (idx_len, signs_len) = (codec.idx_len(), codec.signs_len());
    let records = pseudo_random_bytes(RECORD_COUNT * (idx_len + signs_len));
    let prepared = codec.prepare_query(&query).unwrap();

    let (allocations, checksum) = allocations_during(|| {
        records
            .chunks_exact(idx_len + signs_len)
            .enumerate()
            .fold(0.0f32, |sum, (n, record)| {
                let (idx, signs) = record.split_at(idx_len);
                let code = TurboProdCode {
                    idx,
                    signs,
                    gamma: (n % 1_000) as f32 / 1_000.0,
                    norm: 1.0,
                };
                sum + prepared.score(black_box(code))
            })
    });

    assert_eq!(
        allocations, 0,
        "scoring {RECORD_COUNT} records allocated {allocations} times"
    );
    assert!(checksum.is_finite());
}
