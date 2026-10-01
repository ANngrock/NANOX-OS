//! Memory, time, identity, threads, pipes and sockets, processes, and the
//! policy as the program meets it.

mod common;

use common::*;
use linux_compat::abi::*;
use linux_compat::errno::*;
use linux_compat::policy::{disposition, Disp, HANDLED};
use linux_compat::table::*;
use linux_compat::vma::{Backing, PROT_EXEC, PROT_READ, PROT_WRITE};
use linux_compat::Outcome;

const RW: u64 = (PROT_READ | PROT_WRITE) as u64;
const PRIV_ANON: u64 = MAP_PRIVATE | MAP_ANONYMOUS;

fn mmap(p: &mut Proc, addr: u64, len: u64, prot: u64, flags: u64, fd: i64, off: u64) -> i64 {
    p.call(SYS_MMAP, &[addr, len, prot, flags, fd as u64, off])
}

// ---------------------------------------------------------------- memory

#[test]
fn mmap_anonymous_is_zeroed_and_usable() {
    let mut p = Proc::new();
    let a = mmap(&mut p, 0, 10_000, RW, PRIV_ANON, -1, 0);
    assert!(a > 0 && a % 4096 == 0, "{a:#x}");
    assert_eq!(
        p.mem.pages(),
        64 + 3,
        "three pages mapped (rounded up) next to the scratch region"
    );
    assert_eq!(p.bytes(a as u64, 16), [0u8; 16]);
    let b = mmap(&mut p, 0, 4096, RW, PRIV_ANON, -1, 0);
    assert!(
        b > 0 && (b as u64) < a as u64,
        "top-down placement: the second is below the first"
    );
    assert_eq!(
        p.p.space().entries().len(),
        1,
        "neighbours with the same protection are one entry"
    );
    assert_eq!(p.p.space().mapped_bytes(), 4 * 4096);
    assert_eq!(p.p.space().check(), Ok(()));
    // a program can use what it mapped through a system call
    let fd = p.open("/etc/passwd", O_RDONLY);
    assert_eq!(p.call(SYS_READ, &[fd as u64, a as u64, 4]), 4);
    assert_eq!(p.bytes(a as u64, 4), b"root");
}

#[test]
fn mmap_argument_rules() {
    let mut p = Proc::new();
    assert_eq!(
        mmap(&mut p, 0, 0, RW, PRIV_ANON, -1, 0),
        neg(EINVAL),
        "zero length"
    );
    assert_eq!(
        mmap(&mut p, 0, 4096, RW, MAP_ANONYMOUS, -1, 0),
        neg(EINVAL),
        "neither shared nor private"
    );
    assert_eq!(
        mmap(&mut p, 0, 4096, RW, MAP_ANONYMOUS | 3, -1, 0),
        neg(EINVAL),
        "both"
    );
    assert_eq!(
        mmap(&mut p, 0, 4096, RW, PRIV_ANON | 0x4_0000_0000, -1, 0),
        neg(EINVAL),
        "unknown flag"
    );
    assert_eq!(
        mmap(&mut p, 0, 4096, 0x1_0000, PRIV_ANON, -1, 0),
        neg(EINVAL),
        "protection out of range"
    );
    assert_eq!(
        mmap(&mut p, 5, 4096, RW, PRIV_ANON | MAP_FIXED, -1, 0),
        neg(EINVAL),
        "unaligned fixed address"
    );
    assert_eq!(
        mmap(&mut p, 0x1000, 4096, RW, PRIV_ANON | MAP_FIXED, -1, 0),
        neg(ENOMEM),
        "below the window"
    );
    assert_eq!(
        mmap(
            &mut p,
            0x7fff_ffff_f000,
            8192,
            RW,
            PRIV_ANON | MAP_FIXED,
            -1,
            0
        ),
        neg(ENOMEM),
        "above it"
    );
    assert_eq!(
        mmap(&mut p, 0, u64::MAX - 100, RW, PRIV_ANON, -1, 0),
        neg(ENOMEM),
        "length overflow"
    );
    assert_eq!(
        mmap(&mut p, 0, 2 << 30, RW, PRIV_ANON, -1, 0),
        neg(ENOMEM),
        "over the memory limit"
    );
    assert!(
        mmap(&mut p, 0, 4096, RW, PRIV_ANON | 0x4000 | 0x8000, -1, 0) > 0,
        "NORESERVE and POPULATE are accepted"
    );
    assert_eq!(
        p.p.space().entries().len(),
        1,
        "nothing but the last one was recorded"
    );
}

#[test]
fn mmap_fixed_replaces_and_noreplace_refuses() {
    let mut p = Proc::new();
    let a = mmap(&mut p, 0, 8192, RW, PRIV_ANON, -1, 0) as u64;
    p.mem.poke(a, b"old");
    assert_eq!(
        mmap(&mut p, a, 4096, RW, PRIV_ANON | MAP_FIXED, -1, 0),
        a as i64
    );
    assert_eq!(p.bytes(a, 3), [0, 0, 0], "MAP_FIXED replaced the page");
    assert_eq!(
        p.p.space().entries().len(),
        1,
        "the replaced page joins its unchanged neighbour again"
    );
    assert_eq!(p.p.space().mapped_bytes(), 8192);
    assert_eq!(
        mmap(&mut p, a, 4096, RW, PRIV_ANON | MAP_FIXED_NOREPLACE, -1, 0),
        neg(EEXIST)
    );
    assert_eq!(
        mmap(
            &mut p,
            a - 0x10_0000,
            4096,
            RW,
            PRIV_ANON | MAP_FIXED_NOREPLACE,
            -1,
            0
        ),
        (a - 0x10_0000) as i64
    );
    // a hint that is free is honoured, one that is taken is not
    let h = a - 0x20_0000;
    assert_eq!(mmap(&mut p, h, 4096, RW, PRIV_ANON, -1, 0), h as i64);
    assert_ne!(mmap(&mut p, h, 4096, RW, PRIV_ANON, -1, 0), h as i64);
    assert_eq!(p.p.space().check(), Ok(()));
}

