//! Per-CPU virtual memory layout: kernel stack, IST stacks and per-CPU data.
//!
//! Each CPU gets one slot of `stride` bytes, slots packed from the window
//! start. Inside a slot, from low to high addresses:
//!
//! ```text
//! [guard][kernel stack] ([guard][IST stack k])*  [per-CPU data]
//! ```
//!
//! Stacks grow down, so every stack has an unmapped guard directly below it;
//! the data area of CPU `i` is followed by the first guard of CPU `i+1` (or
//! the end of the used span). Guards are never part of a mapped region.
//! Addresses are checked for 48-bit canonical form; LA57 is not supported.

#![forbid(unsafe_code)]

use crate::{MAX_CPUS, PAGE_SIZE};

/// Maximum IST entries in the x86-64 TSS.
pub const MAX_IST: usize = 7;

/// Half-open virtual range `[start, start + len)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VaRange {
    /// First byte.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
}

impl VaRange {
    /// Creates a range.
    pub const fn new(start: u64, len: u64) -> Self {
        Self { start, len }
    }

    /// One past the last byte, `None` on overflow.
    pub const fn end(&self) -> Option<u64> {
        self.start.checked_add(self.len)
    }

    /// Whether the ranges share at least one byte (empty ranges never do).
    /// Ranges whose end overflows are treated as reaching `u64::MAX`.
    pub fn overlaps(&self, other: &VaRange) -> bool {
        if self.len == 0 || other.len == 0 {
            return false;
        }
        let a_last = self.start.saturating_add(self.len - 1);
        let b_last = other.start.saturating_add(other.len - 1);
        self.start <= b_last && other.start <= a_last
    }

    /// Whether `addr` lies in the range.
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr - self.start < self.len
    }
}

/// Sizes of the per-CPU regions, in bytes; all must be page multiples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerCpuSpec {
    /// Kernel stack (non-zero).
    pub stack_size: u64,
    /// Guard below every stack (at least one page).
    pub guard_size: u64,
    /// Per-CPU data area (non-zero).
    pub data_size: u64,
    /// Number of IST stacks (`0..=7`).
    pub ist_count: usize,
    /// Size of each IST stack (non-zero if `ist_count > 0`).
    pub ist_size: u64,
}

/// Which spec field or input a [`LayoutError`] refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutField {
    /// Window start.
    WindowStart,
    /// Window length.
    WindowLen,
    /// `stack_size`.
    Stack,
    /// `guard_size`.
    Guard,
    /// `data_size`.
    Data,
    /// `ist_size`.
    Ist,
}

/// Why a layout was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// `cpu_count == 0`.
    NoCpus,
    /// `cpu_count > MAX_CPUS`.
    TooManyCpus(usize),
    /// More than [`MAX_IST`] IST stacks.
    TooManyIst(usize),
    /// A size or address is not a page multiple.
    Misaligned(LayoutField),
    /// A required size is zero.
    ZeroSize(LayoutField),
    /// Arithmetic overflow while sizing the layout.
    Overflow,
    /// The window cannot hold all slots.
    WindowTooSmall {
        /// Bytes needed.
        needed: u64,
        /// Bytes available.
        available: u64,
    },
    /// The window is not in one canonical half of the 48-bit address space.
    NonCanonical,
    /// The used span intersects caller-reserved range `index`.
    OverlapsReserved(usize),
}

/// Regions of one CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuRegions {
    /// Unmapped guard below the kernel stack.
    pub stack_guard: VaRange,
    /// Kernel stack.
    pub stack: VaRange,
    /// Unmapped guards below each IST stack (`ist_count` valid entries).
    pub ist_guards: [VaRange; MAX_IST],
    /// IST stacks (`ist_count` valid entries; TSS IST index = position + 1).
    pub ist: [VaRange; MAX_IST],
    /// Number of valid IST entries.
    pub ist_count: usize,
    /// Per-CPU data area.
    pub data: VaRange,
}

impl CpuRegions {
    /// Initial RSP of the kernel stack (16-byte aligned).
    pub fn stack_top(&self) -> u64 {
        self.stack.start + self.stack.len
    }

    /// Initial RSP for IST stack `k` (0-based), if present.
    pub fn ist_top(&self, k: usize) -> Option<u64> {
        (k < self.ist_count).then(|| self.ist[k].start + self.ist[k].len)
    }
}

/// A validated per-CPU layout; regions are computed on demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerCpuLayout {
    base: u64,
    stride: u64,
    cpu_count: usize,
    spec: PerCpuSpec,
}

fn canonical48(addr: u64) -> bool {
    let top = addr >> 47;
    top == 0 || top == (1 << 17) - 1
}

