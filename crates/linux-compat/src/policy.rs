//! What the personality does with each Linux system call: carry it out
//! (`Handled`), pretend it succeeded without effect (`Stub`), refuse it as a
//! privileged or dangerous operation the program must never reach (`Deny`),
//! or report that it does not exist (`Missing`, `ENOSYS`, which well-behaved
//! libc code treats as "not available" and falls back).
//!
//! Every number below is a constant generated from the kernel header
//! (table.rs), so a misspelt name does not compile. The sets are disjoint
//! (a test checks it) and every system call of a real toolchain build is in
//! `Handled` or `Stub` (a test replays the measured list).

use crate::errno::{Errno, EAFNOSUPPORT, ENOSYS, EPERM};
use crate::table::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disp {
    Handled,
    /// Succeeds with this return value and no effect.
    Stub(i64),
    /// Refused with this errno.
    Deny(Errno),
    Missing,
}

pub const HANDLED: &[u32] = &[
    SYS_READ,
    SYS_WRITE,
    SYS_PREAD64,
    SYS_PWRITE64,
    SYS_READV,
    SYS_WRITEV,
    SYS_OPEN,
    SYS_OPENAT,
    SYS_CLOSE,
    SYS_LSEEK,
    SYS_FSTAT,
    SYS_STAT,
    SYS_LSTAT,
    SYS_NEWFSTATAT,
    SYS_STATX,
    SYS_ACCESS,
    SYS_FACCESSAT,
    SYS_FACCESSAT2,
    SYS_READLINK,
    SYS_READLINKAT,
    SYS_GETCWD,
    SYS_CHDIR,
    SYS_FCHDIR,
    SYS_MKDIR,
    SYS_MKDIRAT,
    SYS_UNLINK,
    SYS_UNLINKAT,
    SYS_RMDIR,
    SYS_RENAME,
    SYS_RENAMEAT,
    SYS_RENAMEAT2,
    SYS_GETDENTS64,
    SYS_DUP,
    SYS_DUP2,
    SYS_DUP3,
    SYS_FCNTL,
    SYS_IOCTL,
    SYS_PIPE,
    SYS_PIPE2,
    SYS_SOCKETPAIR,
    SYS_RECVFROM,
    SYS_SENDTO,
    SYS_POLL,
    SYS_MMAP,
    SYS_MUNMAP,
    SYS_MPROTECT,
    SYS_MADVISE,
    SYS_BRK,
    SYS_UNAME,
    SYS_GETRANDOM,
    SYS_CLOCK_GETTIME,
    SYS_GETTIMEOFDAY,
    SYS_NANOSLEEP,
    SYS_CLOCK_NANOSLEEP,
    SYS_EXIT,
    SYS_EXIT_GROUP,
    SYS_GETPID,
    SYS_GETTID,
    SYS_GETPPID,
    SYS_GETUID,
    SYS_GETEUID,
    SYS_GETGID,
    SYS_GETEGID,
    SYS_GETPGRP,
    SYS_SET_TID_ADDRESS,
    SYS_ARCH_PRCTL,
    SYS_FUTEX,
    SYS_PRLIMIT64,
    SYS_GETRLIMIT,
    SYS_SCHED_GETAFFINITY,
    SYS_SCHED_YIELD,
    SYS_FTRUNCATE,
    SYS_FSYNC,
    SYS_FDATASYNC,
    SYS_FLOCK,
    SYS_UTIMENSAT,
    SYS_UMASK,
    SYS_EXECVE,
    SYS_CLONE,
    SYS_CLONE3,
    SYS_WAIT4,
    SYS_TGKILL,
    SYS_FSTATFS,
    SYS_STATFS,
    SYS_LINKAT,
    SYS_SYMLINK,
    SYS_SYMLINKAT,
];

