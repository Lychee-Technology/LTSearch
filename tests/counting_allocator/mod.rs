//! A counting `#[global_allocator]` for heap-allocation tests.
//!
//! Included via `mod counting_allocator;` by each allocation test binary.
//! Installing a global allocator affects every test in the process, so those
//! tests live in their own binaries. Counters are thread-local, so tests
//! running concurrently on other harness threads do not leak into a
//! measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;

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
pub fn allocations_during<T>(f: impl FnOnce() -> T) -> (usize, T) {
    let before = ALLOCATIONS.with(Cell::get);
    let output = f();
    let after = ALLOCATIONS.with(Cell::get);
    (after - before, output)
}

#[test]
fn counting_allocator_observes_heap_allocations() {
    // Guards against a vacuous zero in the including binary's tests.
    let (allocations, buffer) = allocations_during(|| black_box(Vec::<u8>::with_capacity(64)));
    drop(buffer);

    assert!(allocations >= 1, "counter saw {allocations} allocations");
}
