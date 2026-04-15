//! Counting global-allocator shim.
//!
//! When enabled (via `enable()`, typically wired to `--bench`), every call to
//! the system allocator bumps a pair of atomic counters. `snapshot()` returns
//! the current totals so callers can compute deltas around a scope — the
//! `perf::Guard` does this to attribute allocations to labelled spans.
//!
//! Overhead when disabled is one relaxed atomic load per alloc. The
//! `#[global_allocator]` static must be defined in the binary crate;
//! `Counting` is the type to install.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn enable() {
    ENABLED.store(true, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Current `(alloc_count, alloc_bytes)` totals. Monotonic; take deltas.
pub fn snapshot() -> (u64, u64) {
    (ALLOC_COUNT.load(Relaxed), ALLOC_BYTES.load(Relaxed))
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOC_COUNT.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOC_COUNT.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            ALLOC_COUNT.fetch_add(1, Relaxed);
            // Count the growth, not the total — realloc often just extends
            // an existing allocation. Shrinks contribute zero bytes.
            if new_size > layout.size() {
                ALLOC_BYTES.fetch_add((new_size - layout.size()) as u64, Relaxed);
            }
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
