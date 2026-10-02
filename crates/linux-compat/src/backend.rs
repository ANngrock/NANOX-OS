//! What the personality asks of the NANOX side. The real service implements
//! this over the objects it was delegated (a directory capability, memory
//! objects, notifications, the clock); tests implement it in memory. Paths
//! arrive normalized, absolute and confined to the delegated root.

use crate::abi::{FileKind, Stat};
use crate::errno::Errno;
use crate::fdtable::Obj;
use crate::vma::{BrkPlan, MmapPlan};

pub trait Backend {
    /// Opens (and with `O_CREAT` creates) the object at `path`.
    fn open(&mut self, path: &[u8], flags: u32, mode: u32) -> Result<(Obj, FileKind), Errno>;
    fn stat(&mut self, path: &[u8], follow: bool) -> Result<Stat, Errno>;
    fn fstat(&mut self, obj: Obj) -> Result<Stat, Errno>;
    fn read(&mut self, obj: Obj, off: u64, buf: &mut [u8]) -> Result<usize, Errno>;
    fn write(&mut self, obj: Obj, off: u64, data: &[u8]) -> Result<usize, Errno>;
    fn close(&mut self, obj: Obj);
    /// Entry number `index` of a directory: (inode, kind, name length), the name written to `name`.
    fn dirent(
        &mut self,
        obj: Obj,
        index: u64,
        name: &mut [u8; 256],
    ) -> Result<Option<(u64, FileKind, usize)>, Errno>;
    fn mkdir(&mut self, path: &[u8], mode: u32) -> Result<(), Errno>;
    fn unlink(&mut self, path: &[u8]) -> Result<(), Errno>;
    fn rmdir(&mut self, path: &[u8]) -> Result<(), Errno>;
    fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno>;
    fn readlink(&mut self, path: &[u8], buf: &mut [u8]) -> Result<usize, Errno>;
    fn symlink(&mut self, target: &[u8], path: &[u8]) -> Result<(), Errno>;
    fn link(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno>;
    fn truncate(&mut self, obj: Obj, len: u64) -> Result<(), Errno>;
    /// A connected pair: `duplex == false` a pipe (read end, write end),
    /// `duplex == true` the two ends of a stream socket pair.
    fn pipe(&mut self, duplex: bool) -> Result<(Obj, Obj), Errno>;
    /// Which of `events` (`POLLIN`, `POLLOUT`) are ready now.
    fn poll(&mut self, obj: Obj, events: u16) -> u16;

    /// Maps `plan`: zero-filled pages for anonymous memory, file contents
    /// (zero beyond its end) for a file. The backend takes its own reference
    /// to the object of a file mapping, so closing the descriptor leaves the
    /// mapping intact. With `plan.replaces` whatever was there is replaced.
    fn map(&mut self, plan: &MmapPlan) -> Result<(), Errno>;
    fn unmap(&mut self, start: u64, end: u64) -> Result<(), Errno>;
    fn protect(&mut self, start: u64, end: u64, prot: u8) -> Result<(), Errno>;
    /// The pages of `start..end` read as they did when first mapped (zero for
    /// anonymous memory): `madvise(MADV_DONTNEED)`.
    fn discard(&mut self, start: u64, end: u64) -> Result<(), Errno>;
    /// Moves the end of the heap (`plan.new_end` below `plan.old_end` shrinks it).
    fn resize_heap(&mut self, plan: &BrkPlan) -> Result<(), Errno>;

    fn clock_gettime(&mut self, clock: u32) -> Result<(i64, u32), Errno>;
    fn sleep(&mut self, nanos: u64) -> Result<(), Errno>;
    fn random(&mut self, buf: &mut [u8]);
    /// Sleeps on the 32-bit word at `addr` if it still equals `expected`
    /// (comparing and sleeping is one step, which only the kernel can make
    /// atomic): `Ok` when woken, `EAGAIN` when the word differed, `ETIMEDOUT`
    /// after `timeout_ns`, `EINTR` when interrupted.
    fn futex_wait(
        &mut self,
        addr: u64,
        expected: u32,
        timeout_ns: Option<u64>,
    ) -> Result<(), Errno>;
    /// Wakes up to `count` sleepers on `addr`; how many were woken.
    fn futex_wake(&mut self, addr: u64, count: u32) -> Result<u32, Errno>;
    fn set_fs_base(&mut self, value: u64) -> Result<(), Errno>;
}