fn page_multiple(v: u64, field: LayoutField) -> Result<(), LayoutError> {
    if v.is_multiple_of(PAGE_SIZE) {
        Ok(())
    } else {
        Err(LayoutError::Misaligned(field))
    }
}

impl PerCpuLayout {
    /// Validates `spec` and places `cpu_count` slots at the start of
    /// `window`, which must be canonical, page aligned, large enough, and
    /// whose used part must not intersect any of `reserved`.
    pub fn new(
        window: VaRange,
        cpu_count: usize,
        spec: PerCpuSpec,
        reserved: &[VaRange],
    ) -> Result<Self, LayoutError> {
        if cpu_count == 0 {
            return Err(LayoutError::NoCpus);
        }
        if cpu_count > MAX_CPUS {
            return Err(LayoutError::TooManyCpus(cpu_count));
        }
        if spec.ist_count > MAX_IST {
            return Err(LayoutError::TooManyIst(spec.ist_count));
        }
        page_multiple(window.start, LayoutField::WindowStart)?;
        page_multiple(window.len, LayoutField::WindowLen)?;
        page_multiple(spec.stack_size, LayoutField::Stack)?;
        page_multiple(spec.guard_size, LayoutField::Guard)?;
        page_multiple(spec.data_size, LayoutField::Data)?;
        page_multiple(spec.ist_size, LayoutField::Ist)?;
        for (size, field) in [
            (spec.stack_size, LayoutField::Stack),
            (spec.guard_size, LayoutField::Guard),
            (spec.data_size, LayoutField::Data),
        ] {
            if size == 0 {
                return Err(LayoutError::ZeroSize(field));
            }
        }
        if spec.ist_count > 0 && spec.ist_size == 0 {
            return Err(LayoutError::ZeroSize(LayoutField::Ist));
        }
        let window_end = window.end().ok_or(LayoutError::Overflow)?;
        if window.len == 0 || !canonical48(window.start) || !canonical48(window_end - 1) {
            return Err(LayoutError::NonCanonical);
        }
        if window.start >> 47 != (window_end - 1) >> 47 {
            return Err(LayoutError::NonCanonical);
        }

        let guarded_stack = spec
            .guard_size
            .checked_add(spec.stack_size)
            .ok_or(LayoutError::Overflow)?;
        let guarded_ist = spec
            .guard_size
            .checked_add(spec.ist_size)
            .ok_or(LayoutError::Overflow)?;
        let stride = guarded_ist
            .checked_mul(spec.ist_count as u64)
            .and_then(|ist| ist.checked_add(guarded_stack))
            .and_then(|s| s.checked_add(spec.data_size))
            .ok_or(LayoutError::Overflow)?;
        let needed = stride
            .checked_mul(cpu_count as u64)
            .ok_or(LayoutError::Overflow)?;
        if needed > window.len {
            return Err(LayoutError::WindowTooSmall {
                needed,
                available: window.len,
            });
        }
        let used = VaRange::new(window.start, needed);
        if let Some(index) = reserved.iter().position(|r| r.overlaps(&used)) {
            return Err(LayoutError::OverlapsReserved(index));
        }
        Ok(Self {
            base: window.start,
            stride,
            cpu_count,
            spec,
        })
    }

    /// Number of CPUs laid out.
    pub fn cpu_count(&self) -> usize {
        self.cpu_count
    }

    /// Bytes per CPU slot.
    pub fn stride(&self) -> u64 {
        self.stride
    }

    /// The part of the window actually used by all slots.
    pub fn used(&self) -> VaRange {
        // Cannot overflow: checked in `new`.
        VaRange::new(self.base, self.stride * self.cpu_count as u64)
    }

    /// Regions of CPU `cpu`, `None` if out of range.
    pub fn cpu(&self, cpu: usize) -> Option<CpuRegions> {
        if cpu >= self.cpu_count {
            return None;
        }
        let s = &self.spec;
        // All arithmetic below stays inside `used()`, validated in `new`.
        let mut at = self.base + self.stride * cpu as u64;
        let mut take = |len: u64| {
            let r = VaRange::new(at, len);
            at += len;
            r
        };
        let stack_guard = take(s.guard_size);
        let stack = take(s.stack_size);
        let mut ist_guards = [VaRange::default(); MAX_IST];
        let mut ist = [VaRange::default(); MAX_IST];
        for (guard, stack) in ist_guards.iter_mut().zip(ist.iter_mut()).take(s.ist_count) {
            *guard = take(s.guard_size);
            *stack = take(s.ist_size);
        }
        let data = take(s.data_size);
        Some(CpuRegions {
            stack_guard,
            stack,
            ist_guards,
            ist,
            ist_count: s.ist_count,
            data,
        })
    }
}
