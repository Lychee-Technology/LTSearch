//! Heap-allocation count of applying a rotation.

mod counting_allocator;

use std::hint::black_box;

use counting_allocator::allocations_during;
use ltsearch::index::Rotation;

#[test]
fn rotate_and_inverse_rotate_do_not_allocate() {
    const DIM: usize = 512;
    let rotation = Rotation::generate(DIM as u32, 163);
    let x = (0..DIM)
        .map(|dim| ((dim % 17) as f32 - 8.0) / 64.0)
        .collect::<Vec<_>>();
    let mut rotated = vec![0.0; DIM];
    let mut restored = vec![0.0; DIM];

    let (allocations, result) = allocations_during(|| {
        rotation.rotate(black_box(&x), &mut rotated)?;
        rotation.inverse_rotate(black_box(&rotated), &mut restored)
    });

    assert_eq!(allocations, 0, "rotating allocated {allocations} times");
    result.unwrap();
    assert!(restored.iter().all(|value| value.is_finite()));
}
