//! The heap against a shadow model: no overlap, alignment, contents intact,
//! full coalescing, refusal of double frees, and the spin-locked wrapper.

use std::alloc::{GlobalAlloc, Layout};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use nanox_runtime::heap::{GrowHook, GRAIN};
use nanox_runtime::{Heap, HeapError, LockedHeap};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// 16-byte-aligned backing memory that lives as long as the test.
struct Arena {
    mem: Vec<u128>,
}

impl Arena {
    fn new(bytes: usize) -> Self {
        Self {
            mem: vec![0u128; bytes / 16],
        }
    }
    fn start(&mut self) -> *mut u8 {
        self.mem.as_mut_ptr().cast()
    }
    fn len(&self) -> usize {
        self.mem.len() * 16
    }
}

fn heap_over(arena: &mut Arena) -> Heap {
    let mut h = Heap::empty();
    unsafe { h.add_region(arena.start(), arena.len()) }.unwrap();
    h.check().unwrap();
    h
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

/// The whole arena can be allocated again: everything was merged back.
fn assert_whole(h: &mut Heap, total: usize) {
    h.check().unwrap();
    assert_eq!(h.free(), total);
    assert_eq!(h.used(), 0);
    let p = h.alloc(layout(total, 16)).expect("one hole spans the heap");
    unsafe { h.dealloc(p, layout(total, 16)) }.unwrap();
}

#[test]
fn allocate_and_free_round_trips() {
    let mut a = Arena::new(4096);
    let mut h = heap_over(&mut a);
    assert_eq!((h.total(), h.free(), h.used()), (4096, 4096, 0));
    let p = h.alloc(layout(100, 8)).unwrap();
    assert_eq!(p.as_ptr() as usize % 16, 0, "blocks are 16-aligned");
    assert_eq!(h.used(), 112, "rounded up to whole blocks");
    h.check().unwrap();
    unsafe { h.dealloc(p, layout(100, 8)) }.unwrap();
    assert_whole(&mut h, 4096);
}

#[test]
fn zero_size_requests_get_a_block_and_give_it_back() {
    let mut a = Arena::new(256);
    let mut h = heap_over(&mut a);
    let p = h.alloc(layout(0, 1)).unwrap();
    let q = h.alloc(layout(0, 1)).unwrap();
    assert_ne!(p, q);
    assert_eq!(h.used(), 32);
    unsafe {
        h.dealloc(p, layout(0, 1)).unwrap();
        h.dealloc(q, layout(0, 1)).unwrap();
    }
    assert_whole(&mut h, 256);
}

#[test]
fn every_alignment_is_honoured_and_padding_is_reused() {
    let mut a = Arena::new(1 << 16);
    let mut h = heap_over(&mut a);
    let (lo, hi) = (a.start() as usize, a.start() as usize + a.len());
    let mut live = Vec::new();
    for shift in 0..=12 {
        for size in [1usize, 7, 16, 17, 100, 333] {
            let l = layout(size, 1 << shift);
            let p = h.alloc(l).expect("fits");
            let addr = p.as_ptr() as usize;
            assert_eq!(addr % (1 << shift), 0, "align {} size {size}", 1 << shift);
            assert!(addr >= lo && addr + size <= hi);
            live.push((p, l));
            h.check().unwrap();
        }
    }
    let mut spans: Vec<(usize, usize)> = live
        .iter()
        .map(|(p, l)| (p.as_ptr() as usize, p.as_ptr() as usize + l.size()))
        .collect();
    spans.sort_unstable();
    assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0), "no overlaps");
    for (p, l) in live {
        unsafe { h.dealloc(p, l) }.unwrap();
        h.check().unwrap();
    }
    assert_whole(&mut h, 1 << 16);
}