#[test]
fn mmap_of_a_file() {
    let mut p = Proc::new();
    let fd = p.open("/etc/passwd", O_RDONLY);
    let a = mmap(&mut p, 0, 4096, PROT_READ as u64, MAP_PRIVATE, fd, 0);
    assert!(a > 0);
    assert_eq!(p.bytes(a as u64, 4), b"root");
    let b = mmap(&mut p, 0, 4096, PROT_READ as u64, MAP_PRIVATE, fd, 4096);
    assert_eq!(
        p.bytes(b as u64, 4),
        [0; 4],
        "beyond the end of the file reads as zero"
    );
    assert_eq!(
        mmap(&mut p, 0, 4096, PROT_READ as u64, MAP_PRIVATE, fd, 100),
        neg(EINVAL),
        "offset not page aligned"
    );
    // closing the descriptor leaves the mapping alone
    p.call(SYS_CLOSE, &[fd as u64]);
    assert_eq!(p.bytes(a as u64, 4), b"root");
    // private writable is fine; shared writable is not provided
    let rw = p.open("/tmp/m", O_CREAT | O_RDWR);
    assert!(mmap(&mut p, 0, 4096, RW, MAP_PRIVATE, rw, 0) > 0);
    assert_eq!(mmap(&mut p, 0, 4096, RW, MAP_SHARED, rw, 0), neg(ENODEV));
    // write-only descriptors cannot be mapped; directories, terminals and bad descriptors
    let wo = p.open("/tmp/w", O_CREAT | O_WRONLY);
    assert_eq!(
        mmap(&mut p, 0, 4096, PROT_READ as u64, MAP_PRIVATE, wo, 0),
        neg(EACCES)
    );
    let d = p.open("/tmp", O_RDONLY | O_DIRECTORY);
    assert_eq!(
        mmap(&mut p, 0, 4096, PROT_READ as u64, MAP_PRIVATE, d, 0),
        neg(ENODEV)
    );
    assert_eq!(
        mmap(&mut p, 0, 4096, PROT_READ as u64, MAP_PRIVATE, 1, 0),
        neg(ENODEV)
    );
    assert_eq!(
        mmap(&mut p, 0, 4096, PROT_READ as u64, MAP_PRIVATE, 77, 0),
        neg(EBADF)
    );
    let e =
        p.p.space()
            .entries()
            .iter()
            .find(|v| v.start == a as u64)
            .copied()
            .unwrap();
    assert!(matches!(e.backing, Backing::File { offset: 0, .. }));
}

#[test]
fn writable_and_executable_is_refused_and_audited() {
    let mut p = Proc::new();
    let wx = (PROT_READ | PROT_WRITE | PROT_EXEC) as u64;
    assert_eq!(mmap(&mut p, 0, 4096, wx, PRIV_ANON, -1, 0), neg(EPERM));
    let a = mmap(&mut p, 0, 4096, RW, PRIV_ANON, -1, 0) as u64;
    assert_eq!(p.call(SYS_MPROTECT, &[a, 4096, wx]), neg(EPERM));
    let rx = (PROT_READ | PROT_EXEC) as u64;
    assert_eq!(
        p.call(SYS_MPROTECT, &[a, 4096, rx]),
        0,
        "write then execute, one at a time, is fine"
    );
    assert_eq!(p.mem.prot_at(a), Some(PROT_READ | PROT_EXEC));
    let r = p.p.refusals();
    assert_eq!(r.len(), 2);
    assert_eq!((r[0].nr, r[0].errno), (SYS_MMAP, EPERM));
    assert_eq!((r[1].nr, r[1].errno), (SYS_MPROTECT, EPERM));
    // the one configuration that allows it
    let mut cfg = config();
    cfg.allow_wx = true;
    let mut q = Proc::with(cfg);
    assert!(mmap(&mut q, 0, 4096, wx, PRIV_ANON, -1, 0) > 0);
    assert!(q.p.refusals().is_empty());
}

#[test]
fn munmap_mprotect_and_holes() {
    let mut p = Proc::new();
    let a = mmap(&mut p, 0, 4 * 4096, RW, PRIV_ANON, -1, 0) as u64;
    assert_eq!(
        p.call(SYS_MUNMAP, &[a + 4096, 4096]),
        0,
        "a hole in the middle"
    );
    assert_eq!(p.mem.prot_at(a + 4096), None);
    assert_eq!(p.mem.prot_at(a), Some(3));
    assert_eq!(p.p.space().entries().len(), 2);
    assert_eq!(p.call(SYS_MUNMAP, &[a + 1, 4096]), neg(EINVAL));
    assert_eq!(p.call(SYS_MUNMAP, &[a, 0]), neg(EINVAL));
    assert_eq!(
        p.call(SYS_MUNMAP, &[a + 4096, 4096]),
        0,
        "unmapping what is not mapped is not an error"
    );
    // mprotect across the hole is ENOMEM; over a mapped part it applies
    assert_eq!(
        p.call(SYS_MPROTECT, &[a, 3 * 4096, PROT_READ as u64]),
        neg(ENOMEM)
    );
    assert_eq!(
        p.call(SYS_MPROTECT, &[a + 2 * 4096, 2 * 4096, PROT_READ as u64]),
        0
    );
    assert_eq!(p.mem.prot_at(a + 2 * 4096), Some(1));
    assert_eq!(p.mem.prot_at(a), Some(3));
    assert_eq!(p.call(SYS_MPROTECT, &[a, 4096, 0x1_0000]), neg(EINVAL));
    assert_eq!(p.call(SYS_MPROTECT, &[a + 5, 4096, 1]), neg(EINVAL));
    assert_eq!(p.call(SYS_MPROTECT, &[a, 0, 1]), 0);
    assert_eq!(p.p.space().check(), Ok(()));
}

