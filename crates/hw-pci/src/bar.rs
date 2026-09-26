//! Base Address Register decoding and the sizing protocol.

use crate::{regs, Bdf, ConfigSpace, HeaderKind, PciError};

pub const MAX_BAR_SLOTS: usize = 6;

const IO_SPACE: u32 = 0x1;
const IO_FLAG_BITS: u32 = 0x3;
const MEM_TYPE_MASK: u32 = 0x6;
const MEM_TYPE_32: u32 = 0x0;
const MEM_TYPE_64: u32 = 0x4;
const MEM_PREFETCHABLE: u32 = 0x8;
const MEM_FLAG_BITS: u32 = 0xF;
const UPPER_HALF: u64 = 0xFFFF_FFFF_0000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Io,
    Memory32,
    Memory64,
}

impl BarKind {
    pub const fn is_memory(self) -> bool {
        !matches!(self, Self::Io)
    }
}

/// A sized BAR. `address` is the value firmware (or the caller) programmed;
/// `size` is a power of two and `address` is aligned to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub index: u8,
    pub kind: BarKind,
    pub prefetchable: bool,
    pub address: u64,
    pub size: u64,
}

impl Bar {
    /// Whether `[offset, offset + len)` lies inside the BAR window.
    pub const fn contains(&self, offset: u64, len: u64) -> bool {
        match offset.checked_add(len) {
            Some(end) => end <= self.size,
            None => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BarSlot {
    /// The register is hardwired to zero.
    #[default]
    Unimplemented,
    Bar(Bar),
    /// Upper half of the 64-bit BAR in the previous slot.
    Upper64,
}

/// All BAR slots of one function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bars {
    slots: [BarSlot; MAX_BAR_SLOTS],
    len: u8,
}

impl Bars {
    pub fn slots(&self) -> &[BarSlot] {
        self.slots.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// The BAR that starts in slot `index`, if any.
    pub fn get(&self, index: u8) -> Option<&Bar> {
        match self.slots().get(usize::from(index))? {
            BarSlot::Bar(bar) => Some(bar),
            _ => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Bar> {
        self.slots().iter().filter_map(|slot| match slot {
            BarSlot::Bar(bar) => Some(bar),
            _ => None,
        })
    }
}

/// Decode the read-only type bits of a BAR register: kind and prefetchable.
pub const fn decode_bar_type(raw: u32) -> Result<(BarKind, bool), PciError> {
    if raw & IO_SPACE != 0 {
        return Ok((BarKind::Io, false));
    }
    let prefetchable = raw & MEM_PREFETCHABLE != 0;
    match raw & MEM_TYPE_MASK {
        MEM_TYPE_32 => Ok((BarKind::Memory32, prefetchable)),
        MEM_TYPE_64 => Ok((BarKind::Memory64, prefetchable)),
        _ => Err(PciError::BarReservedType),
    }
}

const fn bar_offset(index: u8) -> u16 {
    regs::BAR0 + 4 * index as u16
}

/// Size the BAR starting in slot `index`.
///
/// Protocol: validate the slot and type bits (no writes on failure), clear
/// I/O and memory decode in the command register, write all ones, read the
/// response, write back the original BAR value(s) and command, then verify
/// both read back unchanged. Restoration happens before the response is
/// interpreted, so the function is left as found on every error path after
/// the first write, unless the device itself refuses the restore
/// ([`PciError::BarNotRestored`], [`PciError::CommandNotRestored`]).
///
/// Sizing briefly disables decode; the caller must ensure no driver is using
/// the function concurrently.
pub fn probe_bar<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
    kind: HeaderKind,
    index: u8,
) -> Result<BarSlot, PciError> {
    let slots = kind.bar_slots();
    if index >= slots {
        return Err(PciError::BarIndex);
    }
    let offset = bar_offset(index);
    let original_low = cfg.read_u32(bdf, offset);
    let (bar_kind, prefetchable) = decode_bar_type(original_low)?;
    let wide = matches!(bar_kind, BarKind::Memory64);
    if wide && index + 1 >= slots {
        return Err(PciError::Bar64InLastSlot);
    }
    let original_high = if wide {
        cfg.read_u32(bdf, offset + 4)
    } else {
        0
    };
    let command = cfg.read_u16(bdf, regs::COMMAND);

    cfg.write_u16(
        bdf,
        regs::COMMAND,
        command & !(regs::COMMAND_IO_SPACE | regs::COMMAND_MEMORY_SPACE),
    );
    cfg.write_u32(bdf, offset, u32::MAX);
    let response_low = cfg.read_u32(bdf, offset);
    let response_high = if wide {
        cfg.write_u32(bdf, offset + 4, u32::MAX);
        cfg.read_u32(bdf, offset + 4)
    } else {
        0
    };

    cfg.write_u32(bdf, offset, original_low);
    if wide {
        cfg.write_u32(bdf, offset + 4, original_high);
    }
    cfg.write_u16(bdf, regs::COMMAND, command);

    let restored_low = cfg.read_u32(bdf, offset);
    let restored_high = if wide {
        cfg.read_u32(bdf, offset + 4)
    } else {
        0
    };
    if restored_low != original_low || restored_high != original_high {
        return Err(PciError::BarNotRestored);
    }
    if cfg.read_u16(bdf, regs::COMMAND) != command {
        return Err(PciError::CommandNotRestored);
    }

    let sample = Sample {
        index,
        kind: bar_kind,
        prefetchable,
        original_low,
        original_high,
        response_low,
        response_high,
    };
    Ok(match sample.interpret()? {
        Some(bar) => BarSlot::Bar(bar),
        None => BarSlot::Unimplemented,
    })
}

/// Size every BAR slot of the function in order.
pub fn probe_bars<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
    kind: HeaderKind,
) -> Result<Bars, PciError> {
    let mut bars = Bars {
        slots: [BarSlot::Unimplemented; MAX_BAR_SLOTS],
        len: kind.bar_slots(),
    };
    let mut index = 0;
    while index < kind.bar_slots() {
        let slot = probe_bar(cfg, bdf, kind, index)?;
        if let Some(entry) = bars.slots.get_mut(usize::from(index)) {
            *entry = slot;
        }
        if let BarSlot::Bar(Bar {
            kind: BarKind::Memory64,
            ..
        }) = slot
        {
            // probe_bar rejected a 64-bit BAR in the last slot.
            index += 1;
            if let Some(entry) = bars.slots.get_mut(usize::from(index)) {
                *entry = BarSlot::Upper64;
            }
        }
        index += 1;
    }
    Ok(bars)
}

struct Sample {
    index: u8,
    kind: BarKind,
    prefetchable: bool,
    original_low: u32,
    original_high: u32,
    response_low: u32,
    response_high: u32,
}

impl Sample {
    fn interpret(&self) -> Result<Option<Bar>, PciError> {
        if self.original_low == 0 && self.response_low == 0 {
            // Hardwired to zero: a 32-bit memory encoding with nothing writable.
            return Ok(None);
        }
        // `mask` has ones for every address bit the BAR decodes, including
        // implied upper bits; `address` excludes the flag bits.
        let (mask, address) = match self.kind {
            BarKind::Io => {
                if self.response_low & IO_SPACE == 0 {
                    return Err(PciError::BarResponse);
                }
                let mut low = self.response_low & !IO_FLAG_BITS;
                if low == 0 {
                    return Err(PciError::BarResponse);
                }
                if low & 0xFFFF_0000 == 0 {
                    // 16-bit I/O decoder: upper bits are hardwired to zero.
                    low |= 0xFFFF_0000;
                }
                (
                    UPPER_HALF | u64::from(low),
                    u64::from(self.original_low & !IO_FLAG_BITS),
                )
            }
            BarKind::Memory32 => {
                self.check_memory_flags()?;
                let low = self.response_low & !MEM_FLAG_BITS;
                if low == 0 {
                    return Err(PciError::BarResponse);
                }
                (
                    UPPER_HALF | u64::from(low),
                    u64::from(self.original_low & !MEM_FLAG_BITS),
                )
            }
            BarKind::Memory64 => {
                self.check_memory_flags()?;
                let mask = (u64::from(self.response_high) << 32)
                    | u64::from(self.response_low & !MEM_FLAG_BITS);
                if mask == 0 {
                    return Err(PciError::BarResponse);
                }
                (
                    mask,
                    (u64::from(self.original_high) << 32)
                        | u64::from(self.original_low & !MEM_FLAG_BITS),
                )
            }
        };
        // mask != 0 in every arm, so span < u64::MAX and span + 1 cannot overflow.
        let span = !mask;
        if span & (span + 1) != 0 {
            // Writable bits are not one contiguous run from the top.
            return Err(PciError::BarResponse);
        }
        if address & span != 0 || address.checked_add(span).is_none() {
            return Err(PciError::BarResponse);
        }
        Ok(Some(Bar {
            index: self.index,
            kind: self.kind,
            prefetchable: self.prefetchable,
            address,
            size: span + 1,
        }))
    }

    fn check_memory_flags(&self) -> Result<(), PciError> {
        if self.response_low & MEM_FLAG_BITS != self.original_low & MEM_FLAG_BITS {
            return Err(PciError::BarResponse);
        }
        Ok(())
    }
}
