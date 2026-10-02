//! Whole programs: the start-up and exit of a static Rust program, a program
//! that tries everything it should not, and a long random run that checks
//! the invariants of every table after every call.

mod common;

use common::*;
use linux_compat::abi::*;
use linux_compat::errno::*;
use linux_compat::fdtable::Kind;
use linux_compat::table::*;
use linux_compat::vma::{PROT_EXEC, PROT_READ, PROT_WRITE};
use linux_compat::Outcome;

const RW: u64 = (PROT_READ | PROT_WRITE) as u64;
const PRIV_ANON: u64 = MAP_PRIVATE | MAP_ANONYMOUS;

/// What a static musl Rust binary does between `_start` and `main`, then a
/// small `main`: read a file, print, exit.
#[test]
fn a_static_rust_program_runs_to_completion() {
    let mut p = Proc::new();
    // runtime start-up
    assert_eq!(p.call(SYS_ARCH_PRCTL, &[ARCH_SET_FS, 0x2000_1000]), 0);
    assert_eq!(p.call(SYS_SET_TID_ADDRESS, &[0x2000_1100]), 4242);
    assert_eq!(p.call(SYS_RT_SIGACTION, &[11, 0, 0, 8]), 0);
    assert_eq!(p.call(SYS_RT_SIGACTION, &[7, 0, 0, 8]), 0);
    assert_eq!(p.call(SYS_RT_SIGPROCMASK, &[0, 0, 0, 8]), 0);
    assert_eq!(
        p.call(SYS_RSEQ, &[0x2000_2000, 32, 0, 0]),
        neg(ENOSYS),
        "glibc/musl fall back without rseq"
    );
    let lim = p.buf(16);
    assert_eq!(p.call(SYS_PRLIMIT64, &[0, RLIMIT_STACK as u64, 0, lim]), 0);
    assert_eq!(p.u64_at(lim), 8 << 20);
    // an alternate signal stack with a guard page below it
    let st = p.call(SYS_MMAP, &[0, 5 * 4096, RW, PRIV_ANON, u64::MAX, 0]) as u64;
    assert_eq!(p.call(SYS_MPROTECT, &[st, 4096, 0]), 0);
    assert_eq!(p.mem.prot_at(st), Some(0));
    let ss = p.put(
        &[
            st.to_le_bytes().to_vec(),
            vec![0; 8],
            (4 * 4096u64).to_le_bytes().to_vec(),
        ]
        .concat(),
    );
    assert_eq!(p.call(SYS_SIGALTSTACK, &[ss, 0]), 0);
    // hash seed
    let seed = p.buf(16);
    assert_eq!(p.call(SYS_GETRANDOM, &[seed, 16, 1]), 16);
    // heap
    let brk = p.call(SYS_BRK, &[0]);
    assert_eq!(p.call(SYS_BRK, &[brk as u64 + 0x21000]), brk + 0x21000);
    // main: read a file, look at it, print, exit
    let fd = p.open("/etc/passwd", O_RDONLY | O_CLOEXEC);
    let sb = p.buf(144);
    assert_eq!(p.call(SYS_FSTAT, &[fd as u64, sb]), 0);
    let size = p.u64_at(sb + 48);
    let (_, text) = p.read_str(fd, size as usize + 1);
    assert_eq!(text.len() as u64, size);
    assert_eq!(p.read_str(fd, 8).0, 0);
    assert_eq!(p.call(SYS_CLOSE, &[fd as u64]), 0);
    let first = text.lines().next().unwrap().to_string();
    assert_eq!(
        p.write_str(1, &format!("{first}\n")),
        first.len() as i64 + 1
    );
    assert_eq!(
        p.outcome(SYS_EXIT_GROUP, &[0]),
        Outcome::Exit {
            code: 0,
            group: true
        }
    );
    // what the world saw
    assert_eq!(p.be().tty_out, b"root:x:0:0:root:/root:/bin/sh\n");
    assert!(
        p.p.refusals().is_empty(),
        "a well-behaved program is refused nothing"
    );
    assert_eq!(p.p.fds().open_count(), 3);
    assert_eq!(p.be().live(), 1);
    assert_eq!(p.p.fds().check(), Ok(()));
    assert_eq!(p.p.space().check(), Ok(()));
}