#[test]
fn a_failing_backend_leaves_the_books_untouched() {
    let mut p = Proc::new();
    let a = mmap(&mut p, 0, 4096, RW, PRIV_ANON, -1, 0);
    assert!(a > 0);
    p.be().fail_map = true;
    assert_eq!(mmap(&mut p, 0, 4096, RW, PRIV_ANON, -1, 0), neg(ENOMEM));
    assert_eq!(p.p.space().entries().len(), 1);
    p.be().fail_heap = true;
    let brk0 = p.call(SYS_BRK, &[0]);
    assert_eq!(
        p.call(SYS_BRK, &[(brk0 + 8192) as u64]),
        brk0,
        "brk answers with the unchanged break"
    );
    assert_eq!(p.p.space().entries().len(), 1);
    assert_eq!(p.p.space().mapped_bytes(), 4096);
}

#[test]
fn brk_grows_shrinks_and_refuses() {
    let mut p = Proc::new();
    let base = p.call(SYS_BRK, &[0]);
    assert_eq!(base as u64, config().brk_base);
    assert_eq!(
        p.call(SYS_BRK, &[base as u64 + 10_000]),
        base + 12_288,
        "rounded up to pages"
    );
    p.mem.poke(base as u64, b"heap");
    assert_eq!(p.call(SYS_BRK, &[base as u64 + 20_000]), base + 20_480);
    assert_eq!(
        p.bytes(base as u64, 4),
        b"heap",
        "growing keeps the contents"
    );
    assert_eq!(
        p.call(SYS_BRK, &[base as u64 + 4096]),
        base + 4096,
        "shrinking"
    );
    assert_eq!(p.mem.prot_at(base as u64 + 8192), None);
    assert_eq!(
        p.call(SYS_BRK, &[base as u64 - 1]),
        base + 4096,
        "below the base asks only"
    );
    assert_eq!(
        p.call(SYS_BRK, &[u64::MAX]),
        base + 4096,
        "absurd request: unchanged"
    );
    assert_eq!(
        p.call(SYS_BRK, &[(2u64 << 30) + base as u64]),
        base + 4096,
        "over the memory limit: unchanged"
    );
    assert_eq!(p.p.space().check(), Ok(()));
    // a mapping in the way stops the heap
    let mut q = Proc::new();
    let b = q.call(SYS_BRK, &[0]) as u64;
    assert_eq!(
        mmap(&mut q, b + 0x8000, 4096, RW, PRIV_ANON | MAP_FIXED, -1, 0),
        (b + 0x8000) as i64
    );
    assert_eq!(
        q.call(SYS_BRK, &[b + 0x10000]),
        b as i64,
        "would overlap the mapping"
    );
    assert_eq!(q.call(SYS_BRK, &[b + 0x4000]), (b + 0x4000) as i64);
}

#[test]
fn madvise_discard_zeroes_and_hints_do_nothing() {
    let mut p = Proc::new();
    let a = mmap(&mut p, 0, 2 * 4096, RW, PRIV_ANON, -1, 0) as u64;
    p.mem.poke(a, b"secret");
    assert_eq!(p.call(SYS_MADVISE, &[a, 4096, 3]), 0, "WILLNEED");
    assert_eq!(p.bytes(a, 6), b"secret");
    assert_eq!(p.call(SYS_MADVISE, &[a, 4096, 4]), 0, "DONTNEED");
    assert_eq!(p.bytes(a, 6), [0; 6]);
    assert_eq!(p.call(SYS_MADVISE, &[a + 1, 4096, 4]), neg(EINVAL));
    assert_eq!(p.call(SYS_MADVISE, &[a, 4096, 999]), neg(EINVAL));
    assert_eq!(
        p.call(SYS_MADVISE, &[a, 3 * 4096, 4]),
        neg(ENOMEM),
        "past the mapping"
    );
    assert_eq!(p.call(SYS_MADVISE, &[a, 0, 4]), 0);
}

// ------------------------------------------------------------ time, ids

#[test]
fn clocks_and_sleep_use_the_backend_clock() {
    let mut p = Proc::new();
    let ts = p.buf(16);
    assert_eq!(p.call(SYS_CLOCK_GETTIME, &[1, ts]), 0);
    let t0 = (p.u64_at(ts), p.u64_at(ts + 8));
    assert_eq!(t0, (5, 0));
    let req = p.put(&[2u64.to_le_bytes(), 500u64.to_le_bytes()].concat());
    assert_eq!(p.call(SYS_NANOSLEEP, &[req, 0]), 0);
    p.call(SYS_CLOCK_GETTIME, &[1, ts]);
    assert_eq!((p.u64_at(ts), p.u64_at(ts + 8)), (7, 500));
    p.call(SYS_CLOCK_GETTIME, &[0, ts]);
    assert_eq!(
        p.u64_at(ts),
        1_700_000_007,
        "realtime is monotonic plus the epoch offset"
    );
    assert_eq!(p.call(SYS_CLOCK_GETTIME, &[99, ts]), neg(EINVAL));
    assert_eq!(p.call(SYS_CLOCK_GETTIME, &[1, 0xdead_0000]), neg(EFAULT));
    let tv = p.buf(16);
    let tz = p.buf(8);
    assert_eq!(p.call(SYS_GETTIMEOFDAY, &[tv, tz]), 0);
    assert_eq!(p.u64_at(tv), 1_700_000_007);
    assert_eq!(p.u64_at(tv + 8), 0, "microseconds");
    // bad requests
    let bad = p.put(&[1u64.to_le_bytes(), 1_000_000_000u64.to_le_bytes()].concat());
    assert_eq!(p.call(SYS_NANOSLEEP, &[bad, 0]), neg(EINVAL));
    let neg_s = p.put(&[(-1i64 as u64).to_le_bytes(), 0u64.to_le_bytes()].concat());
    assert_eq!(p.call(SYS_NANOSLEEP, &[neg_s, 0]), neg(EINVAL));
    assert_eq!(p.call(SYS_NANOSLEEP, &[0xdead_0000, 0]), neg(EFAULT));
}