/// Succeed without doing anything: nothing here changes what the program
/// can reach, and libc or the Rust runtime only wants to hear "fine".
pub const STUBBED: &[(u32, i64)] = &[
    (SYS_RT_SIGACTION, 0),
    (SYS_RT_SIGPROCMASK, 0),
    (SYS_RT_SIGRETURN, 0),
    (SYS_SIGALTSTACK, 0),
    (SYS_SET_ROBUST_LIST, 0),
    (SYS_PRCTL, 0),
    (SYS_CHMOD, 0),
    (SYS_FCHMOD, 0),
    (SYS_FCHMODAT, 0),
    (SYS_FCHMODAT2, 0),
    (SYS_GETPGID, 1),
    (SYS_GETSID, 1),
    (SYS_SETPGID, 0),
    (SYS_SETSID, 1),
    (SYS_MLOCK, 0),
    (SYS_MUNLOCK, 0),
    (SYS_MLOCKALL, 0),
    (SYS_MUNLOCKALL, 0),
    (SYS_SYNC, 0),
    (SYS_SETRLIMIT, 0),
    (SYS_GETRUSAGE, 0),
    (SYS_SCHED_SETAFFINITY, 0),
    (SYS_SCHED_GETPARAM, 0),
    (SYS_SCHED_GETSCHEDULER, 0),
    (SYS_CAPGET, 0),
];

/// Privileged or dangerous: the program is told it may not.
pub const DENIED: &[u32] = &[
    SYS_PTRACE,
    SYS_MOUNT,
    SYS_UMOUNT2,
    SYS_REBOOT,
    SYS_KEXEC_LOAD,
    SYS_KEXEC_FILE_LOAD,
    SYS_INIT_MODULE,
    SYS_FINIT_MODULE,
    SYS_DELETE_MODULE,
    SYS_SETUID,
    SYS_SETGID,
    SYS_SETREUID,
    SYS_SETREGID,
    SYS_SETRESUID,
    SYS_SETRESGID,
    SYS_SETFSUID,
    SYS_SETFSGID,
    SYS_SETGROUPS,
    SYS_SETNS,
    SYS_UNSHARE,
    SYS_CHROOT,
    SYS_PIVOT_ROOT,
    SYS_SWAPON,
    SYS_SWAPOFF,
    SYS_ACCT,
    SYS_SETTIMEOFDAY,
    SYS_CLOCK_SETTIME,
    SYS_ADJTIMEX,
    SYS_CLOCK_ADJTIME,
    SYS_BPF,
    SYS_PERF_EVENT_OPEN,
    SYS_IOPL,
    SYS_IOPERM,
    SYS_SYSLOG,
    SYS_QUOTACTL,
    SYS_PROCESS_VM_READV,
    SYS_PROCESS_VM_WRITEV,
    SYS_KCMP,
    SYS_USERFAULTFD,
    SYS_OPEN_BY_HANDLE_AT,
    SYS_NAME_TO_HANDLE_AT,
    SYS_MKNOD,
    SYS_MKNODAT,
    SYS_FANOTIFY_INIT,
    SYS_KEYCTL,
    SYS_ADD_KEY,
    SYS_REQUEST_KEY,
    SYS_CAPSET,
    SYS_SECCOMP,
    SYS_PERSONALITY,
    SYS_VHANGUP,
    SYS_MODIFY_LDT,
    SYS_LOOKUP_DCOOKIE,
    SYS_PIDFD_GETFD,
    SYS_MOVE_PAGES,
    SYS_MIGRATE_PAGES,
    SYS_MBIND,
    SYS_SET_MEMPOLICY,
];

/// Calls a runtime makes to find out whether a feature exists and copes with
/// `ENOSYS`: they stay `Missing` on purpose (glibc and the Rust runtime
/// register `rseq` and fall back when the kernel says no).
pub const PROBED: &[u32] = &[SYS_RSEQ];

pub fn disposition(nr: u32) -> Disp {
    if HANDLED.contains(&nr) {
        Disp::Handled
    } else if let Some((_, v)) = STUBBED.iter().find(|(n, _)| *n == nr) {
        Disp::Stub(*v)
    } else if DENIED.contains(&nr) {
        Disp::Deny(EPERM)
    } else if nr == SYS_SOCKET {
        // No network unless a network capability was delegated.
        Disp::Deny(EAFNOSUPPORT)
    } else {
        Disp::Missing
    }
}

/// The errno a disposition other than `Handled` or `Stub` turns into.
pub fn refusal(d: Disp) -> Errno {
    match d {
        Disp::Deny(e) => e,
        _ => ENOSYS,
    }
}