#[test]
fn exhaustion_is_reported_and_fragmentation_heals_on_free() {
    let mut a = Arena::new(64 * 16);
    let mut h = heap_over(&mut a);
    let blocks: Vec<NonNull<u8>> = (0..64).map(|_| h.alloc(layout(16, 16)).unwrap()).collect();
    assert!(h.alloc(layout(1, 1)).is_none(), "completely full");
    assert_eq!(h.free(), 0);
    // Free every other block: lots of space, no block of 32.
    for b in blocks.iter().step_by(2) {
        unsafe { h.dealloc(*b, layout(16, 16)) }.unwrap();
    }
    h.check().unwrap();
    assert_eq!(h.free(), 32 * 16);
    assert!(h.alloc(layout(32, 16)).is_none(), "fragmented");
    for b in blocks.iter().skip(1).step_by(2) {
        unsafe { h.dealloc(*b, layout(16, 16)) }.unwrap();
    }
    assert_whole(&mut h, 64 * 16);
}

#[test]
fn absurd_sizes_fail_cleanly() {
    let mut a = Arena::new(1024);
    let mut h = heap_over(&mut a);
    for size in [
        isize::MAX as usize - 16,
        isize::MAX as usize / 2,
        2048,
        1025,
    ] {
        assert!(h.alloc(layout(size, 16)).is_none(), "{size}");
    }
    assert!(
        h.alloc(layout(8, 1 << 30)).is_none(),
        "alignment beyond the heap"
    );
    h.check().unwrap();
    assert_eq!(h.free(), 1024);
}

#[test]
fn double_free_and_foreign_blocks_are_refused() {
    let mut a = Arena::new(1024);
    let mut h = heap_over(&mut a);
    let p = h.alloc(layout(64, 16)).unwrap();
    let q = h.alloc(layout(64, 16)).unwrap();
    unsafe { h.dealloc(p, layout(64, 16)) }.unwrap();
    // The same block again, and a block inside the hole it became.
    assert_eq!(
        unsafe { h.dealloc(p, layout(64, 16)) },
        Err(HeapError::Overlap)
    );
    let inside = NonNull::new((p.as_ptr() as usize + 16) as *mut u8).unwrap();
    assert_eq!(
        unsafe { h.dealloc(inside, layout(16, 16)) },
        Err(HeapError::Overlap)
    );
    h.check().unwrap();
    assert_eq!(h.free(), 1024 - 64);
    // A block that straddles the start of a hole.
    let straddle = NonNull::new((q.as_ptr() as usize + 32) as *mut u8).unwrap();
    assert_eq!(
        unsafe { h.dealloc(straddle, layout(128, 16)) },
        Err(HeapError::Overlap)
    );
    unsafe { h.dealloc(q, layout(64, 16)) }.unwrap();
    assert_whole(&mut h, 1024);
}

#[test]
fn regions_are_trimmed_merged_and_checked() {
    let mut a = Arena::new(4096);
    let base = a.start();
    let mut h = Heap::empty();
    // Unaligned at both ends: trimmed to whole blocks.
    unsafe { h.add_region(base.add(3), 1000) }.unwrap();
    assert_eq!(h.total(), 976, "from 16 to 992");
    // Too small, and wrapping the address space.
    assert_eq!(
        unsafe { h.add_region(base.add(2000), 15) },
        Err(HeapError::BadRegion)
    );
    assert_eq!(
        unsafe { h.add_region(base.add(2001), 20) },
        Err(HeapError::BadRegion)
    );
    assert_eq!(
        unsafe { h.add_region(usize::MAX as *mut u8, 100) },
        Err(HeapError::BadRegion)
    );
    // Overlapping what the heap has.
    assert_eq!(
        unsafe { h.add_region(base.add(512), 1024) },
        Err(HeapError::Overlap)
    );
    // Adjacent regions merge into one hole.
    unsafe { h.add_region(base.add(992), 1008) }.unwrap();
    h.check().unwrap();
    let whole = h.total();
    assert_eq!(whole, 976 + 1008);
    let p = h.alloc(layout(whole, 16)).expect("merged into one block");
    unsafe { h.dealloc(p, layout(whole, 16)) }.unwrap();
    // A gap, then a region beyond it: two holes, no merge.
    unsafe { h.add_region(base.add(3000), 512) }.unwrap();
    h.check().unwrap();
    assert!(h.alloc(layout(whole + 16, 16)).is_none());
}