#[test]
fn clock_nanosleep_absolute_and_relative() {
    let mut p = Proc::new();
    let rel = p.put(&[1u64.to_le_bytes(), 0u64.to_le_bytes()].concat());
    assert_eq!(p.call(SYS_CLOCK_NANOSLEEP, &[1, 0, rel, 0]), 0);
    assert_eq!(p.be().sleeps, [1_000_000_000]);
    // absolute: now is 6 s, wake at 8 s -> sleep 2 s; in the past -> no wait
    let abs = p.put(&[8u64.to_le_bytes(), 0u64.to_le_bytes()].concat());
    assert_eq!(p.call(SYS_CLOCK_NANOSLEEP, &[1, 1, abs, 0]), 0);
    assert_eq!(p.be().sleeps, [1_000_000_000, 2_000_000_000]);
    assert_eq!(p.call(SYS_CLOCK_NANOSLEEP, &[1, 1, abs, 0]), 0);
    assert_eq!(*p.be().sleeps.last().unwrap(), 0);
    assert_eq!(
        p.call(SYS_CLOCK_NANOSLEEP, &[3, 0, rel, 0]),
        neg(EINVAL),
        "CPU-time clocks cannot sleep"
    );
    assert_eq!(p.call(SYS_CLOCK_NANOSLEEP, &[1, 2, rel, 0]), neg(EINVAL));
}

#[test]
fn getrandom_fills_and_validates() {
    let mut p = Proc::new();
    let b = p.buf(1000);
    assert_eq!(p.call(SYS_GETRANDOM, &[b, 1000, 0]), 1000);
    let v = p.bytes(b, 1000);
    assert!(
        v.iter().any(|x| *x != 0) && v.windows(8).any(|w| w != &v[..8]),
        "not constant"
    );
    assert_eq!(p.call(SYS_GETRANDOM, &[b, 10, 0x80]), neg(EINVAL));
    assert_eq!(p.call(SYS_GETRANDOM, &[0xdead_0000, 10, 0]), neg(EFAULT));
    assert_eq!(p.call(SYS_GETRANDOM, &[b, 0, 0]), 0);
}

#[test]
fn identity_and_limits() {
    let mut p = Proc::new();
    assert_eq!(p.call(SYS_GETPID, &[]), 4242);
    assert_eq!(p.call(SYS_GETTID, &[]), 4242);
    assert_eq!(p.call(SYS_GETUID, &[]), 1000);
    assert_eq!(p.call(SYS_GETEUID, &[]), 1000);
    assert_eq!(p.call(SYS_GETGID, &[]), 1000);
    assert_eq!(p.call(SYS_GETPPID, &[]), 1);
    let u = p.buf(390);
    assert_eq!(p.call(SYS_UNAME, &[u]), 0);
    let raw = p.bytes(u, 390);
    let field = |i: usize| {
        String::from_utf8_lossy(&raw[i * 65..(i + 1) * 65])
            .trim_end_matches('\0')
            .to_string()
    };
    assert_eq!(
        (field(0), field(1), field(2), field(4)),
        (
            "Linux".into(),
            "nanox".into(),
            "6.1.0-nanox".into(),
            "x86_64".into()
        )
    );
    let l = p.buf(16);
    assert_eq!(p.call(SYS_PRLIMIT64, &[0, RLIMIT_NOFILE as u64, 0, l]), 0);
    assert_eq!((p.u64_at(l), p.u64_at(l + 8)), (64, 64));
    assert_eq!(p.call(SYS_GETRLIMIT, &[RLIMIT_STACK as u64, l]), 0);
    assert_eq!(p.u64_at(l), 8 << 20);
    assert_eq!(
        p.call(SYS_PRLIMIT64, &[0, RLIMIT_NOFILE as u64, l, 0]),
        neg(EPERM),
        "limits cannot be changed"
    );
    assert_eq!(p.call(SYS_PRLIMIT64, &[999, 7, 0, l]), neg(ESRCH));
    assert_eq!(p.call(SYS_PRLIMIT64, &[4242, 7, 0, l]), 0);
    let m = p.buf(8);
    assert_eq!(p.call(SYS_SCHED_GETAFFINITY, &[0, 128, m]), 8);
    assert_eq!(p.u64_at(m), 1);
    assert_eq!(p.call(SYS_SCHED_GETAFFINITY, &[0, 4, m]), neg(EINVAL));
    assert_eq!(p.call(SYS_SCHED_GETAFFINITY, &[1, 8, m]), neg(ESRCH));
    assert_eq!(p.call(SYS_SCHED_YIELD, &[]), 0);
}

#[test]
fn thread_pointer_and_tid_address() {
    let mut p = Proc::new();
    assert_eq!(p.call(SYS_ARCH_PRCTL, &[ARCH_SET_FS, 0x2000_0000]), 0);
    assert_eq!(p.p.fs_base(), 0x2000_0000);
    assert_eq!(p.be().fs_base, 0x2000_0000);
    let out = p.buf(8);
    assert_eq!(p.call(SYS_ARCH_PRCTL, &[ARCH_GET_FS, out]), 0);
    assert_eq!(p.u64_at(out), 0x2000_0000);
    assert_eq!(
        p.call(SYS_ARCH_PRCTL, &[ARCH_SET_FS, 0xffff_8000_0000_0000]),
        neg(EPERM),
        "a kernel address"
    );
    assert_eq!(
        p.call(SYS_ARCH_PRCTL, &[0x1001, 1]),
        neg(EINVAL),
        "GS is not offered"
    );
    assert_eq!(p.call(SYS_SET_TID_ADDRESS, &[0x3000]), 4242);
    assert_eq!(p.p.clear_child_tid(), 0x3000);
}

