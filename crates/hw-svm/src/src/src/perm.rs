//! MSR and I/O permission maps (APM 15.10, 15.11).
//!
//! MSRPM: 8 KiB, two bits per MSR (read, then write) for three ranges of
//! 8192 MSRs: 0000_0000h at byte 0, C000_0000h at 800h, C001_0000h at
//! 1000h; MSRs outside the ranges always cause an intercept. IOPM: 12 KiB,
//! one bit per port for ports 0..FFFFh; the extra bits cover multi-byte
//! accesses that run past FFFFh.

/// Bytes of an MSR permission map.
pub const MSRPM_BYTES: usize = 8192;
/// Bytes of an I/O permission map.
pub const IOPM_BYTES: usize = 12288;

/// An MSR permission map in a caller-provided buffer.
pub struct MsrPermissionMap<'a> {
    map: &'a mut [u8; MSRPM_BYTES],
}

impl<'a> MsrPermissionMap<'a> {
    /// Starts with every MSR intercepted for read and write.
    pub fn intercept_all(map: &'a mut [u8; MSRPM_BYTES]) -> Self {
        map.fill(0xFF);
        Self { map }
    }

    /// Bit index of the read bit of `msr`, or `None` outside the ranges.
    pub fn bit(msr: u32) -> Option<usize> {
        let (base, byte) = match msr {
            0x0000_0000..=0x0000_1FFF => (0x0000_0000, 0x000),
            0xC000_0000..=0xC000_1FFF => (0xC000_0000, 0x800),
            0xC001_0000..=0xC001_1FFF => (0xC001_0000, 0x1000),
            _ => return None,
        };
        Some(byte * 8 + 2 * (msr - base) as usize)
    }

    /// Lets the guest read and/or write `msr` without an exit. Returns false
    /// for MSRs outside the map (they always exit).
    pub fn allow(&mut self, msr: u32, read: bool, write: bool) -> bool {
        let Some(b) = Self::bit(msr) else {
            return false;
        };
        for (i, allowed) in [(b, read), (b + 1, write)] {
            let (byte, mask) = (i / 8, 1u8 << (i % 8));
            if allowed {
                self.map[byte] &= !mask;
            } else {
                self.map[byte] |= mask;
            }
        }
        true
    }

    /// True if an access intercepts (MSRs outside the ranges always do).
    pub fn intercepts(&self, msr: u32, write: bool) -> bool {
        match Self::bit(msr) {
            Some(b) => {
                let i = b + usize::from(write);
                self.map[i / 8] & (1 << (i % 8)) != 0
            }
            None => true,
        }
    }
}

/// An I/O permission map in a caller-provided buffer.
pub struct IoPermissionMap<'a> {
    map: &'a mut [u8; IOPM_BYTES],
}

impl<'a> IoPermissionMap<'a> {
    /// Starts with every port intercepted.
    pub fn intercept_all(map: &'a mut [u8; IOPM_BYTES]) -> Self {
        map.fill(0xFF);
        Self { map }
    }

    /// Lets the guest access `port` directly.
    pub fn allow(&mut self, port: u16) {
        self.map[usize::from(port) / 8] &= !(1 << (port % 8));
    }

    /// True if an access of `size` bytes (1, 2 or 4) at `port` intercepts:
    /// any covered port intercepts the whole access.
    pub fn intercepts(&self, port: u16, size: u8) -> bool {
        (0..usize::from(size.clamp(1, 4))).any(|i| {
            let p = usize::from(port) + i;
            self.map[p / 8] & (1 << (p % 8)) != 0
        })
    }
}
