//! A first-fit heap over regions the caller hands it.
//!
//! Free memory is an address-ordered list of holes, each stored in the free
//! memory itself. Every block is a multiple of 16 bytes and 16-aligned, so a
//! hole is always big enough for its own header (two words) and the padding
//! in front of or behind an allocation is either zero or a usable hole.
//! Freeing merges a block with both neighbours; a block that overlaps a hole
//! (a double free) is refused, not linked. [`Heap::check`] walks the list and
//! verifies the invariants; the tests call it after every operation.
//!
//! [`LockedHeap`] wraps a heap in a spin lock as a `GlobalAlloc` and can ask
//! a hook for more memory when a request does not fit: in a NANOX process the
//! hook maps a MemoryObject (`memory_map`).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};

/// Every block size and address is a multiple of this.
pub const GRAIN: usize = 16;

#[repr(C)]
struct Hole {
    size: usize,
    next: Option<NonNull<Hole>>,
}

const _: () = assert!(core::mem::size_of::<Hole>() == GRAIN);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapError {
    /// The block overlaps free memory (a double free) or lies inside a hole.
    Overlap,
    /// The region is too small to hold a hole, or wraps the address space.
    BadRegion,
}

fn align_up(x: usize, a: usize) -> Option<usize> {
    x.checked_add(a - 1).map(|v| v & !(a - 1))
}

fn round_size(size: usize) -> Option<usize> {
    align_up(size.max(1), GRAIN)
}

#[derive(Default)]
pub struct Heap {
    first: Option<NonNull<Hole>>,
    free: usize,
    total: usize,
}

// SAFETY: the heap owns the memory its holes live in; moving the owner to
// another thread moves that ownership with it.
unsafe impl Send for Heap {}

impl Heap {
    pub const fn empty() -> Self {
        Self {
            first: None,
            free: 0,
            total: 0,
        }
    }

    /// Bytes handed to the heap, rounded to whole blocks.
    pub fn total(&self) -> usize {
        self.total
    }

    pub fn free(&self) -> usize {
        self.free
    }

    pub fn used(&self) -> usize {
        self.total - self.free
    }

    /// Adds `[start, start + len)` to the heap. The range is shrunk to whole
    /// 16-byte blocks; a range that overlaps memory the heap already has is
    /// refused.
    ///
    /// # Safety
    /// The range must be valid, writable memory that nothing else uses and
    /// that stays valid and untouched by others for as long as the heap
    /// lives.
    pub unsafe fn add_region(&mut self, start: *mut u8, len: usize) -> Result<(), HeapError> {
        let a = align_up(start as usize, GRAIN).ok_or(HeapError::BadRegion)?;
        let end = (start as usize)
            .checked_add(len)
            .ok_or(HeapError::BadRegion)?
            & !(GRAIN - 1);
        if end <= a || end - a < GRAIN {
            return Err(HeapError::BadRegion);
        }
        // SAFETY: the caller vouches for the range; it is 16-aligned and at
        // least one block long.
        unsafe { self.insert(a, end - a) }?;
        self.total += end - a;
        Ok(())
    }

    /// Links the block `[addr, addr + size)` into the free list, merging it
    /// with its neighbours.
    ///
    /// # Safety
    /// `addr` is 16-aligned, `size` a positive multiple of 16, and the block
    /// is writable memory owned by the heap.
    unsafe fn insert(&mut self, addr: usize, size: usize) -> Result<(), HeapError> {
        let end = addr.checked_add(size).ok_or(HeapError::BadRegion)?;
        let mut prev: Option<NonNull<Hole>> = None;
        let mut cur = self.first;
        // SAFETY: every hole in the list was written by this heap and lies in
        // memory it owns.
        unsafe {
            while let Some(h) = cur {
                let h_addr = h.as_ptr() as usize;
                if h_addr >= end {
                    break;
                }
                if h_addr + h.as_ref().size > addr {
                    return Err(HeapError::Overlap);
                }
                prev = cur;
                cur = h.as_ref().next;
            }
            // `prev` ends before addr, `cur` starts at or after end.
            let (mut start, mut len, mut next) = (addr, size, cur);
            if let Some(n) = cur {
                if n.as_ptr() as usize == end {
                    len += n.as_ref().size;
                    next = n.as_ref().next;
                }
            }
            if let Some(mut p) = prev {
                if p.as_ptr() as usize + p.as_ref().size == addr {
                    // Merge into the previous hole.
                    p.as_mut().size += len;
                    p.as_mut().next = next;
                    self.free += size;
                    return Ok(());
                }
            }
            let hole = start as *mut Hole;
            ptr::write(hole, Hole { size: len, next });
            start = hole as usize;
            let node = NonNull::new_unchecked(start as *mut Hole);
            match prev {
                Some(mut p) => p.as_mut().next = Some(node),
                None => self.first = Some(node),
            }
        }
        self.free += size;
        Ok(())
    }