#[test]
fn futex_wait_wake_and_timeouts() {
    let mut p = Proc::new();
    let w = p.put(&5u32.to_le_bytes());
    // value differs: EAGAIN; matches: the backend waits
    assert_eq!(
        p.call(SYS_FUTEX, &[w, FUTEX_WAIT as u64, 6, 0]),
        neg(EAGAIN)
    );
    assert_eq!(
        p.call(
            SYS_FUTEX,
            &[w, (FUTEX_WAIT | FUTEX_PRIVATE_FLAG) as u64, 5, 0]
        ),
        0
    );
    // relative timeout
    let t = p.put(&[0u64.to_le_bytes(), 7_000u64.to_le_bytes()].concat());
    assert_eq!(
        p.call(SYS_FUTEX, &[w, FUTEX_WAIT as u64, 5, t]),
        neg(ETIMEDOUT)
    );
    assert_eq!(*p.be().futex_calls.last().unwrap(), (w, 5, Some(7_000)));
    // WAIT_BITSET takes an absolute monotonic deadline: now is 5 s (+7 us after the wait above)
    let abs = p.put(&[9u64.to_le_bytes(), 0u64.to_le_bytes()].concat());
    assert_eq!(
        p.call(
            SYS_FUTEX,
            &[w, 9 | FUTEX_PRIVATE_FLAG as u64, 5, abs, 0, 0xffff_ffff]
        ),
        neg(ETIMEDOUT)
    );
    let (_, _, to) = *p.be().futex_calls.last().unwrap();
    assert_eq!(to, Some(9_000_000_000 - 5_000_007_000));
    assert_eq!(
        p.call(SYS_FUTEX, &[w, 9, 5, 0, 0, 0]),
        neg(EINVAL),
        "an empty bitset"
    );
    // wake
    p.be().futex_waiters = 3;
    assert_eq!(
        p.call(SYS_FUTEX, &[w, (FUTEX_WAKE | FUTEX_PRIVATE_FLAG) as u64, 1]),
        1
    );
    assert_eq!(
        p.call(SYS_FUTEX, &[w, FUTEX_WAKE as u64, i32::MAX as u64]),
        3
    );
    assert_eq!(
        p.call(SYS_FUTEX, &[w, FUTEX_WAKE as u64, (-1i32) as u32 as u64]),
        0,
        "a negative count wakes none"
    );
    // bad address, unaligned, unsupported operations are refused and audited
    assert_eq!(
        p.call(SYS_FUTEX, &[0xdead_0000, FUTEX_WAIT as u64, 0, 0]),
        neg(EFAULT)
    );
    assert_eq!(
        p.call(SYS_FUTEX, &[w + 1, FUTEX_WAIT as u64, 0, 0]),
        neg(EINVAL)
    );
    assert_eq!(p.call(SYS_FUTEX, &[w, 3, 1, 1, w]), neg(ENOSYS), "requeue");
    assert_eq!(
        p.call(SYS_FUTEX, &[w, 6, 0, 0]),
        neg(ENOSYS),
        "priority inheritance"
    );
    assert_eq!(p.p.refusals().len(), 2);
}

// ---------------------------------------------------- pipes and sockets

#[test]
fn pipe_carries_bytes_and_ends_cleanly() {
    let mut p = Proc::new();
    let fds = p.buf(8);
    assert_eq!(p.call(SYS_PIPE2, &[fds, u64::from(O_CLOEXEC)]), 0);
    let (r, w) = (p.u32_at(fds) as i64, p.u32_at(fds + 4) as i64);
    assert!(r >= 3 && w == r + 1);
    assert_eq!(p.call(SYS_FCNTL, &[r as u64, F_GETFD, 0]), 1);
    assert_eq!(p.read_str(r, 10).0, neg(EAGAIN), "empty, writer open");
    assert_eq!(p.write_str(w, "ping"), 4);
    assert_eq!(p.read_str(r, 10), (4, "ping".into()));
    // the ends are one-way
    assert_eq!(p.write_str(r, "x"), neg(EBADF));
    assert_eq!(p.read_str(w, 1).0, neg(EBADF));
    assert_eq!(p.call(SYS_LSEEK, &[r as u64, 0, 0]), neg(ESPIPE));
    p.call(SYS_CLOSE, &[w as u64]);
    assert_eq!(
        p.read_str(r, 10).0,
        0,
        "end of file once the writer is gone"
    );
    p.call(SYS_CLOSE, &[r as u64]);
    assert_eq!(p.be().live(), 1, "both ends were closed in the backend");
    // writing with no reader
    let fds = p.buf(8);
    p.call(SYS_PIPE, &[fds]);
    let (r, w) = (p.u32_at(fds) as u64, p.u32_at(fds + 4) as u64);
    p.call(SYS_CLOSE, &[r]);
    assert_eq!(p.write_str(w as i64, "x"), neg(EPIPE));
    assert_eq!(p.call(SYS_PIPE2, &[fds, 0x40]), neg(EINVAL));
    assert_eq!(p.call(SYS_PIPE2, &[0xdead_0000, 0]), neg(EFAULT));
    assert_eq!(p.p.fds().check(), Ok(()));
}

