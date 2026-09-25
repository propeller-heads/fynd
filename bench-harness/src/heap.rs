//! A global allocator that counts the live heap and its peak, for the derived bench's memory
//! figures.
//!
//! The binary installs [`CountingAllocator`] with `#[global_allocator]`; [`sample`] then reads the
//! counts. In a binary without it, every sample is zero.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
};

use fynd_core::derived::bench::HeapSample;

/// Bytes allocated and not yet freed.
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// The largest `LIVE_BYTES` since the last [`sample`].
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting what it hands out.
pub struct CountingAllocator;

impl CountingAllocator {
    fn add(size: usize) {
        let live = LIVE_BYTES.fetch_add(size, Ordering::Relaxed) + size;
        PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
    }

    fn sub(size: usize) {
        LIVE_BYTES.fetch_sub(size, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to `System` with the caller's arguments, so it keeps `System`'s
// guarantees; the counters only observe sizes.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract, which `System` shares.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            Self::add(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            Self::add(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, which is `System`, with this `layout`.
        unsafe { System.dealloc(ptr, layout) };
        Self::sub(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as in `dealloc`, and the caller upholds `realloc`'s size contract.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            Self::sub(layout.size());
            Self::add(new_size);
        }
        new_ptr
    }
}

/// Returns the live heap and the peak since the previous call, and starts a new peak at the live
/// heap.
pub fn sample() -> HeapSample {
    let live_bytes = LIVE_BYTES.load(Ordering::Relaxed);
    let peak_bytes = PEAK_BYTES
        .swap(live_bytes, Ordering::Relaxed)
        .max(live_bytes);
    HeapSample { live_bytes, peak_bytes }
}