#[test]
fn random_operations_keep_every_invariant() {
    for seed in 1..=6u64 {
        let mut rng = Rng(seed * 0x9E37_79B9);
        let mut a = Arena::new(1 << 17);
        let mut h = heap_over(&mut a);
        let total = h.total();
        let (lo, hi) = (a.start() as usize, a.start() as usize + a.len());
        let mut live: Vec<(NonNull<u8>, Layout, u8)> = Vec::new();
        for step in 0..40_000 {
            if live.is_empty() || (rng.below(100) < 55 && live.len() < 400) {
                let align = 1usize << rng.below(9);
                let size = match rng.below(10) {
                    0 => 0,
                    1..=6 => rng.below(200) as usize,
                    7 | 8 => rng.below(3000) as usize,
                    _ => rng.below(9000) as usize,
                };
                let l = layout(size, align);
                if let Some(p) = h.alloc(l) {
                    let addr = p.as_ptr() as usize;
                    assert_eq!(addr % align.max(GRAIN), 0);
                    assert!(addr >= lo && addr + size.max(1) <= hi);
                    let tag = (step % 251) as u8 + 1;
                    unsafe { std::ptr::write_bytes(p.as_ptr(), tag, size) };
                    live.push((p, l, tag));
                }
            } else {
                let i = rng.below(live.len() as u64) as usize;
                let (p, l, tag) = live.swap_remove(i);
                let bytes = unsafe { std::slice::from_raw_parts(p.as_ptr(), l.size()) };
                assert!(bytes.iter().all(|b| *b == tag), "contents were overwritten");
                unsafe { h.dealloc(p, l) }.unwrap();
            }
            if step % 97 == 0 {
                h.check().unwrap();
            }
        }
        // Blocks never overlap and all still hold their contents.
        let mut spans: Vec<(usize, usize)> = live
            .iter()
            .map(|(p, l, _)| (p.as_ptr() as usize, p.as_ptr() as usize + l.size().max(1)))
            .collect();
        spans.sort_unstable();
        assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0), "seed {seed}");
        for (p, l, tag) in live.drain(..) {
            let bytes = unsafe { std::slice::from_raw_parts(p.as_ptr(), l.size()) };
            assert!(bytes.iter().all(|b| *b == tag));
            unsafe { h.dealloc(p, l) }.unwrap();
        }
        assert_whole(&mut h, total);
    }
}

static GROWN: AtomicUsize = AtomicUsize::new(0);
static LAST_MIN: AtomicUsize = AtomicUsize::new(0);

/// A hook that hands out a fresh 64 KiB region each time, and remembers what
/// was asked for.
unsafe fn grow_64k(min: usize) -> Option<(*mut u8, usize)> {
    GROWN.fetch_add(1, Ordering::SeqCst);
    LAST_MIN.store(min, Ordering::SeqCst);
    let region = Box::leak(vec![0u128; 4096].into_boxed_slice());
    Some((region.as_mut_ptr().cast(), 4096 * 16))
}

unsafe fn grow_never(_min: usize) -> Option<(*mut u8, usize)> {
    None
}

unsafe fn grow_too_small(_min: usize) -> Option<(*mut u8, usize)> {
    let region = Box::leak(vec![0u128; 2].into_boxed_slice());
    Some((region.as_mut_ptr().cast(), 32))
}