#[test]
fn a_failed_pipe_leaves_nothing_behind() {
    let mut p = Proc::new();
    let before = (p.be().live(), p.p.fds().open_count());
    assert_eq!(p.call(SYS_PIPE, &[0xdead_0000]), neg(EFAULT));
    assert_eq!((p.be().live(), p.p.fds().open_count()), before);
    // table full: fill it, then ask for a pipe
    let b = p.buf(8);
    let mut n = 0;
    while p.call(SYS_PIPE, &[b]) == 0 {
        n += 1;
        assert!(n < 64);
    }
    let held = (p.be().live(), p.p.fds().open_count());
    assert_eq!(p.call(SYS_PIPE, &[b]), neg(EMFILE));
    assert_eq!(
        (p.be().live(), p.p.fds().open_count()),
        held,
        "a half-built pipe was undone"
    );
    assert!(p.p.fds().open_count() >= 62);
    assert_eq!(p.p.fds().check(), Ok(()));
}

#[test]
fn socketpair_is_bidirectional() {
    let mut p = Proc::new();
    let sv = p.buf(8);
    assert_eq!(
        p.call(SYS_SOCKETPAIR, &[1, 1 | u64::from(O_CLOEXEC), 0, sv]),
        0
    );
    let (a, b) = (p.u32_at(sv) as i64, p.u32_at(sv + 4) as i64);
    assert_eq!(p.call(SYS_FCNTL, &[a as u64, F_GETFD, 0]), 1);
    let data = p.put(b"hello");
    assert_eq!(p.call(SYS_SENDTO, &[a as u64, data, 5, 0, 0, 0]), 5);
    let out = p.buf(16);
    assert_eq!(p.call(SYS_RECVFROM, &[b as u64, out, 16, 0, 0, 0]), 5);
    assert_eq!(p.bytes(out, 5), b"hello");
    assert_eq!(p.call(SYS_SENDTO, &[b as u64, data, 2, 0, 0, 0]), 2);
    assert_eq!(
        p.read_str(a, 8),
        (2, "he".into()),
        "plain read works on a socket"
    );
    // flags
    assert_eq!(
        p.call(SYS_RECVFROM, &[b as u64, out, 16, 0x40, 0, 0]),
        neg(EAGAIN),
        "MSG_DONTWAIT on an empty socket"
    );
    assert_eq!(
        p.call(SYS_RECVFROM, &[b as u64, out, 16, 0x2, 0, 0]),
        neg(EINVAL)
    );
    assert_eq!(
        p.call(SYS_SENDTO, &[a as u64, data, 1, 0, data, 16]),
        neg(EISCONN)
    );
    let addrlen = p.put(&99u32.to_le_bytes());
    p.call(SYS_SENDTO, &[a as u64, data, 1, 0, 0, 0]);
    assert_eq!(
        p.call(SYS_RECVFROM, &[b as u64, out, 16, 0, out, addrlen]),
        1
    );
    assert_eq!(p.u32_at(addrlen), 0, "no peer address is reported");
    // not sockets
    let f = p.open("/etc/passwd", O_RDONLY);
    assert_eq!(
        p.call(SYS_RECVFROM, &[f as u64, out, 16, 0, 0, 0]),
        neg(ENOTSOCK)
    );
    assert_eq!(
        p.call(SYS_SENDTO, &[f as u64, data, 1, 0, 0, 0]),
        neg(ENOTSOCK)
    );
    // unsupported kinds
    assert_eq!(p.call(SYS_SOCKETPAIR, &[2, 1, 0, sv]), neg(EAFNOSUPPORT));
    assert_eq!(
        p.call(SYS_SOCKETPAIR, &[1, 2, 0, sv]),
        neg(EOPNOTSUPP),
        "datagram"
    );
    assert_eq!(p.call(SYS_SOCKETPAIR, &[1, 1, 6, sv]), neg(EOPNOTSUPP));
    assert_eq!(
        p.call(SYS_SOCKETPAIR, &[1, 1 | 0x8000_0000, 0, sv]),
        neg(EINVAL)
    );
    // closing one end: the other sees end of file and EPIPE
    p.call(SYS_CLOSE, &[b as u64]);
    assert_eq!(p.read_str(a, 8).0, 0);
    assert_eq!(p.write_str(a, "x"), neg(EPIPE));
}

#[test]
fn poll_reports_readiness_and_waits_in_slices() {
    let mut p = Proc::new();
    let fds = p.buf(8);
    p.call(SYS_PIPE, &[fds]);
    let (r, w) = (p.u32_at(fds) as i32, p.u32_at(fds + 4) as i32);
    let file = p.open("/etc/passwd", O_RDONLY) as i32;
    let entry = |fd: i32, ev: u16| {
        [
            fd.to_le_bytes().to_vec(),
            ev.to_le_bytes().to_vec(),
            vec![0, 0],
        ]
        .concat()
    };
    let set = p.put(
        &[
            entry(r, POLLIN),
            entry(w, POLLOUT),
            entry(file, POLLIN),
            entry(-1, POLLIN),
            entry(60, POLLIN),
        ]
        .concat(),
    );
    let revents =
        |p: &Proc, i: u64| u16::from_le_bytes(p.bytes(set + 8 * i + 6, 2).try_into().unwrap());
    // nothing to read yet, writable, file always ready, negative ignored, unknown is NVAL
    assert_eq!(p.call(SYS_POLL, &[set, 5, 0]), 3);
    assert_eq!(
        [0, 1, 2, 3, 4].map(|i| revents(&p, i)),
        [0, POLLOUT, POLLIN, 0, POLLNVAL]
    );
    p.write_str(w as i64, "x");
    assert_eq!(p.call(SYS_POLL, &[set, 5, 0]), 4);
    assert_eq!(revents(&p, 0), POLLIN);
    // timeout with nothing ready: only the pipe read end
    let only = p.put(&entry(r, POLLIN));
    p.read_str(r as i64, 8);
    assert_eq!(p.call(SYS_POLL, &[only, 1, 3]), 0);
    assert_eq!(p.be().sleeps, [1_000_000; 3], "three 1 ms slices");
    assert_eq!(p.call(SYS_POLL, &[only, 0, 5]), 0);
    assert_eq!(
        *p.be().sleeps.last().unwrap(),
        5_000_000,
        "no descriptors: one sleep"
    );
    // a hang-up is reported without being asked for
    p.call(SYS_CLOSE, &[w as u64]);
    assert_eq!(p.call(SYS_POLL, &[only, 1, 0]), 1);
    let hup = u16::from_le_bytes(p.bytes(only + 6, 2).try_into().unwrap());
    assert_eq!(hup & 0x10, 0x10, "POLLHUP");
    assert_eq!(p.call(SYS_POLL, &[only, 2000, 0]), neg(EINVAL));
    assert_eq!(p.call(SYS_POLL, &[0xdead_0000, 1, 0]), neg(EFAULT));
}

