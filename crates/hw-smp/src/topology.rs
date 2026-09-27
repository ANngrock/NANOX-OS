//! Logical CPU table built from firmware processor-local APIC entries.
//!
//! Numbering rule: the BSP is logical CPU 0; every other *enabled* CPU follows
//! in firmware order (`1..enabled_count`); CPUs that are disabled but
//! *online capable* (hot-pluggable, ACPI 6.3 MADT flag bit 1) come after all
//! enabled ones. Entries that are neither enabled nor online capable describe
//! no usable processor: they are counted as ignored and take no part in the
//! duplicate check (firmware commonly lists such placeholders).

#![forbid(unsafe_code)]

use crate::MAX_CPUS;

/// Highest APIC ID addressable in xAPIC physical destination mode. ID `0xFF`
/// is the xAPIC broadcast destination, so it and anything above require
/// x2APIC mode.
pub const XAPIC_MAX_ID: u32 = 0xFE;

/// x2APIC broadcast destination; never a valid processor ID.
pub const X2APIC_BROADCAST: u32 = 0xFFFF_FFFF;

/// One processor-local APIC entry as reported by firmware (MADT type 0 or 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApicEntry {
    /// Local APIC ID (8-bit for type 0, 32-bit for type 9).
    pub apic_id: u32,
    /// MADT flag bit 0: the processor is usable now.
    pub enabled: bool,
    /// MADT flag bit 1: the processor may be brought online later.
    pub online_capable: bool,
}

impl ApicEntry {
    /// Decodes the MADT "Local APIC Flags" field.
    pub const fn from_madt_flags(apic_id: u32, flags: u32) -> Self {
        Self {
            apic_id,
            enabled: flags & 1 != 0,
            online_capable: flags & 2 != 0,
        }
    }
}

/// Whether a logical CPU is usable at boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuPresence {
    /// Enabled by firmware; a start-up candidate.
    Enabled,
    /// Disabled but online capable (hot-plug slot); never started at boot.
    OnlineCapable,
}

/// One logical CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    /// Local APIC ID.
    pub apic_id: u32,
    /// Enabled or online capable.
    pub presence: CpuPresence,
    /// The ID cannot be addressed in xAPIC mode (`apic_id > XAPIC_MAX_ID`).
    pub needs_x2apic: bool,
}

impl CpuInfo {
    /// Placeholder used to initialise caller buffers.
    pub const EMPTY: Self = Self {
        apic_id: 0,
        presence: CpuPresence::OnlineCapable,
        needs_x2apic: false,
    };
}

impl Default for CpuInfo {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Reasons a topology cannot be built. No partial table is returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologyError {
    /// The caller buffer is empty.
    NoCapacity,
    /// More usable CPUs than `min(buffer length, MAX_CPUS)`.
    Overflow {
        /// Effective capacity that was exceeded.
        capacity: usize,
    },
    /// Two usable entries share an APIC ID.
    DuplicateApicId(u32),
    /// The BSP APIC ID is not listed.
    BspMissing(u32),
    /// The BSP is listed but not enabled.
    BspNotEnabled(u32),
    /// A usable entry carries the x2APIC broadcast ID.
    InvalidApicId(u32),
}

/// Logical CPU table stored in a caller-provided buffer.
#[derive(Clone, Copy, Debug)]
pub struct Topology<'a> {
    cpus: &'a [CpuInfo],
    enabled: usize,
    ignored: usize,
}

/// Builds the logical CPU table. `buf` bounds the result together with
/// [`MAX_CPUS`]; the table occupies `buf[..topology.len()]`.
pub fn build_topology<'a, I>(
    entries: I,
    bsp_apic_id: u32,
    buf: &'a mut [CpuInfo],
) -> Result<Topology<'a>, TopologyError>
where
    I: IntoIterator<Item = ApicEntry>,
{
    let cap = buf.len().min(MAX_CPUS);
    if cap == 0 {
        return Err(TopologyError::NoCapacity);
    }
    let buf: &'a mut [CpuInfo] = &mut buf[..cap];
    // Slot 0 is reserved for the BSP. Enabled APs grow upwards from 1,
    // online-capable CPUs grow downwards from the end (newest lowest).
    let mut front = 1;
    let mut back = cap;
    let mut bsp_seen = false;
    let mut ignored = 0;

    for entry in entries {
        if !entry.enabled && !entry.online_capable {
            ignored += 1;
            continue;
        }
        let id = entry.apic_id;
        if id == X2APIC_BROADCAST {
            return Err(TopologyError::InvalidApicId(id));
        }
        let info = CpuInfo {
            apic_id: id,
            presence: if entry.enabled {
                CpuPresence::Enabled
            } else {
                CpuPresence::OnlineCapable
            },
            needs_x2apic: id > XAPIC_MAX_ID,
        };
        if id == bsp_apic_id {
            if bsp_seen {
                return Err(TopologyError::DuplicateApicId(id));
            }
            if !entry.enabled {
                return Err(TopologyError::BspNotEnabled(id));
            }
            buf[0] = info;
            bsp_seen = true;
            continue;
        }
        // Non-BSP IDs cannot collide with slot 0 (it only ever holds the BSP ID).
        if buf[1..front]
            .iter()
            .chain(buf[back..].iter())
            .any(|c| c.apic_id == id)
        {
            return Err(TopologyError::DuplicateApicId(id));
        }
        if front == back {
            return Err(TopologyError::Overflow { capacity: cap });
        }
        if entry.enabled {
            buf[front] = info;
            front += 1;
        } else {
            back -= 1;
            buf[back] = info;
        }
    }
    if !bsp_seen {
        return Err(TopologyError::BspMissing(bsp_apic_id));
    }
    let online_capable = cap - back;
    buf[back..].reverse();
    buf[front..].rotate_left(back - front);
    let len = front + online_capable;
    let buf: &'a [CpuInfo] = buf;
    Ok(Topology {
        cpus: &buf[..len],
        enabled: front,
        ignored,
    })
}

impl<'a> Topology<'a> {
    /// All logical CPUs; index = logical CPU number.
    pub fn cpus(&self) -> &'a [CpuInfo] {
        self.cpus
    }

    /// Number of logical CPUs (enabled + online capable).
    pub fn len(&self) -> usize {
        self.cpus.len()
    }

    /// Always false for a built topology (the BSP is present).
    pub fn is_empty(&self) -> bool {
        self.cpus.is_empty()
    }

    /// Logical CPU `index`.
    pub fn cpu(&self, index: usize) -> Option<&'a CpuInfo> {
        self.cpus.get(index)
    }

    /// The BSP (logical CPU 0).
    pub fn bsp(&self) -> &'a CpuInfo {
        &self.cpus[0]
    }

    /// Enabled CPUs including the BSP; they are logical `0..enabled_count()`.
    pub fn enabled_count(&self) -> usize {
        self.enabled
    }

    /// Disabled-but-online-capable CPUs; logical `enabled_count()..len()`.
    pub fn online_capable_count(&self) -> usize {
        self.cpus.len() - self.enabled
    }

    /// Entries that were neither enabled nor online capable.
    pub fn ignored_count(&self) -> usize {
        self.ignored
    }

    /// Logical index of the CPU with `apic_id`.
    pub fn index_of(&self, apic_id: u32) -> Option<usize> {
        self.cpus.iter().position(|c| c.apic_id == apic_id)
    }

    /// Whether any listed CPU requires x2APIC mode to be addressed.
    pub fn requires_x2apic(&self) -> bool {
        self.cpus.iter().any(|c| c.needs_x2apic)
    }
}
