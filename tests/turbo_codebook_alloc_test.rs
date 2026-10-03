//! Heap-allocation count of quantizing coordinates with the Lloyd-Max
//! codebook.

mod counting_allocator;

use std::hint::black_box;

use counting_allocator::allocations_during;
use ltsearch::index::LloydMaxCodebook;

#[test]
fn encode_and_decode_do_not_allocate() {
    let codebook = LloydMaxCodebook::committed(512, 2).unwrap();
    let coordinates = (0..512)
        .map(|dim| ((dim % 17) as f32 - 8.0) / 64.0)
        .collect::<Vec<_>>();

    let (allocations, sum) = allocations_during(|| {
        coordinates
            .iter()
            .map(|&x| codebook.decode(codebook.encode(black_box(x))))
            .sum::<f32>()
    });

    assert_eq!(allocations, 0, "quantizing allocated {allocations} times");
    assert!(sum.is_finite());
}