// ------------------------------------------------------------- processes

#[test]
fn execve_is_limited_to_the_spawnable_list() {
    let mut p = Proc::new();
    let ok = p.cstr("/bin/true");
    assert_eq!(
        p.outcome(SYS_EXECVE, &[ok, 0, 0]),
        Outcome::Defer {
            nr: SYS_EXECVE,
            args: [ok, 0, 0, 0, 0, 0]
        }
    );
    let rel = p.cstr("../bin/./true");
    assert!(
        matches!(p.outcome(SYS_EXECVE, &[rel, 0, 0]), Outcome::Defer { .. }),
        "the path is normalized first"
    );
    let other = p.cstr("/bin/data");
    assert_eq!(p.call(SYS_EXECVE, &[other, 0, 0]), neg(EACCES));
    let etc = p.cstr("/etc/passwd");
    assert_eq!(p.call(SYS_EXECVE, &[etc, 0, 0]), neg(EACCES));
    let none = p.cstr("/nowhere");
    assert_eq!(
        p.call(SYS_EXECVE, &[none, 0, 0]),
        neg(EACCES),
        "not listed, whether or not it exists"
    );
    let r = p.p.refusals();
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|x| x.nr == SYS_EXECVE && x.errno == EACCES));
    assert_eq!(p.call(SYS_EXECVE, &[0xdead_0000, 0, 0]), neg(EFAULT));
    let empty = p.cstr("");
    assert_eq!(p.call(SYS_EXECVE, &[empty, 0, 0]), neg(ENOENT));
    // listed but missing, listed but not executable
    static LIST: [&[u8]; 2] = [b"/bin/gone", b"/bin/data"];
    let mut cfg = config();
    cfg.spawnable = &LIST;
    let mut q = Proc::with(cfg);
    let (gone, data) = (q.cstr("/bin/gone"), q.cstr("/bin/data"));
    assert_eq!(q.call(SYS_EXECVE, &[gone, 0, 0]), neg(ENOENT));
    assert_eq!(q.call(SYS_EXECVE, &[data, 0, 0]), neg(EACCES));
    // nothing spawnable: no process creation at all
    let mut cfg = config();
    cfg.spawnable = &[];
    let mut z = Proc::with(cfg);
    let t = z.cstr("/bin/true");
    assert_eq!(z.call(SYS_EXECVE, &[t, 0, 0]), neg(EACCES));
}

#[test]
fn clone_allows_threads_and_gates_processes_and_namespaces() {
    let mut p = Proc::new();
    let thread = 0x1_0000u64 | 0x100 | 0x200 | 0x400; // CLONE_THREAD|VM|FS|FILES
    assert!(matches!(
        p.outcome(SYS_CLONE, &[thread, 0x7000, 0, 0, 0]),
        Outcome::Defer { nr: SYS_CLONE, .. }
    ));
    assert!(
        matches!(
            p.outcome(SYS_CLONE, &[17, 0, 0, 0, 0]),
            Outcome::Defer { .. }
        ),
        "fork-style with a spawnable program"
    );
    for ns in [
        0x2_0000u64,
        0x0400_0000,
        0x1000_0000,
        0x2000_0000,
        0x4000_0000,
    ] {
        assert_eq!(
            p.call(SYS_CLONE, &[thread | ns, 0, 0, 0, 0]),
            neg(EPERM),
            "namespace flag {ns:#x}"
        );
    }
    // clone3 reads its flags from the argument block
    let mut args = vec![0u8; 88];
    args[..8].copy_from_slice(&thread.to_le_bytes());
    let a = p.put(&args);
    assert!(matches!(
        p.outcome(SYS_CLONE3, &[a, 88]),
        Outcome::Defer { nr: SYS_CLONE3, .. }
    ));
    args[..8].copy_from_slice(&(thread | 0x1000_0000).to_le_bytes());
    let b = p.put(&args);
    assert_eq!(p.call(SYS_CLONE3, &[b, 88]), neg(EPERM));
    assert_eq!(p.call(SYS_CLONE3, &[a, 8]), neg(EINVAL), "block too small");
    assert_eq!(p.call(SYS_CLONE3, &[0xdead_0000, 88]), neg(EFAULT));
    // without a spawnable program, only threads
    let mut cfg = config();
    cfg.spawnable = &[];
    let mut q = Proc::with(cfg);
    assert!(matches!(
        q.outcome(SYS_CLONE, &[thread, 0, 0, 0, 0]),
        Outcome::Defer { .. }
    ));
    assert_eq!(
        q.call(SYS_CLONE, &[17, 0, 0, 0, 0]),
        neg(EPERM),
        "fork is not allowed"
    );
    assert!(matches!(
        p.outcome(SYS_WAIT4, &[u64::MAX, 0, 0, 0]),
        Outcome::Defer { nr: SYS_WAIT4, .. }
    ));
}