/// Everything a hostile or careless program might try, and what each attempt
/// gets: nothing outside the delegated root, no new authority.
#[test]
fn a_hostile_program_gets_nothing_it_was_not_given() {
    let mut p = Proc::new();
    p.be().add(
        b"/secret",
        FileKind::File,
        0o600,
        b"inside the root, so readable",
    );
    // paths that try to climb out end up inside the root
    for path in [
        "/../../../etc/passwd",
        "../../etc/passwd",
        "/tmp/../../../../etc/passwd",
        "//etc//passwd",
        "/etc/./passwd",
    ] {
        let fd = p.open(path, O_RDONLY);
        assert!(fd >= 0, "{path}");
        assert_eq!(
            p.read_str(fd, 4).1,
            "root",
            "{path} is the file inside the root"
        );
        p.call(SYS_CLOSE, &[fd as u64]);
    }
    let fd = p.open("/../secret", O_RDONLY);
    assert!(fd >= 0, "the root of the program is the root of its world");
    // links are stored as written but resolved by the backend inside its root
    let (t, l) = (p.cstr("/../../etc/passwd"), p.cstr("/tmp/escape"));
    assert_eq!(p.call(SYS_SYMLINK, &[t, l]), 0);
    let fd = p.open("/tmp/escape", O_RDONLY);
    assert_eq!(p.read_str(fd, 4).1, "root");
    // a link to itself
    let (t, l) = (p.cstr("/tmp/loop"), p.cstr("/tmp/loop"));
    assert_eq!(p.call(SYS_SYMLINK, &[t, l]), 0);
    assert_eq!(p.open("/tmp/loop", O_RDONLY), neg(ELOOP));
    // the privileged calls
    assert_eq!(p.call(SYS_MOUNT, &[0; 5]), neg(EPERM));
    assert_eq!(p.call(SYS_PTRACE, &[16, 1, 0, 0]), neg(EPERM));
    assert_eq!(p.call(SYS_SETUID, &[0]), neg(EPERM));
    assert_eq!(p.call(SYS_CHROOT, &[0]), neg(EPERM));
    assert_eq!(p.call(SYS_SOCKET, &[2, 1, 0]), neg(EAFNOSUPPORT));
    assert_eq!(p.call(SYS_REBOOT, &[0; 4]), neg(EPERM));
    assert_eq!(p.call(SYS_CLOCK_SETTIME, &[0, 0]), neg(EPERM));
    assert_eq!(p.call(SYS_BPF, &[0; 3]), neg(EPERM));
    assert_eq!(p.call(SYS_UNSHARE, &[0x2000_0000]), neg(EPERM));
    assert_eq!(p.call(SYS_MKNOD, &[t, 0o60000, 0]), neg(EPERM));
    // a second program, a namespace, writable code
    let sh = p.cstr("/bin/data");
    assert_eq!(p.call(SYS_EXECVE, &[sh, 0, 0]), neg(EACCES));
    assert_eq!(
        p.call(SYS_CLONE, &[0x2_0000 | 0x1_0000, 0, 0, 0, 0]),
        neg(EPERM)
    );
    let wx = (PROT_READ | PROT_WRITE | PROT_EXEC) as u64;
    assert_eq!(
        p.call(SYS_MMAP, &[0, 4096, wx, PRIV_ANON, u64::MAX, 0]),
        neg(EPERM)
    );
    // the kernel's addresses and the service's own memory are not the program's
    assert_eq!(
        p.call(SYS_READ, &[0, 0xffff_8000_0000_0000, 8]),
        neg(EFAULT)
    );
    assert_eq!(
        p.call(
            SYS_MMAP,
            &[
                0xffff_8000_0000_0000,
                4096,
                RW,
                PRIV_ANON | MAP_FIXED,
                u64::MAX,
                0
            ]
        ),
        neg(ENOMEM)
    );
    assert_eq!(
        p.call(SYS_ARCH_PRCTL, &[ARCH_SET_FS, 0xffff_8000_0000_0000]),
        neg(EPERM)
    );
    // /proc and /dev are whatever the backend root has: nothing here
    assert_eq!(p.open("/proc/self/mem", O_RDWR), neg(ENOENT));
    // every refusal is on the record, in order
    let seen: Vec<(u32, i32)> = p.p.refusals().iter().map(|r| (r.nr, r.errno.0)).collect();
    assert_eq!(
        seen,
        [
            (SYS_MOUNT, 1),
            (SYS_PTRACE, 1),
            (SYS_SETUID, 1),
            (SYS_CHROOT, 1),
            (SYS_SOCKET, 97),
            (SYS_REBOOT, 1),
            (SYS_CLOCK_SETTIME, 1),
            (SYS_BPF, 1),
            (SYS_UNSHARE, 1),
            (SYS_MKNOD, 1),
            (SYS_EXECVE, 13),
            (SYS_CLONE, 1),
            (SYS_MMAP, 1),
        ]
    );
    // and every path that reached the backend was absolute and normalized (the backend asserts it)
    assert!(p.be().paths_seen.len() > 10);
}

