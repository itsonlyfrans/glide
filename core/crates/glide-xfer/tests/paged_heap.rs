#![cfg(feature = "file-engine")]

use glide_proto::wire::*;
use glide_xfer::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn add(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: layouts and pointers are forwarded unchanged to the System allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplied a valid allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            add(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: pointer/layout match an allocation forwarded above.
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: pointer/layout match an allocation; size is valid.
        let result = unsafe { System.realloc(pointer, layout, size) };
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

#[test]
fn million_entry_synthetic_tree_has_constant_rust_heap() {
    let root = tempfile::tempdir_in(std::env::var_os("CARGO_TARGET_DIR").expect("external target"))
        .expect("spool parent");
    let config = Config::default();
    let paging = PagingConfig {
        max_entries: 1_000_000,
        max_roots: 1,
        ..PagingConfig::default()
    };
    let page_size = paging.page_entries;
    let cancel = Cancel::new();
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let mut spool = PageSpool::new(root.path(), config, paging).expect("spool");
    let empty_hash = *blake3::hash(&[]).as_bytes();
    let started = std::time::Instant::now();
    for (page, start) in (0usize..1_000_000).step_by(page_size).enumerate() {
        let end = (start + page_size).min(1_000_000);
        let mut files = ManifestFiles::new();
        for id in start..end {
            files
                .push(FileManifestEntry {
                    file_id: id as u32,
                    relative_path: if id == 0 {
                        "tree".into()
                    } else {
                        format!("tree/{id}.txt")
                    },
                    size: 0,
                    is_dir: id == 0,
                    blake3_hash: if id == 0 { None } else { Some(empty_hash) },
                })
                .expect("bounded page");
        }
        spool
            .push(
                &FileManifest {
                    transfer_id: "million".into(),
                    clip_id: "clip".into(),
                    chunk_size: MAX_FILE_CHUNK_BYTES as u32,
                    page: page as u32,
                    final_page: end == 1_000_000,
                    files,
                },
                None,
                &cancel,
            )
            .expect("validate and spool page");
    }
    let manifest = spool.finish().expect("complete manifest");
    assert_eq!(manifest.summary().items, 1_000_000);
    assert_eq!(manifest.summary().bytes, 0);
    assert_eq!(
        manifest.entry(999_999).expect("last entry").relative_path,
        "tree/999999.txt"
    );
    assert!(matches!(manifest.approve(), Consent::Approved { .. }));
    let peak = PEAK.load(Ordering::Relaxed);
    assert!(
        peak - baseline < 48 * 1024 * 1024,
        "Rust heap peak {peak}, baseline {baseline}"
    );
    println!("1M synthetic tree: peak Rust heap={peak}, added={}, manifest spool={} bytes, elapsed={:.2}s", peak - baseline, manifest.spool_bytes(), started.elapsed().as_secs_f64());
}
