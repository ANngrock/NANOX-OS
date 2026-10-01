//! Access to the memory of the guest program. Every pointer a system call
//! receives is checked here: unmapped memory is `EFAULT`, never a crash of the
//! service and never a read of the service own memory.

use crate::errno::{Errno, EFAULT, ENAMETOOLONG};

pub const PAGE: u64 = 4096;
pub const PATH_MAX: usize = 4096;
/// The most iovec entries one call may pass (`UIO_MAXIOV`).
pub const IOV_MAX: usize = 1024;

pub trait GuestMem {
    /// Copies guest memory at `addr` into `out`; `EFAULT` if any byte is not mapped readable.
    fn read(&self, addr: u64, out: &mut [u8]) -> Result<(), Errno>;
    /// Copies `data` to guest memory; `EFAULT` if any byte is not mapped writable.
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Errno>;
}

/// The end of the lower half of the canonical address space: the part that
/// belongs to a program. Everything above is the kernel's.
pub const USER_TOP: u64 = 1 << 47;

/// `addr .. addr + len` must lie in the program half of the address space
/// (which also rules out wrapping around it).
pub fn check_range(addr: u64, len: u64) -> Result<(), Errno> {
    match addr.checked_add(len) {
        Some(end) if end <= USER_TOP => Ok(()),
        _ => Err(EFAULT),
    }
}

/// Reads a NUL-terminated string of at most `out.len()` bytes (terminator not
/// included in the result length). Reads page by page so that a fault is
/// reported exactly when the string runs into an unmapped page.
pub fn read_cstr(m: &dyn GuestMem, addr: u64, out: &mut [u8]) -> Result<usize, Errno> {
    let mut n = 0;
    while n < out.len() {
        let at = addr.checked_add(n as u64).ok_or(EFAULT)?;
        let to_page = (PAGE - at % PAGE) as usize;
        let take = to_page.min(out.len() - n);
        m.read(at, &mut out[n..n + take])?;
        if let Some(z) = out[n..n + take].iter().position(|b| *b == 0) {
            return Ok(n + z);
        }
        n += take;
    }
    Err(ENAMETOOLONG)
}

pub fn read_u64(m: &dyn GuestMem, addr: u64) -> Result<u64, Errno> {
    let mut b = [0u8; 8];
    m.read(addr, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

pub fn read_u32(m: &dyn GuestMem, addr: u64) -> Result<u32, Errno> {
    let mut b = [0u8; 4];
    m.read(addr, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub fn write_u64(m: &mut dyn GuestMem, addr: u64, v: u64) -> Result<(), Errno> {
    m.write(addr, &v.to_le_bytes())
}

pub fn write_u32(m: &mut dyn GuestMem, addr: u64, v: u32) -> Result<(), Errno> {
    m.write(addr, &v.to_le_bytes())
}
