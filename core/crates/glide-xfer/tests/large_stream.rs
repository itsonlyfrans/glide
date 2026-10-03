#![cfg(feature = "file-engine")]

#[path = "../examples/support/mod.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn add(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every allocation is forwarded unchanged to System, with matching layout
// and pointer on deallocation/reallocation; only allocation size accounting is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller supplies a valid allocation layout to the system allocator.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            add(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: pointer and layout come from the matching forwarded allocation.
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: pointer/layout refer to the matching forwarded allocation; size is valid.
        let result = unsafe { System.realloc(ptr, layout, size) };
        if !result.is_null() {
            if size >= layout.size() {
                add(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        result
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[tokio::test]
async fn two_gib_synthetic_stream_has_constant_rust_heap() {
    let (left, right) = tokio::io::duplex(64 * 1024);
    let rate = support::framed_stream(left, right, 2 << 30, true)
        .await
        .expect("2 GiB verified stream");
    let peak = PEAK.load(Ordering::Relaxed);
    // This counts actual Rust allocations, including codecs and runtime. Native zstd
    // and OS allocations are excluded; their bounded window is configured separately.
    assert!(peak < 48 * 1024 * 1024, "peak Rust heap: {peak}");
    assert!(glide_xfer::Config::default().payload_memory_ceiling() < 192 * 1024 * 1024);
    println!("2 GiB, peak Rust heap={peak}, {rate:.3} GB/s (synthetic compressed duplex)");
    for bytes in [16 << 20, 64 << 20] {
        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        let (left, right) = tokio::io::duplex(64 * 1024);
        let mut sends = Vec::new();
        let mut receives = Vec::new();
        for _ in 0..4 {
            let (send, receive) = tokio::io::duplex(64 * 1024);
            sends.push(send);
            receives.push(receive);
        }
        support::engine_stream(left, right, sends, receives, bytes, false)
            .await
            .expect("full staged engine");
        let peak = PEAK.load(Ordering::Relaxed);
        assert!(
            peak < 2 * glide_xfer::Config::default().payload_memory_ceiling(),
            "engine pair peak {peak}"
        );
        println!("disk engine pair: bytes={bytes}, peak Rust heap={peak}");
    }
}
