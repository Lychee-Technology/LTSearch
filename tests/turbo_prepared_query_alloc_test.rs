//! Heap-allocation count of the per-record scoring path.
//!
//! This is its own test binary because it installs a counting
//! `#[global_allocator]`, which would otherwise apply to every test in the
//! process. Counters are thread-local, so tests running concurrently on other
//! harness threads do not leak into a measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;

use ltsearch::index::{CentroidTable, PreparedTurboQuery, ProjectionMatrix, TurboRecord512};

struct CountingAllocator;

thread_local! {
    // `const` init: no lazy initialization or destructor registration, so the
    // allocator can touch it without re-entering itself.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocation() {
    // `try_with`: the slot may already be gone while a thread is exiting.
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        System.realloc(ptr, layout, new_size)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Heap allocations (alloc, alloc_zeroed, realloc) made on this thread by `f`.
fn allocations_during<T>(f: impl FnOnce() -> T) -> (usize, T) {
    let before = ALLOCATIONS.with(Cell::get);
    let output = f();
    let after = ALLOCATIONS.with(Cell::get);
    (after - before, output)
}

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
fn counting_allocator_observes_heap_allocations() {
    // Guards against a vacuous zero in the test below.
    let (allocations, buffer) = allocations_during(|| black_box(Vec::<u8>::with_capacity(64)));
    drop(buffer);

    assert!(allocations >= 1, "counter saw {allocations} allocations");
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
