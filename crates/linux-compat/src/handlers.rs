//! The switch from a system-call number to the code that carries it out.
//! Every number in `policy::HANDLED` has an arm here (a test checks that no
//! handled number answers `ENOSYS`).

use crate::abi::*;
use crate::backend::Backend;
use crate::errno::*;
use crate::mem::GuestMem;
use crate::personality::{Outcome, Personality};
use crate::table::*;

/// A descriptor argument is a C `int`: only the low 32 bits count, and negative is invalid.
pub(crate) fn fd_of(v: u64) -> Result<usize, Errno> {
    usize::try_from(v as i32).map_err(|_| EBADF)
}

impl<B: Backend> Personality<B> {
    pub(crate) fn handled(&mut self, mem: &mut dyn GuestMem, n: u32, a: [u64; 6]) -> Outcome {
        let dirfd = AT_FDCWD as u64;
        let r = match n {
            SYS_EXIT => {
                return Outcome::Exit {
                    code: a[0] as i32,
                    group: false,
                }
            }
            SYS_EXIT_GROUP => {
                return Outcome::Exit {
                    code: a[0] as i32,
                    group: true,
                }
            }
            SYS_EXECVE => return self.sys_execve(mem, n, a),
            SYS_CLONE => return self.sys_clone(mem, n, a, false),
            SYS_CLONE3 => return self.sys_clone(mem, n, a, true),
            SYS_WAIT4 => return Outcome::Defer { nr: n, args: a },
            SYS_TGKILL => return self.sys_tgkill(n, a),

            SYS_READ => fd_of(a[0]).and_then(|fd| self.read_to_guest(mem, fd, a[1], a[2], None)),
            SYS_WRITE => {
                fd_of(a[0]).and_then(|fd| self.write_from_guest(mem, fd, a[1], a[2], None))
            }
            SYS_PREAD64 => {
                fd_of(a[0]).and_then(|fd| self.read_to_guest(mem, fd, a[1], a[2], Some(a[3])))
            }
            SYS_PWRITE64 => {
                fd_of(a[0]).and_then(|fd| self.write_from_guest(mem, fd, a[1], a[2], Some(a[3])))
            }
            SYS_READV => fd_of(a[0]).and_then(|fd| self.sys_vectored(mem, fd, a[1], a[2], false)),
            SYS_WRITEV => fd_of(a[0]).and_then(|fd| self.sys_vectored(mem, fd, a[1], a[2], true)),
            SYS_OPEN => self.sys_open(mem, dirfd, a[0], a[1], a[2]),
            SYS_OPENAT => self.sys_open(mem, a[0], a[1], a[2], a[3]),
            SYS_CLOSE => self.sys_close(a),
            SYS_LSEEK => fd_of(a[0]).and_then(|fd| self.sys_lseek(fd, a[1] as i64, a[2])),
            SYS_FSTAT => fd_of(a[0]).and_then(|fd| self.sys_fstat(mem, fd, a[1])),
            SYS_STAT => self.sys_stat(mem, dirfd, a[0], a[1], 0),
            SYS_LSTAT => self.sys_stat(mem, dirfd, a[0], a[1], AT_SYMLINK_NOFOLLOW),
            SYS_NEWFSTATAT => self.sys_stat(mem, a[0], a[1], a[2], a[3] as u32),
            SYS_STATX => self.sys_statx(mem, a),
            SYS_ACCESS => self.sys_access(mem, dirfd, a[0], a[1], 0),
            SYS_FACCESSAT => self.sys_access(mem, a[0], a[1], a[2], 0),
            SYS_FACCESSAT2 => self.sys_access(mem, a[0], a[1], a[2], a[3] as u32),
            SYS_READLINK => self.sys_readlink(mem, dirfd, a[0], a[1], a[2]),
            SYS_READLINKAT => self.sys_readlink(mem, a[0], a[1], a[2], a[3]),
            SYS_GETCWD => self.sys_getcwd(mem, a[0], a[1]),
            SYS_CHDIR => self.sys_chdir(mem, a[0]),
            SYS_FCHDIR => fd_of(a[0]).and_then(|fd| self.sys_fchdir(fd)),
            SYS_MKDIR => self.sys_mkdir(mem, dirfd, a[0], a[1]),
            SYS_MKDIRAT => self.sys_mkdir(mem, a[0], a[1], a[2]),
            SYS_UNLINK => self.sys_unlink(mem, dirfd, a[0], 0),
            SYS_UNLINKAT => self.sys_unlink(mem, a[0], a[1], a[2] as u32),
            SYS_RMDIR => self.sys_unlink(mem, dirfd, a[0], AT_REMOVEDIR),
            SYS_RENAME => self.sys_rename(mem, [dirfd, a[0], dirfd, a[1], 0], 0),
            SYS_RENAMEAT => self.sys_rename(mem, [a[0], a[1], a[2], a[3], 0], 0),
            SYS_RENAMEAT2 => self.sys_rename(mem, [a[0], a[1], a[2], a[3], 0], a[4] as u32),
            SYS_LINKAT => self.sys_link(mem, [a[0], a[1], a[2], a[3], 0], a[4] as u32),
            SYS_SYMLINK => self.sys_symlink(mem, a[0], dirfd, a[1]),
            SYS_SYMLINKAT => self.sys_symlink(mem, a[0], a[1], a[2]),
            SYS_GETDENTS64 => fd_of(a[0]).and_then(|fd| self.sys_getdents(mem, fd, a[1], a[2])),
            SYS_UTIMENSAT => self.sys_utimensat(mem, a),
            SYS_STATFS => self.sys_statfs(mem, false, a[0], a[1]),
            SYS_FSTATFS => self.sys_statfs(mem, true, a[0], a[1]),

            SYS_DUP => fd_of(a[0]).and_then(|fd| self.sys_dup(fd)),
            SYS_DUP2 => self.sys_dup_to(a[0], a[1], None),
            SYS_DUP3 => self.sys_dup_to(a[0], a[1], Some(a[2])),
            SYS_FCNTL => fd_of(a[0]).and_then(|fd| self.sys_fcntl(fd, a[1], a[2])),
            SYS_IOCTL => fd_of(a[0]).and_then(|fd| self.sys_ioctl(fd)),
            SYS_PIPE => self.sys_pipe(mem, a[0], 0),
            SYS_PIPE2 => self.sys_pipe(mem, a[0], a[1]),
            SYS_SOCKETPAIR => self.sys_socketpair(mem, [a[0], a[1], a[2], a[3]]),
            SYS_RECVFROM => fd_of(a[0]).and_then(|fd| self.sys_recvfrom(mem, fd, a)),
            SYS_SENDTO => fd_of(a[0]).and_then(|fd| self.sys_sendto(mem, fd, a)),
            SYS_POLL => self.sys_poll(mem, a[0], a[1], a[2] as i32),
            SYS_FTRUNCATE => fd_of(a[0]).and_then(|fd| self.sys_ftruncate(fd, a[1])),
            SYS_FSYNC | SYS_FDATASYNC | SYS_FLOCK => {
                fd_of(a[0]).and_then(|fd| self.sys_fd_noop(fd))
            }
            SYS_UMASK => {
                let old = self.umask;
                self.umask = (a[0] as u32) & 0o777;
                Ok(u64::from(old))
            }

            SYS_MMAP => self.sys_mmap(a),
            SYS_MUNMAP => self.sys_munmap(a),
            SYS_MPROTECT => self.sys_mprotect(a),
            SYS_MADVISE => self.sys_madvise(a),
            SYS_BRK => self.sys_brk(a),

            SYS_UNAME => self.sys_uname(mem, a[0]),
            SYS_GETRANDOM => self.sys_getrandom(mem, a[0], a[1], a[2]),
            SYS_CLOCK_GETTIME => self.sys_clock_gettime(mem, a[0], a[1]),
            SYS_GETTIMEOFDAY => self.sys_gettimeofday(mem, a[0], a[1]),
            SYS_NANOSLEEP => self.sys_nanosleep(mem, a[0], a[1]),
            SYS_CLOCK_NANOSLEEP => self.sys_clock_nanosleep(mem, [a[0], a[1], a[2], a[3]]),
            SYS_GETPID | SYS_GETTID | SYS_GETPGRP => Ok(u64::from(self.cfg.pid)),
            SYS_GETPPID => Ok(1),
            SYS_GETUID | SYS_GETEUID => Ok(u64::from(self.cfg.uid)),
            SYS_GETGID | SYS_GETEGID => Ok(u64::from(self.cfg.gid)),
            SYS_SET_TID_ADDRESS => Ok(self.sys_set_tid_address(a[0])),
            SYS_ARCH_PRCTL => self.sys_arch_prctl(mem, a[0], a[1]),
            SYS_FUTEX => self.sys_futex(mem, n, a),
            SYS_PRLIMIT64 => {
                if a[0] != 0 && a[0] != u64::from(self.cfg.pid) {
                    Err(ESRCH)
                } else {
                    self.sys_rlimit(mem, a[1], a[2], a[3])
                }
            }
            SYS_GETRLIMIT => self.sys_rlimit(mem, a[0], 0, a[1]),
            SYS_SCHED_GETAFFINITY => self.sys_sched_getaffinity(mem, a[0], a[1], a[2]),
            SYS_SCHED_YIELD => Ok(0),
            _ => Err(ENOSYS),
        };
        // Writable-and-executable memory is the one refusal of an otherwise
        // handled call that the audit must show.
        if r == Err(EPERM) && matches!(n, SYS_MMAP | SYS_MPROTECT) {
            self.note(n, EPERM);
        }
        Outcome::Return(ret(r))
    }
}