    /// Allocates a block for `layout`, or `None` if nothing fits. Zero-size
    /// requests get the smallest block.
    pub fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let size = round_size(layout.size())?;
        let align = layout.align().max(GRAIN);
        let mut link: *mut Option<NonNull<Hole>> = &mut self.first;
        // SAFETY: the list holds only holes this heap wrote; `link` always
        // points at the list head or at the next field of a hole.
        unsafe {
            while let Some(h) = *link {
                let addr = h.as_ptr() as usize;
                let hole_end = addr + h.as_ref().size;
                if let Some(start) = align_up(addr, align) {
                    if let Some(end) = start.checked_add(size) {
                        if end <= hole_end {
                            let next = h.as_ref().next;
                            // Both paddings are multiples of 16: zero or a
                            // hole that fits its header.
                            let mut after = next;
                            if hole_end > end {
                                let back = end as *mut Hole;
                                ptr::write(
                                    back,
                                    Hole {
                                        size: hole_end - end,
                                        next: after,
                                    },
                                );
                                after = Some(NonNull::new_unchecked(back));
                            }
                            if start > addr {
                                let front = addr as *mut Hole;
                                ptr::write(
                                    front,
                                    Hole {
                                        size: start - addr,
                                        next: after,
                                    },
                                );
                                after = Some(NonNull::new_unchecked(front));
                            }
                            *link = after;
                            self.free -= size;
                            return NonNull::new(start as *mut u8);
                        }
                    }
                }
                link = &mut (*h.as_ptr()).next;
            }
        }
        None
    }

    /// Returns a block to the heap.
    ///
    /// # Safety
    /// `ptr` must have come from [`Heap::alloc`] on this heap with the same
    /// `layout`, and must not be used afterwards. A block that overlaps free
    /// memory is refused with an error; other misuse is undefined behaviour.
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) -> Result<(), HeapError> {
        let size = round_size(layout.size()).ok_or(HeapError::BadRegion)?;
        // SAFETY: by the contract above the block is ours and unused.
        unsafe { self.insert(ptr.as_ptr() as usize, size) }
    }

    /// Verifies the free list: address order, no overlap, no adjacent holes,
    /// every hole a positive multiple of 16 at a 16-aligned address, and the
    /// free-byte count.
    pub fn check(&self) -> Result<(), &'static str> {
        let mut sum = 0usize;
        let mut prev_end = 0usize;
        let mut cur = self.first;
        // SAFETY: as above, the list holds only holes this heap wrote.
        unsafe {
            while let Some(h) = cur {
                let addr = h.as_ptr() as usize;
                let size = h.as_ref().size;
                if !addr.is_multiple_of(GRAIN) {
                    return Err("hole not aligned");
                }
                if size == 0 || !size.is_multiple_of(GRAIN) {
                    return Err("hole size not a positive multiple of 16");
                }
                if prev_end != 0 && addr < prev_end {
                    return Err("holes overlap or are out of order");
                }
                if prev_end != 0 && addr == prev_end {
                    return Err("adjacent holes were not merged");
                }
                prev_end = addr + size;
                sum += size;
                cur = h.as_ref().next;
            }
        }
        if sum != self.free {
            return Err("free byte count out of step with the list");
        }
        if self.free > self.total {
            return Err("more free than total");
        }
        Ok(())
    }
}

/// Supplies more memory when the heap runs out: a region at least `min`
/// bytes long, or `None`. Must return memory valid for [`Heap::add_region`].
pub type GrowHook = unsafe fn(min: usize) -> Option<(*mut u8, usize)>;

/// A spin-locked heap usable as the global allocator.
pub struct LockedHeap {
    lock: AtomicBool,
    heap: UnsafeCell<Heap>,
    grow: UnsafeCell<Option<GrowHook>>,
}

// SAFETY: every access to the cells is under `lock`.
unsafe impl Sync for LockedHeap {}

impl Default for LockedHeap {
    fn default() -> Self {
        Self::empty()
    }
}

impl LockedHeap {
    pub const fn empty() -> Self {
        Self {
            lock: AtomicBool::new(false),
            heap: UnsafeCell::new(Heap::empty()),
            grow: UnsafeCell::new(None),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Heap, Option<GrowHook>) -> R) -> R {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: we hold the lock.
        let r = unsafe { f(&mut *self.heap.get(), *self.grow.get()) };
        self.lock.store(false, Ordering::Release);
        r
    }

    /// Adds an initial region.
    ///
    /// # Safety
    /// As [`Heap::add_region`].
    pub unsafe fn add_region(&self, start: *mut u8, len: usize) -> Result<(), HeapError> {
        // SAFETY: forwarded contract.
        self.with(|h, _| unsafe { h.add_region(start, len) })
    }

    /// Sets the hook asked for more memory.
    pub fn set_grow_hook(&self, hook: Option<GrowHook>) {
        self.with(|_, _| {
            // SAFETY: under the lock.
            unsafe { *self.grow.get() = hook }
        })
    }

    /// Bytes in use and bytes free.
    pub fn stats(&self) -> (usize, usize) {
        self.with(|h, _| (h.used(), h.free()))
    }

    pub fn check(&self) -> Result<(), &'static str> {
        self.with(|h, _| h.check())
    }
}

// SAFETY: blocks are disjoint, aligned as asked and live until freed; the
// lock makes the list updates atomic with respect to other threads.
unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|h, grow| {
            if let Some(p) = h.alloc(layout) {
                return p.as_ptr();
            }
            let Some(grow) = grow else {
                return ptr::null_mut();
            };
            // Enough for the block plus the padding alignment may need.
            let want = round_size(layout.size())
                .and_then(|s| s.checked_add(layout.align().max(GRAIN)))
                .unwrap_or(usize::MAX);
            // SAFETY: the hook promises valid memory.
            let region = unsafe { grow(want) };
            match region {
                // SAFETY: as promised by the hook.
                Some((start, len)) if unsafe { h.add_region(start, len) }.is_ok() => {
                    h.alloc(layout).map_or(ptr::null_mut(), NonNull::as_ptr)
                }
                _ => ptr::null_mut(),
            }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(p) = NonNull::new(ptr) {
            // A refused block is a double free; leaking it is the safe answer.
            // SAFETY: by the GlobalAlloc contract.
            let _ = self.with(|h, _| unsafe { h.dealloc(p, layout) });
        }
    }
}