#[test]
fn locked_heap_serves_the_global_allocator_interface() {
    let mut a = Arena::new(1 << 16);
    let heap = LockedHeap::empty();
    assert!(
        unsafe { heap.alloc(layout(8, 8)) }.is_null(),
        "no memory yet"
    );
    unsafe { heap.add_region(a.start(), a.len()) }.unwrap();
    let l = layout(1000, 64);
    let p = unsafe { heap.alloc(l) };
    assert!(!p.is_null() && (p as usize).is_multiple_of(64));
    assert_eq!(heap.stats().0, 1008);
    unsafe { heap.dealloc(p, l) };
    unsafe { heap.dealloc(std::ptr::null_mut(), l) }; // a null free is ignored
    assert_eq!(heap.stats(), (0, 1 << 16));
    heap.check().unwrap();
    // A double free leaks nothing and corrupts nothing.
    let p = unsafe { heap.alloc(l) };
    unsafe { heap.dealloc(p, l) };
    unsafe { heap.dealloc(p, l) };
    heap.check().unwrap();
    assert_eq!(heap.stats(), (0, 1 << 16));
}

#[test]
fn the_heap_grows_through_its_hook_only_when_needed() {
    let heap = LockedHeap::empty();
    let hook: GrowHook = grow_64k;
    heap.set_grow_hook(Some(hook));
    let before = GROWN.load(Ordering::SeqCst);
    let p = unsafe { heap.alloc(layout(100, 16)) };
    assert!(!p.is_null());
    assert_eq!(
        GROWN.load(Ordering::SeqCst),
        before + 1,
        "first request grows"
    );
    assert_eq!(
        LAST_MIN.load(Ordering::SeqCst),
        112 + 16,
        "block plus alignment slack"
    );
    let q = unsafe { heap.alloc(layout(100, 16)) };
    assert!(!q.is_null());
    assert_eq!(
        GROWN.load(Ordering::SeqCst),
        before + 1,
        "room left: no growth"
    );
    // Bigger than one grant: the hook is asked again, but the grant is too small.
    let big = unsafe { heap.alloc(layout(1 << 20, 16)) };
    assert!(big.is_null());
    assert_eq!(LAST_MIN.load(Ordering::SeqCst), (1 << 20) + 16);
    heap.check().unwrap();
    heap.set_grow_hook(Some(grow_never));
    assert!(unsafe { heap.alloc(layout(1 << 20, 16)) }.is_null());
    heap.set_grow_hook(Some(grow_too_small));
    assert!(
        unsafe { heap.alloc(layout(1 << 20, 16)) }.is_null(),
        "grant too small"
    );
    heap.check().unwrap();
    unsafe {
        heap.dealloc(p, layout(100, 16));
        heap.dealloc(q, layout(100, 16));
    }
}

#[test]
fn concurrent_use_through_the_lock() {
    let mut a = Arena::new(1 << 20);
    let heap = LockedHeap::empty();
    unsafe { heap.add_region(a.start(), a.len()) }.unwrap();
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let heap = &heap;
            s.spawn(move || {
                let mut rng = Rng(0xABCD + t);
                let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
                for step in 0..30_000u32 {
                    if live.len() < 50 && (live.is_empty() || rng.below(100) < 55) {
                        let l = layout(1 + rng.below(700) as usize, 1 << rng.below(7));
                        let p = unsafe { heap.alloc(l) };
                        if p.is_null() {
                            continue;
                        }
                        let tag = ((t as u8) << 6) | ((step % 63) as u8 + 1);
                        unsafe { std::ptr::write_bytes(p, tag, l.size()) };
                        live.push((p, l, tag));
                    } else {
                        let i = rng.below(live.len() as u64) as usize;
                        let (p, l, tag) = live.swap_remove(i);
                        let bytes = unsafe { std::slice::from_raw_parts(p, l.size()) };
                        assert!(bytes.iter().all(|b| *b == tag), "another thread wrote here");
                        unsafe { heap.dealloc(p, l) };
                    }
                }
                for (p, l, _) in live {
                    unsafe { heap.dealloc(p, l) };
                }
            });
        }
    });
    heap.check().unwrap();
    assert_eq!(heap.stats(), (0, 1 << 20));
}
