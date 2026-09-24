use crate::{fatal, uefi};
use boot_protocol::{PAGE_SIZE, ReservedRange};
use core::ptr;

#[derive(Clone, Copy)]
pub struct Pages {
    pub base: u64,
    pub count: usize,
}

impl Pages {
    pub fn allocate(bs: &uefi::BootServices, count: usize, executable: bool) -> Self {
        if count == 0 || count > 32768 {
            fatal("allocation-size");
        }
        // AllocateMaxAddress guarantees physical pointers are canonical and
        // transition pages remain in the lower identity-mapped 4 GiB window.
        let mut base = 0xffff_ffffu64;
        // SAFETY: table is validated at entry and still in the boot phase;
        // output points to an aligned live u64 exclusively owned by this call.
        let status =
            unsafe { (bs.allocate_pages)(1, if executable { 1 } else { 2 }, count, &mut base) };
        if status != uefi::SUCCESS {
            fatal("allocate-pages");
        }
        if base == 0
            || base & 4095 != 0
            || base
                .checked_add(count as u64 * PAGE_SIZE)
                .is_none_or(|end| end > 0x1_0000_0000)
        {
            fatal("allocation-address");
        }
        // SAFETY: AllocatePages returned exclusive, identity-addressable,
        // page-aligned LoaderData/LoaderCode memory for precisely count pages.
        // It remains owned until explicit free or reserved kernel handoff.
        unsafe {
            ptr::write_bytes(base as *mut u8, 0, count * PAGE_SIZE as usize);
        }
        Self { base, count }
    }

    pub fn free(self, bs: &uefi::BootServices) {
        // SAFETY: this token denotes the complete live allocation, no borrowed
        // slice is retained by callers; this operation occurs before map exit.
        let status = unsafe { (bs.free_pages)(self.base, self.count) };
        if status != uefi::SUCCESS {
            fatal("free-pages");
        }
    }

    pub fn reserved(self, kind: u32) -> ReservedRange {
        ReservedRange {
            phys_start: self.base,
            page_count: self.count as u64,
            kind,
            reserved: 0,
        }
    }
}

pub struct PageTables {
    pub pages: Pages,
    used: usize,
}

impl PageTables {
    pub fn new(bs: &uefi::BootServices) -> Self {
        Self {
            pages: Pages::allocate(bs, 128, false),
            used: 1,
        }
    }

    pub fn map(&mut self, virtual_start: u64, pages: Pages, writable: bool, executable: bool) {
        if virtual_start & 4095 != 0 || pages.base & 4095 != 0 || (writable && executable) {
            fatal("map-alignment-or-wx");
        }
        for page in 0..pages.count {
            let va = virtual_start
                .checked_add(page as u64 * PAGE_SIZE)
                .unwrap_or_else(|| fatal("map-overflow"));
            let pa = pages.base + page as u64 * PAGE_SIZE;
            let canonical = va <= 0x0000_7fff_ffff_ffff || va >= 0xffff_8000_0000_0000;
            if !canonical {
                fatal("noncanonical-map");
            }
            let indices = [
                ((va >> 39) & 511),
                ((va >> 30) & 511),
                ((va >> 21) & 511),
                ((va >> 12) & 511),
            ];
            let mut table = self.pages.base;
            for index in &indices[..3] {
                // SAFETY: table is either the allocated PML4 or a child
                // allocated from the same zeroed pool; index is in 0..512,
                // all entries are 8-byte aligned and only this builder writes.
                let slot = unsafe { &mut *((table as *mut u64).add(*index as usize)) };
                if *slot == 0 {
                    if self.used == self.pages.count {
                        fatal("page-table-capacity");
                    }
                    let child = self.pages.base + self.used as u64 * PAGE_SIZE;
                    self.used += 1;
                    *slot = child | 3; // Supervisor, present, parent permits RW.
                }
                table = *slot & 0x000f_ffff_ffff_f000;
            }
            // SAFETY: the final table and bounded index have the same exclusive
            // pool ownership and alignment as above; duplicate mappings fail.
            let slot = unsafe { &mut *((table as *mut u64).add(indices[3] as usize)) };
            if *slot != 0 {
                fatal("mapping-overlap");
            }
            *slot = pa | 1 | if writable { 2 } else { 0 } | if executable { 0 } else { 1 << 63 };
        }
    }
}