// ------------------------------------------------------------- random run

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

/// The descriptors of a kind that are open now.
fn fds_of(p: &Proc, kind: Kind) -> Vec<u64> {
    (0..64u64)
        .filter(|fd| p.p.fds().get(*fd as usize).is_ok_and(|o| o.kind == kind))
        .collect()
}

const FD_POOL: [u64; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 40, 63, 64, 1000, u64::MAX];

const PATHS: [&str; 22] = [
    "/",
    "/tmp",
    "/tmp/a",
    "/tmp/b",
    "/tmp/b/c",
    "a",
    "b/c",
    "../x",
    "/etc/passwd",
    "/tmp/../../etc/passwd",
    "",
    ".",
    "..",
    "/tmp/",
    "//tmp//a//",
    "/tmp/link",
    "/tmp/loop",
    "/bin/true",
    "/dev/tty",
    "/home/z",
    "/tmp/a/b/c/d/e/f",
    "x\u{1}y",
];

fn run_random(seed: u64, calls: usize) -> std::collections::BTreeMap<u32, (u32, u32)> {
    let mut stats = std::collections::BTreeMap::<u32, (u32, u32)>::new();
    let mut p = Proc::new();
    let mut r = Rng(seed);
    let (t, l) = (p.cstr("/etc/passwd"), p.cstr("/tmp/link"));
    p.call(SYS_SYMLINK, &[t, l]);
    let (t, l) = (p.cstr("/tmp/loop"), p.cstr("/tmp/loop"));
    p.call(SYS_SYMLINK, &[t, l]);
    let mut paths: Vec<u64> = PATHS.iter().map(|s| p.cstr(s)).collect();
    paths.push(p.put(&vec![b'q'; 300]));
    paths.push(p.put(&vec![b'q'; 5000]));
    let bufs: Vec<u64> = (0..4).map(|_| p.buf(8192)).collect();
    let mut mapped: Vec<u64> = Vec::new();
    // descriptors the program was given, so that later calls can use them
    let mut opened: Vec<u64> = Vec::new();
    let flag_pool = [
        O_RDONLY,
        O_WRONLY,
        O_RDWR,
        O_CREAT | O_RDWR,
        O_CREAT | O_EXCL | O_RDWR,
        O_TRUNC | O_WRONLY,
        O_APPEND | O_WRONLY,
        O_DIRECTORY,
        O_NOFOLLOW,
        O_CLOEXEC | O_RDONLY,
        O_NONBLOCK | O_RDWR,
        0o7777_7777,
    ];
    // `close` is listed often: descriptors are made by many calls and the table must not stay full.
    let nrs = [
        SYS_OPEN,
        SYS_OPENAT,
        SYS_CLOSE,
        SYS_CLOSE,
        SYS_CLOSE,
        SYS_CLOSE,
        SYS_CLOSE,
        SYS_CLOSE,
        SYS_CLOSE,
        SYS_READ,
        SYS_WRITE,
        SYS_PREAD64,
        SYS_PWRITE64,
        SYS_LSEEK,
        SYS_DUP,
        SYS_DUP2,
        SYS_DUP3,
        SYS_FCNTL,
        SYS_FSTAT,
        SYS_STAT,
        SYS_LSTAT,
        SYS_MKDIR,
        SYS_UNLINK,
        SYS_RMDIR,
        SYS_RENAME,
        SYS_LINKAT,
        SYS_SYMLINK,
        SYS_READLINK,
        SYS_GETDENTS64,
        SYS_CHDIR,
        SYS_FCHDIR,
        SYS_GETCWD,
        SYS_PIPE2,
        SYS_SOCKETPAIR,
        SYS_MMAP,
        SYS_MUNMAP,
        SYS_MPROTECT,
        SYS_MADVISE,
        SYS_BRK,
        SYS_POLL,
        SYS_FTRUNCATE,
        SYS_RECVFROM,
        SYS_SENDTO,
        SYS_UNLINKAT,
        SYS_MKDIRAT,
        SYS_READV,
        SYS_WRITEV,
        SYS_FACCESSAT,
        SYS_NEWFSTATAT,
        SYS_STATX,
    ];
    for i in 0..calls {
        let nr = r.pick(&nrs);
        // Calls that only make sense on one kind of descriptor mostly get one.
        let wanted = match nr {
            SYS_READ | SYS_WRITE | SYS_PREAD64 | SYS_PWRITE64 | SYS_LSEEK | SYS_FTRUNCATE
            | SYS_READV | SYS_WRITEV | SYS_FSTAT => Some(Kind::File),
            SYS_GETDENTS64 | SYS_FCHDIR => Some(Kind::Dir),
            SYS_SENDTO | SYS_RECVFROM => Some(Kind::Socket),
            _ => None,
        };
        let of_kind = wanted.map(|k| fds_of(&p, k)).filter(|v| !v.is_empty());
        let fd = match (r.below(4), &of_kind) {
            (0, _) => r.below(70),
            (_, Some(v)) => r.pick(v),
            (1 | 2, None) if !opened.is_empty() => r.pick(&opened),
            _ => r.pick(&FD_POOL),
        };
        let path = r.pick(&paths);
        let buf = if r.below(8) == 0 {
            r.pick(&[0u64, 0xdead_0000, u64::MAX, 8])
        } else if !mapped.is_empty() && r.below(3) == 0 {
            r.pick(&mapped)
        } else {
            r.pick(&bufs)
        };
        let count = r.pick(&[0u64, 1, 7, 100, 4096, 5000, 8192, 100_000, u64::MAX]);
        let a: [u64; 6] = match nr {
            SYS_OPEN => [path, u64::from(r.pick(&flag_pool)), 0o644, 0, 0, 0],
            SYS_OPENAT | SYS_FACCESSAT => [
                r.pick(&[AT_FDCWD as u64, 3, 4, 5, 99]),
                path,
                u64::from(r.pick(&flag_pool)),
                0o644,
                0,
                0,
            ],
            SYS_STATX | SYS_NEWFSTATAT => [
                r.pick(&[AT_FDCWD as u64, 3, 4]),
                path,
                buf,
                r.pick(&[0, 0x100, 0x1000, 0x200]),
                buf,
                0,
            ],
            SYS_STAT | SYS_LSTAT => [path, buf, 0, 0, 0, 0],
            SYS_READ | SYS_WRITE => [fd, buf, count, 0, 0, 0],
            SYS_PREAD64 | SYS_PWRITE64 => [fd, buf, count, r.pick(&[0, 3, 4096, u64::MAX]), 0, 0],
            SYS_READV | SYS_WRITEV => [fd, buf, r.pick(&[0, 1, 2, 1024, 1025]), 0, 0, 0],
            SYS_LSEEK => [
                fd,
                r.pick(&[0, 1, 100, u64::MAX]),
                r.pick(&[0, 1, 2, 3]),
                0,
                0,
                0,
            ],
            SYS_DUP2 | SYS_DUP3 => [
                fd,
                r.pick(&[0, 3, 5, 9, 63, 64]),
                r.pick(&[0, u64::from(O_CLOEXEC), 1]),
                0,
                0,
                0,
            ],
            SYS_FCNTL => [
                fd,
                r.pick(&[0, 1, 2, 3, 4, 1030, 99]),
                r.pick(&[0, 1, 5, 64, u64::from(O_NONBLOCK)]),
                0,
                0,
                0,
            ],
            SYS_FSTAT => [fd, buf, 0, 0, 0, 0],
            SYS_MKDIR | SYS_UNLINK | SYS_RMDIR | SYS_CHDIR => [path, 0o755, 0, 0, 0, 0],
            SYS_MKDIRAT => [AT_FDCWD as u64, path, 0o755, 0, 0, 0],
            SYS_UNLINKAT => [AT_FDCWD as u64, path, r.pick(&[0, 0x200, 0x40]), 0, 0, 0],
            SYS_RENAME | SYS_SYMLINK => [path, r.pick(&paths), 0, 0, 0, 0],
            SYS_LINKAT => [AT_FDCWD as u64, path, AT_FDCWD as u64, r.pick(&paths), 0, 0],
            SYS_READLINK => [path, buf, r.pick(&[0, 1, 8, 5000]), 0, 0, 0],
            SYS_GETDENTS64 => [fd, buf, r.pick(&[0, 8, 32, 300, 8192]), 0, 0, 0],
            SYS_GETCWD => [buf, r.pick(&[0, 1, 2, 64, 5000]), 0, 0, 0, 0],
            SYS_PIPE2 => [
                buf,
                r.pick(&[0, u64::from(O_CLOEXEC), u64::from(O_NONBLOCK), 0x40]),
                0,
                0,
                0,
                0,
            ],
            SYS_SOCKETPAIR => [
                r.pick(&[1, 2]),
                r.pick(&[1, 2, 1 | u64::from(O_CLOEXEC)]),
                0,
                buf,
                0,
                0,
            ],
            SYS_MMAP => [
                r.pick(&[0, 0x3000_0000, 0x5000_0000, 0x7000_0000_0000]),
                r.pick(&[0, 1, 4096, 10_000, 1 << 20, 1 << 40]),
                r.pick(&[0, 1, 3, 5, 7, 16]),
                r.pick(&[
                    PRIV_ANON,
                    PRIV_ANON | MAP_FIXED,
                    MAP_PRIVATE,
                    MAP_SHARED | MAP_ANONYMOUS,
                    PRIV_ANON | MAP_FIXED_NOREPLACE,
                ]),
                r.pick(&[u64::MAX, 3, 4]),
                r.pick(&[0, 4096, 5]),
            ],
            SYS_MUNMAP | SYS_MPROTECT | SYS_MADVISE => [
                r.pick(&mapped_or(&mapped)) + r.pick(&[0, 0, 4096, 1]),
                r.pick(&[0, 1, 4096, 8192, 1 << 20, u64::MAX]),
                r.pick(&[0, 1, 3, 4, 5, 7, 8, 16]),
                0,
                0,
                0,
            ],
            SYS_BRK => [
                r.pick(&[
                    0,
                    0x4000_0000,
                    0x4000_0000 + 8192,
                    0x4000_0000 + (1 << 20),
                    0x1000,
                    u64::MAX,
                    0x4000_0000 + (3 << 30),
                ]),
                0,
                0,
                0,
                0,
                0,
            ],
            SYS_POLL => [
                buf,
                r.pick(&[0, 1, 2, 3, 1024, 1025]),
                r.pick(&[0, 1, 5]),
                0,
                0,
                0,
            ],
            SYS_FTRUNCATE => [fd, r.pick(&[0, 10, 5000, u64::MAX]), 0, 0, 0, 0],
            SYS_RECVFROM | SYS_SENDTO => [
                fd,
                buf,
                count.min(10_000),
                r.pick(&[0, 0x40, 2]),
                r.pick(&[0, buf]),
                r.pick(&[0, buf]),
            ],
            _ => [fd, buf, count, path, 0, 0],
        };
        let out = p.outcome(nr, &a);
        if let Outcome::Return(v) = out {
            let e = stats.entry(nr).or_default();
            if v >= 0 {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
            assert!(
                v >= 0 || (-4095..0).contains(&v),
                "call {i}: nr {nr} gave {v}"
            );
            if nr == SYS_MMAP && v > 0 {
                mapped.push(v as u64);
            }
            if v >= 0
                && matches!(
                    nr,
                    SYS_OPEN | SYS_OPENAT | SYS_DUP | SYS_DUP2 | SYS_DUP3 | SYS_FCNTL
                )
                && opened.len() < 64
            {
                opened.push(v as u64);
            }
        }
        if r.below(400) == 0 {
            p.p.exec_committed();
            mapped.clear();
        }
        assert_eq!(p.p.fds().check(), Ok(()), "call {i}: nr {nr} {a:?}");
        assert_eq!(p.p.space().check(), Ok(()), "call {i}: nr {nr} {a:?}");
        assert_eq!(
            p.be().live(),
            p.p.fds().description_count(),
            "call {i}: nr {nr} {a:?}: the backend holds exactly the objects the descriptors name"
        );
    }
    stats
}

/// Addresses for `munmap`/`mprotect`/`madvise`: what the program mapped, or
/// a free spot; never the scratch region that holds the program's strings.
fn mapped_or(mapped: &[u64]) -> Vec<u64> {
    if mapped.is_empty() {
        vec![0x5000_0000]
    } else {
        mapped.to_vec()
    }
}

#[test]
fn random_calls_keep_every_table_consistent() {
    let mut total = std::collections::BTreeMap::<u32, (u32, u32)>::new();
    for seed in [0x1234_5678_9abc_def1u64, 0xdead_beef_cafe_f00d, 7] {
        for (nr, (ok, err)) in run_random(seed, 12_000) {
            let t = total.entry(nr).or_default();
            t.0 += ok;
            t.1 += err;
        }
    }
    // The run must reach both the success and the failure side of the calls
    // that matter, or it proves little.
    for nr in [
        SYS_OPEN,
        SYS_OPENAT,
        SYS_CLOSE,
        SYS_READ,
        SYS_WRITE,
        SYS_PREAD64,
        SYS_LSEEK,
        SYS_DUP,
        SYS_DUP2,
        SYS_FCNTL,
        SYS_FSTAT,
        SYS_STAT,
        SYS_MKDIR,
        SYS_UNLINK,
        SYS_RENAME,
        SYS_GETDENTS64,
        SYS_CHDIR,
        SYS_PIPE2,
        SYS_SOCKETPAIR,
        SYS_MMAP,
        SYS_MUNMAP,
        SYS_MPROTECT,
        SYS_MADVISE,
        SYS_POLL,
        SYS_RECVFROM,
        SYS_SENDTO,
        SYS_READV,
        SYS_WRITEV,
    ] {
        let (ok, err) = total[&nr];
        assert!(
            ok >= 30 && err >= 30,
            "syscall {nr}: only {ok} successes and {err} failures in the random run"
        );
    }
}
