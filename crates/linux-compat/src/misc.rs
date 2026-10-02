//! Time, randomness, identity, limits, thread bookkeeping and futexes.

use crate::abi::*;
use crate::backend::Backend;
use crate::errno::*;
use crate::mem::{read_u64, write_u64, GuestMem};
use crate::personality::{Personality, MAX_FDS};

const NANOS: u64 = 1_000_000_000;
/// Clock ids 0..=7 are real clocks (realtime, monotonic, the CPU-time pair, raw, coarse, boot).
const CLOCK_MAX: u64 = 7;
const CLOCK_REALTIME: u32 = 0;
const CLOCK_MONOTONIC: u32 = 1;
const TIMER_ABSTIME: u64 = 1;
/// Linux stops `getrandom` at this many bytes per call.
const RANDOM_MAX: u64 = 0x1ff_ffff;
const GRND_ALL: u64 = 1 | 2 | 4;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_CLOCK_REALTIME: u32 = 256;
const RLIMIT_AS: u32 = 9;
const STACK_LIMIT: u64 = 8 << 20;

fn read_timespec(mem: &dyn GuestMem, at: u64) -> Result<u64, Errno> {
    let (sec, nsec) = (read_u64(mem, at)? as i64, read_u64(mem, at + 8)? as i64);
    if sec < 0 || !(0..NANOS as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    Ok((sec as u64)
        .saturating_mul(NANOS)
        .saturating_add(nsec as u64))
}

impl<B: Backend> Personality<B> {
    pub(crate) fn sys_uname(&mut self, mem: &mut dyn GuestMem, buf: u64) -> Result<u64, Errno> {
        mem.write(buf, &encode_utsname(self.cfg.release, b"#1 NANOX"))?;
        Ok(0)
    }

    pub(crate) fn sys_getrandom(
        &mut self,
        mem: &mut dyn GuestMem,
        buf: u64,
        len: u64,
        flags: u64,
    ) -> Result<u64, Errno> {
        if flags & !GRND_ALL != 0 {
            return Err(EINVAL);
        }
        let len = len.min(RANDOM_MAX);
        let mut done = 0u64;
        let mut tmp = [0u8; 256];
        while done < len {
            let n = (len - done).min(256) as usize;
            self.backend.random(&mut tmp[..n]);
            if let Err(e) = mem.write(buf + done, &tmp[..n]) {
                if done == 0 {
                    return Err(e);
                }
                break;
            }
            done += n as u64;
        }
        Ok(done)
    }

    pub(crate) fn sys_clock_gettime(
        &mut self,
        mem: &mut dyn GuestMem,
        clock: u64,
        ts: u64,
    ) -> Result<u64, Errno> {
        if clock > CLOCK_MAX {
            return Err(EINVAL);
        }
        let (sec, nsec) = self.backend.clock_gettime(clock as u32)?;
        write_u64(mem, ts, sec as u64)?;
        write_u64(mem, ts + 8, u64::from(nsec))?;
        Ok(0)
    }

    pub(crate) fn sys_gettimeofday(
        &mut self,
        mem: &mut dyn GuestMem,
        tv: u64,
        tz: u64,
    ) -> Result<u64, Errno> {
        if tv != 0 {
            let (sec, nsec) = self.backend.clock_gettime(CLOCK_REALTIME)?;
            write_u64(mem, tv, sec as u64)?;
            write_u64(mem, tv + 8, u64::from(nsec / 1000))?;
        }
        if tz != 0 {
            mem.write(tz, &[0u8; 8])?;
        }
        Ok(0)
    }

    pub(crate) fn sys_nanosleep(
        &mut self,
        mem: &mut dyn GuestMem,
        req: u64,
        rem: u64,
    ) -> Result<u64, Errno> {
        let ns = read_timespec(mem, req)?;
        self.backend.sleep(ns)?;
        if rem != 0 {
            mem.write(rem, &[0u8; 16])?;
        }
        Ok(0)
    }

    /// `clock_nanosleep` for the realtime, monotonic and boot clocks, relative or absolute.
    pub(crate) fn sys_clock_nanosleep(
        &mut self,
        mem: &mut dyn GuestMem,
        a: [u64; 4],
    ) -> Result<u64, Errno> {
        if !matches!(a[0], 0 | 1 | 7) {
            return Err(EINVAL);
        }
        if a[1] & !TIMER_ABSTIME != 0 {
            return Err(EINVAL);
        }
        let mut ns = read_timespec(mem, a[2])?;
        if a[1] & TIMER_ABSTIME != 0 {
            let (sec, nsec) = self.backend.clock_gettime(a[0] as u32)?;
            let now = (sec.max(0) as u64)
                .saturating_mul(NANOS)
                .saturating_add(u64::from(nsec));
            ns = ns.saturating_sub(now);
        }
        self.backend.sleep(ns)?;
        if a[3] != 0 && a[1] & TIMER_ABSTIME == 0 {
            mem.write(a[3], &[0u8; 16])?;
        }
        Ok(0)
    }

    pub(crate) fn sys_set_tid_address(&mut self, ptr: u64) -> u64 {
        self.clear_tid = ptr;
        u64::from(self.cfg.pid)
    }

    pub(crate) fn sys_arch_prctl(
        &mut self,
        mem: &mut dyn GuestMem,
        code: u64,
        addr: u64,
    ) -> Result<u64, Errno> {
        match code {
            ARCH_SET_FS => {
                // A user-space base: the upper half is the kernel's.
                if addr >= 1 << 47 {
                    return Err(EPERM);
                }
                self.backend.set_fs_base(addr)?;
                self.fs_base = addr;
                Ok(0)
            }
            ARCH_GET_FS => {
                write_u64(mem, addr, self.fs_base)?;
                Ok(0)
            }
            _ => Err(EINVAL),
        }
    }

    /// `futex` waits and wakes. The wait/wake pair and its bitset forms are
    /// carried out by the backend; the bitset is not honoured (a wake may
    /// reach a waiter it was not meant for, which futex users must tolerate).
    /// Requeue and priority-inheritance operations are refused with `ENOSYS`.
    pub(crate) fn sys_futex(
        &mut self,
        mem: &mut dyn GuestMem,
        nr: u32,
        a: [u64; 6],
    ) -> Result<u64, Errno> {
        let op = a[1] as u32;
        let addr = a[0];
        if !addr.is_multiple_of(4) {
            return Err(EINVAL);
        }
        match op & FUTEX_CMD_MASK {
            c @ (FUTEX_WAIT | FUTEX_WAIT_BITSET) => {
                let mut word = [0u8; 4];
                mem.read(addr, &mut word)?;
                if c == FUTEX_WAIT_BITSET && a[5] as u32 == 0 {
                    return Err(EINVAL);
                }
                let timeout = if a[3] == 0 {
                    None
                } else {
                    let ns = read_timespec(mem, a[3])?;
                    if c == FUTEX_WAIT_BITSET {
                        let clock = if op & FUTEX_CLOCK_REALTIME != 0 {
                            CLOCK_REALTIME
                        } else {
                            CLOCK_MONOTONIC
                        };
                        let (sec, nsec) = self.backend.clock_gettime(clock)?;
                        let now = (sec.max(0) as u64)
                            .saturating_mul(NANOS)
                            .saturating_add(u64::from(nsec));
                        Some(ns.saturating_sub(now))
                    } else {
                        Some(ns)
                    }
                };
                self.backend.futex_wait(addr, a[2] as u32, timeout)?;
                Ok(0)
            }
            c @ (FUTEX_WAKE | FUTEX_WAKE_BITSET) => {
                if c == FUTEX_WAKE_BITSET && a[5] as u32 == 0 {
                    return Err(EINVAL);
                }
                let count = (a[2] as i32).max(0) as u32;
                Ok(u64::from(self.backend.futex_wake(addr, count)?))
            }
            _ => {
                self.refuse(nr, ENOSYS);
                Err(ENOSYS)
            }
        }
    }

    /// `prlimit64` and `getrlimit`: reading is answered, changing is refused.
    pub(crate) fn sys_rlimit(
        &mut self,
        mem: &mut dyn GuestMem,
        resource: u64,
        new: u64,
        old: u64,
    ) -> Result<u64, Errno> {
        if new != 0 {
            return Err(EPERM);
        }
        if old != 0 {
            let (cur, max) = match resource as u32 {
                RLIMIT_NOFILE => (MAX_FDS as u64, MAX_FDS as u64),
                RLIMIT_STACK => (STACK_LIMIT, RLIM_INFINITY),
                RLIMIT_AS => (self.cfg.mem_limit, self.cfg.mem_limit),
                _ => (RLIM_INFINITY, RLIM_INFINITY),
            };
            write_u64(mem, old, cur)?;
            write_u64(mem, old + 8, max)?;
        }
        Ok(0)
    }

    /// One CPU: the mask is 8 bytes with bit 0 set.
    pub(crate) fn sys_sched_getaffinity(
        &mut self,
        mem: &mut dyn GuestMem,
        pid: u64,
        size: u64,
        mask: u64,
    ) -> Result<u64, Errno> {
        if pid != 0 && pid != u64::from(self.cfg.pid) {
            return Err(ESRCH);
        }
        if size < 8 {
            return Err(EINVAL);
        }
        write_u64(mem, mask, 1)?;
        Ok(8)
    }
}
