//! Heap-allocation count of the per-record scoring path.

mod counting_allocator;

use std::hint::black_box;

use counting_allocator::allocations_during;
use ltsearch::index::{CentroidTable, PreparedTurboQuery, ProjectionMatrix, TurboRecord512};

const DIM: usize = 512;
const RECORD_COUNT: usize = 10_000;

/// Deterministic xorshift64 stream; the content only has to vary.
fn pseudo_random_records(count: usize) -> Vec<TurboRecord512> {
    let mut state = 0x0162_a110_c000_0001u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..count)
        .map(|doc_id| {
            let mut record = TurboRecord512 {
                doc_id: doc_id as u64,
                idx: [0; 128],
                qjl: [0; 64],
                gamma: (next() % 1_000) as f32 / 1_000.0,
                _reserved: [0; 4],
            };
            record
                .idx
                .iter_mut()
                .chain(record.qjl.iter_mut())
                .for_each(|byte| *byte = next() as u8);
            record
        })
        .collect()
}

#[test]
fn prepared_query_scores_ten_thousand_records_without_allocating() {
    let centroids = CentroidTable::generate(DIM as u32, 4, 7);
    let projection = ProjectionMatrix::generate(DIM as u32, DIM as u32, 11);
    let query = (0..DIM)
        .map(|dim| ((dim % 17) as f32 - 8.0) / 64.0)
        .collect::<Vec<_>>();
    let records = pseudo_random_records(RECORD_COUNT);
    let prepared = PreparedTurboQuery::prepare(&query, &centroids, &projection).unwrap();

    let (allocations, checksum) = allocations_during(|| {
        records.iter().fold(0.0f32, |sum, record| {
            sum + prepared.score(black_box(record))
        })
    });

    assert_eq!(
        allocations, 0,
        "scoring {RECORD_COUNT} records allocated {allocations} times"
    );
    assert!(checksum.is_finite());
}