#[test]
fn exit_and_signals() {
    let mut p = Proc::new();
    assert_eq!(
        p.outcome(SYS_EXIT, &[3]),
        Outcome::Exit {
            code: 3,
            group: false
        }
    );
    assert_eq!(
        p.outcome(SYS_EXIT_GROUP, &[(-1i32) as u32 as u64]),
        Outcome::Exit {
            code: -1,
            group: true
        }
    );
    assert_eq!(p.call(SYS_TGKILL, &[4242, 4242, 0]), 0, "signal 0 probes");
    assert_eq!(p.call(SYS_TGKILL, &[4242, 9999, 0]), neg(ESRCH));
    assert_eq!(
        p.call(SYS_TGKILL, &[1, 1, 9]),
        neg(ESRCH),
        "no other process can be signalled"
    );
    assert_eq!(
        p.outcome(SYS_TGKILL, &[4242, 4242, 6]),
        Outcome::Defer {
            nr: SYS_TGKILL,
            args: [4242, 4242, 6, 0, 0, 0]
        }
    );
    assert_eq!(p.call(SYS_TGKILL, &[4242, 4242, 65]), neg(EINVAL));
}

#[test]
fn exec_committed_resets_what_exec_resets() {
    let mut p = Proc::new();
    let keep = p.open("/etc/passwd", O_RDONLY);
    let gone = p.open("/tmp/c", O_CREAT | O_RDWR | O_CLOEXEC);
    mmap(&mut p, 0, 4096, RW, PRIV_ANON, -1, 0);
    p.call(SYS_ARCH_PRCTL, &[ARCH_SET_FS, 0x2000_0000]);
    p.call(SYS_SET_TID_ADDRESS, &[0x3000]);
    let live = p.be().live();
    p.p.exec_committed();
    assert_eq!(
        p.call(SYS_FCNTL, &[keep as u64, F_GETFD, 0]),
        0,
        "an ordinary descriptor survives"
    );
    assert_eq!(
        p.call(SYS_FCNTL, &[gone as u64, F_GETFD, 0]),
        neg(EBADF),
        "a close-on-exec one does not"
    );
    assert_eq!(p.be().live(), live - 1, "and its object was closed");
    assert!(p.p.space().entries().is_empty());
    assert_eq!((p.p.fs_base(), p.p.clear_child_tid()), (0, 0));
}

// ----------------------------------------------------------------- policy

#[test]
fn denied_missing_and_stubbed_calls() {
    let mut p = Proc::new();
    assert_eq!(p.call(SYS_PTRACE, &[0, 0, 0, 0]), neg(EPERM));
    assert_eq!(p.call(SYS_MOUNT, &[0; 5]), neg(EPERM));
    assert_eq!(p.call(SYS_SETUID, &[0]), neg(EPERM));
    assert_eq!(p.call(SYS_REBOOT, &[0; 4]), neg(EPERM));
    assert_eq!(p.call(SYS_SOCKET, &[2, 1, 0]), neg(EAFNOSUPPORT));
    assert_eq!(p.call(SYS_RSEQ, &[0; 4]), neg(ENOSYS));
    assert_eq!(p.call(9999, &[]), neg(ENOSYS));
    assert_eq!(p.call(SYS_RT_SIGACTION, &[2, 0, 0, 8]), 0);
    assert_eq!(p.call(SYS_GETSID, &[0]), 1);
    assert_eq!(p.p.refused_total(), 6, "stubs are not refusals");
    let r = p.p.refusals();
    assert_eq!((r[0].nr, r[0].errno), (SYS_PTRACE, EPERM));
    assert_eq!((r[4].nr, r[4].errno), (SYS_SOCKET, EAFNOSUPPORT));
}

#[test]
fn the_audit_ring_keeps_the_most_recent() {
    let mut p = Proc::new();
    for _ in 0..40 {
        p.call(SYS_PTRACE, &[0; 4]);
    }
    p.call(SYS_MOUNT, &[0; 5]);
    assert_eq!(p.p.refused_total(), 41);
    let r = p.p.refusals();
    assert_eq!(r.len(), 16);
    assert_eq!(r[15].nr, SYS_MOUNT, "newest last");
    assert_eq!(r[0].nr, SYS_PTRACE);
}

#[test]
fn a_number_beyond_32_bits_is_not_another_call() {
    let mut p = Proc::new();
    let mut a = [0u64; 6];
    a[0] = 1;
    let mut mem = p.mem.clone();
    let r = p.p.syscall(&mut mem, (1u64 << 32) + u64::from(SYS_EXIT), a);
    assert_eq!(
        r,
        Outcome::Return(neg(ENOSYS)),
        "the high bits do not alias exit"
    );
}

#[test]
fn every_handled_call_has_an_implementation() {
    for &n in HANDLED {
        assert_eq!(disposition(n), Disp::Handled);
        let mut p = Proc::new();
        let r = p.outcome(n, &[0; 6]);
        assert_ne!(
            r,
            Outcome::Return(neg(ENOSYS)),
            "syscall {n} is listed as handled but not implemented"
        );
    }
}

#[test]
fn no_call_with_zero_arguments_crashes_the_service() {
    // Whatever a program passes, the answer is a value, an exit or a deferral.
    for n in 0..460u32 {
        let mut p = Proc::new();
        let _ = p.outcome(n, &[0; 6]);
        let _ = p.outcome(n, &[u64::MAX; 6]);
        let _ = p.outcome(n, &[SCRATCH; 6]);
        assert_eq!(p.p.fds().check(), Ok(()), "syscall {n}");
        assert_eq!(p.p.space().check(), Ok(()), "syscall {n}");
    }
}
