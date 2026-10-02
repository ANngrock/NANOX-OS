//! Files, directories and descriptors through the personality.

mod common;

use common::*;
use linux_compat::abi::*;
use linux_compat::errno::*;
use linux_compat::table::*;

const FD: u64 = AT_FDCWD as u64;

#[test]
fn create_write_read_back() {
    let mut p = Proc::new();
    let fd = p.open("/tmp/a", O_CREAT | O_WRONLY | O_TRUNC);
    assert!(fd >= 3, "descriptors 0..3 are taken: {fd}");
    assert_eq!(p.write_str(fd, "hello, "), 7);
    assert_eq!(p.write_str(fd, "world"), 5);
    assert_eq!(p.call(SYS_CLOSE, &[fd as u64]), 0);
    assert_eq!(p.be().file("/tmp/a").unwrap(), b"hello, world");
    let fd = p.open("/tmp/a", O_RDONLY);
    assert_eq!(p.read_str(fd, 5), (5, "hello".into()));
    assert_eq!(p.read_str(fd, 100), (7, ", world".into()));
    assert_eq!(p.read_str(fd, 100).0, 0, "end of file");
    // lseek and pread
    assert_eq!(p.call(SYS_LSEEK, &[fd as u64, 3, 0]), 3);
    assert_eq!(p.read_str(fd, 2), (2, "lo".into()));
    assert_eq!(p.call(SYS_LSEEK, &[fd as u64, (-2i64) as u64, 2]), 10);
    assert_eq!(
        p.call(SYS_LSEEK, &[fd as u64, (-1i64) as u64, 0]),
        neg(EINVAL)
    );
    let b = p.buf(4);
    assert_eq!(p.call(SYS_PREAD64, &[fd as u64, b, 4, 1]), 4);
    assert_eq!(p.bytes(b, 4), b"ello");
    assert_eq!(
        p.call(SYS_LSEEK, &[fd as u64, 0, 1]),
        10,
        "pread does not move the offset"
    );
    assert_eq!(p.call(SYS_CLOSE, &[fd as u64]), 0);
    assert_eq!(
        p.be().live(),
        1,
        "only the terminal is still open in the backend"
    );
}

#[test]
fn access_modes_are_enforced() {
    let mut p = Proc::new();
    let ro = p.open("/etc/passwd", O_RDONLY);
    assert_eq!(p.write_str(ro, "x"), neg(EBADF));
    let wo = p.open("/tmp/w", O_CREAT | O_WRONLY);
    assert_eq!(p.read_str(wo, 1).0, neg(EBADF));
    assert_eq!(p.call(SYS_FTRUNCATE, &[ro as u64, 0]), neg(EBADF));
    // reading a directory
    let d = p.open("/tmp", O_RDONLY | O_DIRECTORY);
    assert_eq!(p.read_str(d, 1).0, neg(EISDIR));
    assert_eq!(p.write_str(d, "x"), neg(EBADF));
    assert_eq!(p.open("/tmp", O_RDWR), neg(EISDIR));
    assert_eq!(p.open("/etc/passwd", O_RDONLY | O_DIRECTORY), neg(ENOTDIR));
    assert_eq!(p.open("/etc/passwd/", O_RDONLY), neg(ENOTDIR));
    assert_eq!(p.open("/tmp/", O_CREAT | O_WRONLY), neg(EISDIR));
}

#[test]
fn open_flags_create_excl_trunc_append() {
    let mut p = Proc::new();
    assert_eq!(p.open("/tmp/none", O_RDONLY), neg(ENOENT));
    let fd = p.open("/tmp/f", O_CREAT | O_EXCL | O_RDWR);
    assert!(fd >= 0);
    assert_eq!(p.open("/tmp/f", O_CREAT | O_EXCL | O_RDWR), neg(EEXIST));
    p.write_str(fd, "abcdef");
    let a = p.open("/tmp/f", O_WRONLY | O_APPEND);
    assert_eq!(p.write_str(a, "XY"), 2);
    assert_eq!(p.write_str(a, "Z"), 1);
    assert_eq!(p.be().file("/tmp/f").unwrap(), b"abcdefXYZ");
    // pwrite on an append descriptor appends (Linux behaviour)
    let b = p.put(b"!");
    assert_eq!(p.call(SYS_PWRITE64, &[a as u64, b, 1, 0]), 1);
    assert_eq!(p.be().file("/tmp/f").unwrap(), b"abcdefXYZ!");
    let t = p.open("/tmp/f", O_WRONLY | O_TRUNC);
    assert!(t >= 0);
    assert_eq!(p.be().file("/tmp/f").unwrap(), b"");
    // O_CREAT under a missing or non-directory parent
    assert_eq!(p.open("/nodir/x", O_CREAT | O_WRONLY), neg(ENOENT));
    assert_eq!(p.open("/etc/passwd/x", O_CREAT | O_WRONLY), neg(ENOTDIR));
}

#[test]
fn umask_applies_to_created_files() {
    let mut p = Proc::new();
    assert_eq!(p.call(SYS_UMASK, &[0o077]), 0o022, "the previous mask");
    let path = p.cstr("/tmp/m");
    assert!(p.call(SYS_OPEN, &[path, u64::from(O_CREAT | O_WRONLY), 0o666]) >= 0);
    let b = p.buf(144);
    assert_eq!(p.call(SYS_STAT, &[path, b]), 0);
    let mode = u32::from_le_bytes(p.bytes(b + 24, 4).try_into().unwrap());
    assert_eq!(mode, S_IFREG | 0o600);
    assert_eq!(p.call(SYS_UMASK, &[0o022]), 0o077);
}

#[test]
fn stat_layouts() {
    let mut p = Proc::new();
    let path = p.cstr("/etc/passwd");
    let b = p.buf(144);
    assert_eq!(p.call(SYS_STAT, &[path, b]), 0);
    assert_eq!(p.u64_at(b + 48), 30, "st_size");
    assert_eq!(p.u32_at(b + 24), S_IFREG | 0o644);
    assert_eq!(p.u32_at(b + 28), 1000, "st_uid is the process uid");
    assert_eq!(p.u64_at(b + 16), 1, "st_nlink");
    // fstat agrees
    let fd = p.open("/etc/passwd", O_RDONLY);
    let c = p.buf(144);
    assert_eq!(p.call(SYS_FSTAT, &[fd as u64, c]), 0);
    assert_eq!(p.bytes(b, 144), p.bytes(c, 144));
    // statx
    let x = p.buf(256);
    let empty = p.cstr("");
    assert_eq!(
        p.call(
            SYS_STATX,
            &[fd as u64, empty, u64::from(AT_EMPTY_PATH), 0x7ff, x]
        ),
        0
    );
    assert_eq!(p.u64_at(x + 40), 30);
    assert_eq!(
        u16::from_le_bytes(p.bytes(x + 28, 2).try_into().unwrap()) as u32,
        S_IFREG | 0o644
    );
    // newfstatat with a relative path and AT_FDCWD
    let rel = p.cstr("etc/passwd");
    assert_eq!(p.call(SYS_NEWFSTATAT, &[FD, rel, b, 0]), 0);
    // missing, and a directory path with trailing slash on a file
    let missing = p.cstr("/nope");
    assert_eq!(p.call(SYS_STAT, &[missing, b]), neg(ENOENT));
    let slash = p.cstr("/etc/passwd/");
    assert_eq!(p.call(SYS_STAT, &[slash, b]), neg(ENOTDIR));
}

#[test]
fn lstat_does_not_follow_links() {
    let mut p = Proc::new();
    let (t, l) = (p.cstr("/etc/passwd"), p.cstr("/tmp/link"));
    assert_eq!(p.call(SYS_SYMLINK, &[t, l]), 0);
    let b = p.buf(144);
    assert_eq!(p.call(SYS_LSTAT, &[l, b]), 0);
    assert_eq!(p.u32_at(b + 24) & 0o170_000, S_IFLNK);
    assert_eq!(p.call(SYS_STAT, &[l, b]), 0);
    assert_eq!(p.u32_at(b + 24) & 0o170_000, S_IFREG);
    let out = p.buf(64);
    assert_eq!(p.call(SYS_READLINK, &[l, out, 64]), 11);
    assert_eq!(p.bytes(out, 11), b"/etc/passwd");
    assert_eq!(
        p.call(SYS_READLINK, &[t, out, 64]),
        neg(EINVAL),
        "not a link"
    );
    assert_eq!(
        p.call(SYS_READLINK, &[l, out, 4]),
        4,
        "truncated to the buffer"
    );
    assert_eq!(p.call(SYS_READLINK, &[l, out, 0]), neg(EINVAL));
    // O_NOFOLLOW on a link
    assert_eq!(p.open("/tmp/link", O_RDONLY | O_NOFOLLOW), neg(ELOOP));
    assert_eq!(p.be().live(), 1, "the refused open did not leak its object");
}

#[test]
fn directories_cwd_and_openat() {
    let mut p = Proc::new();
    let buf = p.buf(64);
    assert_eq!(p.call(SYS_GETCWD, &[buf, 64]), 2);
    assert_eq!(p.bytes(buf, 2), b"/\0");
    let tmp = p.cstr("/tmp");
    assert_eq!(p.call(SYS_CHDIR, &[tmp]), 0);
    assert_eq!(p.call(SYS_GETCWD, &[buf, 64]), 5);
    assert_eq!(p.call(SYS_GETCWD, &[buf, 4]), neg(ERANGE));
    // relative open lands in /tmp
    let fd = p.open("rel.txt", O_CREAT | O_WRONLY);
    assert!(fd >= 0);
    assert!(p.be().exists("/tmp/rel.txt"));
    // chdir to a file, to a missing directory
    let file = p.cstr("/etc/passwd");
    assert_eq!(p.call(SYS_CHDIR, &[file]), neg(ENOTDIR));
    let none = p.cstr("/nowhere");
    assert_eq!(p.call(SYS_CHDIR, &[none]), neg(ENOENT));
    // openat relative to a directory descriptor
    let etc = p.open("/etc", O_RDONLY | O_DIRECTORY);
    let name = p.cstr("passwd");
    let f = p.call(SYS_OPENAT, &[etc as u64, name, 0, 0]);
    assert!(f >= 0);
    assert_eq!(p.read_str(f, 4), (4, "root".into()));
    // ... "." and ".." stay confined and relative to that directory
    let up = p.cstr("../etc/passwd");
    assert!(p.call(SYS_OPENAT, &[etc as u64, up, 0, 0]) >= 0);
    // fchdir
    assert_eq!(p.call(SYS_FCHDIR, &[etc as u64]), 0);
    assert_eq!(p.call(SYS_GETCWD, &[buf, 64]), 5);
    assert_eq!(p.bytes(buf, 5), b"/etc\0");
    // a non-directory descriptor, a closed descriptor
    assert_eq!(p.call(SYS_OPENAT, &[f as u64, name, 0, 0]), neg(ENOTDIR));
    assert_eq!(p.call(SYS_FCHDIR, &[f as u64]), neg(ENOTDIR));
    p.call(SYS_CLOSE, &[etc as u64]);
    assert_eq!(p.call(SYS_OPENAT, &[etc as u64, name, 0, 0]), neg(EBADF));
    // absolute paths ignore the directory descriptor
    let tmpd = p.open("/tmp", O_RDONLY | O_DIRECTORY);
    let abs = p.cstr("/etc/passwd");
    assert!(p.call(SYS_OPENAT, &[tmpd as u64, abs, 0, 0]) >= 0);
}

#[test]
fn directory_slots_are_released_with_the_descriptor() {
    let mut p = Proc::new();
    for _ in 0..40 {
        let d = p.open("/tmp", O_RDONLY | O_DIRECTORY);
        assert!(d >= 0, "directory slots must be reusable");
        assert_eq!(p.call(SYS_CLOSE, &[d as u64]), 0);
    }
    // sixteen at once is the bound, the seventeenth is refused cleanly
    let mut open = Vec::new();
    for _ in 0..16 {
        open.push(p.open("/tmp", O_RDONLY | O_DIRECTORY));
    }
    assert!(open.iter().all(|d| *d >= 0));
    let live = p.be().live();
    assert_eq!(p.open("/tmp", O_RDONLY | O_DIRECTORY), neg(EMFILE));
    assert_eq!(p.be().live(), live, "the refused open closed its object");
}

#[test]
fn getdents_lists_and_resumes() {
    let mut p = Proc::new();
    for n in ["a", "b", "c", "hello"] {
        p.open(&format!("/home/{n}"), O_CREAT | O_WRONLY);
    }
    let d = p.open("/home", O_RDONLY | O_DIRECTORY);
    let buf = p.buf(4096);
    let n = p.call(SYS_GETDENTS64, &[d as u64, buf, 4096]);
    assert!(n > 0);
    let raw = p.bytes(buf, n as usize);
    let mut names = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let reclen = u16::from_le_bytes([raw[at + 16], raw[at + 17]]) as usize;
        assert_eq!(reclen % 8, 0);
        let end = raw[at + 19..].iter().position(|b| *b == 0).unwrap();
        names.push(String::from_utf8(raw[at + 19..at + 19 + end].to_vec()).unwrap());
        let dot = names.last().unwrap().starts_with('.');
        assert_eq!(raw[at + 18], if dot { DT_DIR } else { DT_REG });
        assert!(
            19 + end < reclen,
            "the name is NUL-terminated inside its record"
        );
        at += reclen;
    }
    assert_eq!(names, [".", "..", "a", "b", "c", "hello"]);
    assert_eq!(
        p.call(SYS_GETDENTS64, &[d as u64, buf, 4096]),
        0,
        "end of directory"
    );
    // rewind and read in small steps: every call makes progress and none overlaps
    assert_eq!(p.call(SYS_LSEEK, &[d as u64, 0, 0]), 0);
    let mut count = 0;
    loop {
        let n = p.call(SYS_GETDENTS64, &[d as u64, buf, 32]);
        assert!(n >= 0, "{n}");
        if n == 0 {
            break;
        }
        count += 1;
    }
    assert_eq!(count, 6);
    assert_eq!(p.call(SYS_LSEEK, &[d as u64, 0, 0]), 0);
    assert_eq!(
        p.call(SYS_GETDENTS64, &[d as u64, buf, 8]),
        neg(EINVAL),
        "too small for one entry"
    );
    let f = p.open("/etc/passwd", O_RDONLY);
    assert_eq!(p.call(SYS_GETDENTS64, &[f as u64, buf, 64]), neg(ENOTDIR));
}

#[test]
fn mkdir_unlink_rmdir_rename_link() {
    let mut p = Proc::new();
    let (d, f, g) = (p.cstr("/tmp/d"), p.cstr("/tmp/d/f"), p.cstr("/tmp/d/g"));
    assert_eq!(p.call(SYS_MKDIR, &[d, 0o777]), 0);
    assert_eq!(p.call(SYS_MKDIR, &[d, 0o777]), neg(EEXIST));
    let root = p.cstr("/");
    assert_eq!(p.call(SYS_MKDIR, &[root, 0o777]), neg(EEXIST));
    let fd = p.call(SYS_OPEN, &[f, u64::from(O_CREAT | O_WRONLY), 0o644]);
    p.write_str(fd, "data");
    assert_eq!(p.call(SYS_RMDIR, &[d]), neg(ENOTEMPTY));
    assert_eq!(p.call(SYS_UNLINK, &[d]), neg(EISDIR));
    assert_eq!(p.call(SYS_LINKAT, &[FD, f, FD, g, 0]), 0);
    let b = p.buf(144);
    p.call(SYS_STAT, &[g, b]);
    assert_eq!(p.u64_at(b + 16), 2, "two names");
    assert_eq!(p.call(SYS_UNLINK, &[f]), 0);
    assert_eq!(
        p.be().file("/tmp/d/g").unwrap(),
        b"data",
        "the data survives under the other name"
    );
    let h = p.cstr("/tmp/d/h");
    assert_eq!(p.call(SYS_RENAME, &[g, h]), 0);
    assert!(!p.be().exists("/tmp/d/g") && p.be().exists("/tmp/d/h"));
    let e = p.cstr("/tmp/e");
    assert_eq!(
        p.call(SYS_RENAME, &[d, e]),
        0,
        "renaming a directory moves what is in it"
    );
    assert!(p.be().exists("/tmp/e/h"));
    assert_eq!(
        p.call(SYS_UNLINKAT, &[FD, e, u64::from(AT_REMOVEDIR)]),
        neg(ENOTEMPTY)
    );
    let eh = p.cstr("/tmp/e/h");
    assert_eq!(p.call(SYS_UNLINKAT, &[FD, eh, 0]), 0);
    assert_eq!(p.call(SYS_UNLINKAT, &[FD, e, u64::from(AT_REMOVEDIR)]), 0);
    assert_eq!(
        p.call(SYS_UNLINKAT, &[FD, e, 0x40]),
        neg(EINVAL),
        "unknown flag"
    );
    // the root cannot be removed, moved or replaced
    assert_eq!(p.call(SYS_RMDIR, &[root]), neg(EBUSY));
    assert_eq!(p.call(SYS_UNLINK, &[root]), neg(EBUSY));
    let t = p.cstr("/tmp");
    assert_eq!(p.call(SYS_RENAME, &[root, t]), neg(EBUSY));
    assert_eq!(p.call(SYS_RENAME, &[t, root]), neg(EBUSY));
    // RENAME_NOREPLACE
    let (a, b2) = (p.cstr("/tmp/na"), p.cstr("/tmp/nb"));
    p.open("/tmp/na", O_CREAT | O_WRONLY);
    p.open("/tmp/nb", O_CREAT | O_WRONLY);
    assert_eq!(p.call(SYS_RENAMEAT2, &[FD, a, FD, b2, 1]), neg(EEXIST));
    assert_eq!(p.call(SYS_RENAMEAT2, &[FD, a, FD, b2, 0]), 0);
    assert_eq!(
        p.call(SYS_RENAMEAT2, &[FD, a, FD, b2, 4]),
        neg(EINVAL),
        "RENAME_EXCHANGE is not provided"
    );
}

#[test]
fn access_checks_owner_bits() {
    let mut p = Proc::new();
    let (data, exe) = (p.cstr("/bin/data"), p.cstr("/bin/true"));
    assert_eq!(p.call(SYS_ACCESS, &[data, 0]), 0);
    assert_eq!(p.call(SYS_ACCESS, &[data, 4]), 0, "readable");
    assert_eq!(
        p.call(SYS_ACCESS, &[data, 1]),
        neg(EACCES),
        "not executable"
    );
    assert_eq!(p.call(SYS_ACCESS, &[exe, 1]), 0);
    assert_eq!(p.call(SYS_ACCESS, &[exe, 8]), neg(EINVAL));
    let none = p.cstr("/none");
    assert_eq!(p.call(SYS_FACCESSAT2, &[FD, none, 0, 0]), neg(ENOENT));
}

#[test]
fn dup_family_shares_offsets_and_flags() {
    let mut p = Proc::new();
    let a = p.open("/etc/passwd", O_RDONLY);
    let b = p.call(SYS_DUP, &[a as u64]);
    assert!(b > a);
    assert_eq!(p.read_str(a, 4).1, "root");
    assert_eq!(p.read_str(b, 2).1, ":x", "the offset is shared");
    // dup2 onto an open descriptor closes it first
    let c = p.open("/tmp/c", O_CREAT | O_RDWR);
    let live = p.be().live();
    assert_eq!(p.call(SYS_DUP2, &[a as u64, c as u64]), c);
    assert_eq!(
        p.be().live(),
        live - 1,
        "the replaced description was closed"
    );
    assert_eq!(p.read_str(c, 2).1, ":0", "and shares the offset of a");
    assert_eq!(p.call(SYS_DUP2, &[a as u64, a as u64]), a);
    assert_eq!(p.call(SYS_DUP3, &[a as u64, a as u64, 0]), neg(EINVAL));
    assert_eq!(
        p.call(SYS_DUP3, &[a as u64, 20, 1]),
        neg(EINVAL),
        "only O_CLOEXEC is a dup3 flag"
    );
    assert_eq!(p.call(SYS_DUP3, &[a as u64, 20, u64::from(O_CLOEXEC)]), 20);
    assert_eq!(p.call(SYS_FCNTL, &[20, F_GETFD, 0]), 1);
    assert_eq!(p.call(SYS_FCNTL, &[a as u64, F_GETFD, 0]), 0);
    assert_eq!(p.call(SYS_DUP2, &[99, 5]), neg(EBADF));
    assert_eq!(
        p.call(SYS_DUP2, &[a as u64, 64]),
        neg(EBADF),
        "beyond the table"
    );
    // fcntl
    let d = p.call(SYS_FCNTL, &[a as u64, F_DUPFD_CLOEXEC, 30]);
    assert_eq!(d, 30);
    assert_eq!(p.call(SYS_FCNTL, &[d as u64, F_GETFD, 0]), 1);
    assert_eq!(p.call(SYS_FCNTL, &[d as u64, F_SETFD, 0]), 0);
    assert_eq!(p.call(SYS_FCNTL, &[d as u64, F_GETFD, 0]), 0);
    assert_eq!(p.call(SYS_FCNTL, &[a as u64, F_DUPFD, 64]), neg(EINVAL));
    assert_eq!(
        p.call(
            SYS_FCNTL,
            &[a as u64, F_SETFL, u64::from(O_NONBLOCK | O_WRONLY)]
        ),
        0
    );
    assert_eq!(
        p.call(SYS_FCNTL, &[a as u64, F_GETFL, 0]) as u32,
        O_NONBLOCK,
        "the access mode cannot be changed"
    );
    assert_eq!(p.call(SYS_FCNTL, &[a as u64, 9999, 0]), neg(EINVAL));
    assert_eq!(p.call(SYS_FCNTL, &[77, F_GETFD, 0]), neg(EBADF));
    // closing one of several keeps the object; the last close drops it
    let live = p.be().live();
    p.call(SYS_CLOSE, &[a as u64]);
    assert_eq!(p.be().live(), live);
    p.call(SYS_CLOSE, &[b as u64]);
    p.call(SYS_CLOSE, &[c as u64]);
    p.call(SYS_CLOSE, &[20]);
    p.call(SYS_CLOSE, &[30]);
    assert_eq!(p.be().live(), live - 1);
    assert_eq!(p.call(SYS_CLOSE, &[a as u64]), neg(EBADF));
    assert_eq!(p.p.fds().check(), Ok(()));
}

#[test]
fn ioctl_says_not_a_terminal() {
    let mut p = Proc::new();
    assert_eq!(p.call(SYS_IOCTL, &[1, 0x5401, 0]), neg(ENOTTY));
    assert_eq!(p.call(SYS_IOCTL, &[50, 0x5401, 0]), neg(EBADF));
}

#[test]
fn the_terminal_is_descriptors_zero_to_two() {
    let mut p = Proc::new();
    assert_eq!(p.write_str(1, "out"), 3);
    assert_eq!(p.write_str(2, "err"), 3);
    assert_eq!(p.be().tty_out, b"outerr");
    p.be().tty_in.extend(b"typed");
    assert_eq!(p.read_str(0, 100), (5, "typed".into()));
    assert_eq!(p.call(SYS_LSEEK, &[1, 0, 0]), neg(ESPIPE));
    let b = p.put(b"x");
    assert_eq!(p.call(SYS_PWRITE64, &[1, b, 1, 0]), neg(ESPIPE));
    assert_eq!(p.call(SYS_FSYNC, &[1]), 0);
    assert_eq!(p.call(SYS_FSYNC, &[60]), neg(EBADF));
}

#[test]
fn readv_and_writev() {
    let mut p = Proc::new();
    let fd = p.open("/tmp/v", O_CREAT | O_RDWR);
    let (a, b) = (p.put(b"alpha"), p.put(b"-beta"));
    let mut iov = Vec::new();
    for (base, len) in [(a, 5u64), (0, 0), (b, 5)] {
        iov.extend_from_slice(&base.to_le_bytes());
        iov.extend_from_slice(&len.to_le_bytes());
    }
    let iov = p.put(&iov);
    assert_eq!(p.call(SYS_WRITEV, &[fd as u64, iov, 3]), 10);
    assert_eq!(p.be().file("/tmp/v").unwrap(), b"alpha-beta");
    p.call(SYS_LSEEK, &[fd as u64, 0, 0]);
    let (x, y) = (p.buf(3), p.buf(20));
    let mut iov = Vec::new();
    for (base, len) in [(x, 3u64), (y, 20)] {
        iov.extend_from_slice(&base.to_le_bytes());
        iov.extend_from_slice(&len.to_le_bytes());
    }
    let iov = p.put(&iov);
    assert_eq!(
        p.call(SYS_READV, &[fd as u64, iov, 2]),
        10,
        "short second read ends the call"
    );
    assert_eq!(p.bytes(x, 3), b"alp");
    assert_eq!(p.bytes(y, 7), b"ha-beta");
}

#[test]
fn iovec_validation() {
    let mut p = Proc::new();
    let fd = p.open("/tmp/v", O_CREAT | O_RDWR);
    assert_eq!(
        p.call(SYS_WRITEV, &[fd as u64, SCRATCH, 1025]),
        neg(EINVAL),
        "more than UIO_MAXIOV"
    );
    assert_eq!(
        p.call(SYS_WRITEV, &[fd as u64, 0xdead_0000, 1]),
        neg(EFAULT)
    );
    // lengths that overflow i64 together
    let a = p.put(b"x");
    let mut iov = Vec::new();
    for len in [i64::MAX as u64, 1] {
        iov.extend_from_slice(&a.to_le_bytes());
        iov.extend_from_slice(&len.to_le_bytes());
    }
    let iov = p.put(&iov);
    assert_eq!(p.call(SYS_WRITEV, &[fd as u64, iov, 2]), neg(EINVAL));
    assert_eq!(
        p.be().file("/tmp/v").unwrap(),
        b"",
        "nothing was written before the check failed"
    );
    // an unreadable buffer in the second entry: the first is written, the error is reported next time
    let mut iov = Vec::new();
    for (base, len) in [(a, 1u64), (0xdead_0000u64, 4)] {
        iov.extend_from_slice(&base.to_le_bytes());
        iov.extend_from_slice(&len.to_le_bytes());
    }
    let iov = p.put(&iov);
    assert_eq!(
        p.call(SYS_WRITEV, &[fd as u64, iov, 2]),
        1,
        "partial progress is reported, not the fault"
    );
    assert_eq!(p.call(SYS_WRITEV, &[fd as u64, iov + 16, 1]), neg(EFAULT));
}

#[test]
fn bad_pointers_fail_cleanly_and_change_nothing() {
    let mut p = Proc::new();
    let fd = p.open("/etc/passwd", O_RDONLY);
    assert_eq!(p.call(SYS_READ, &[fd as u64, 0xdead_0000, 10]), neg(EFAULT));
    assert_eq!(
        p.call(SYS_LSEEK, &[fd as u64, 0, 1]),
        0,
        "the failed read did not move the offset"
    );
    assert_eq!(
        p.call(SYS_READ, &[fd as u64, u64::MAX - 3, 10]),
        neg(EFAULT),
        "range wraps the address space"
    );
    assert_eq!(p.call(SYS_OPEN, &[0xdead_0000, 0, 0]), neg(EFAULT));
    assert_eq!(p.call(SYS_OPEN, &[0, 0, 0]), neg(EFAULT));
    assert_eq!(p.call(SYS_STAT, &[0xdead_0000, SCRATCH]), neg(EFAULT));
    let path = p.cstr("/etc/passwd");
    assert_eq!(p.call(SYS_STAT, &[path, 0xdead_0000]), neg(EFAULT));
    // a path with no terminator before the end of mapped memory
    p.mem.poke(SCRATCH + SCRATCH_LEN - 8, b"/tmp/abc");
    assert_eq!(
        p.call(SYS_OPEN, &[SCRATCH + SCRATCH_LEN - 8, 0, 0]),
        neg(EFAULT)
    );
    // a path longer than PATH_MAX
    let long = p.put(&vec![b'a'; 5000]);
    assert_eq!(p.call(SYS_OPEN, &[long, 0, 0]), neg(ENAMETOOLONG));
    // writing to read-only memory
    p.mem.protect(SCRATCH, SCRATCH + 4096, 1);
    assert_eq!(p.call(SYS_READ, &[fd as u64, SCRATCH, 4]), neg(EFAULT));
    let empty = p.cstr("");
    assert_eq!(p.call(SYS_OPEN, &[empty, 0, 0]), neg(ENOENT));
}

#[test]
fn huge_counts_are_clamped() {
    let mut p = Proc::new();
    let fd = p.open("/etc/passwd", O_RDONLY);
    let b = p.buf(64);
    // count larger than the file (and than MAX_RW) reads what there is
    assert_eq!(p.call(SYS_READ, &[fd as u64, b, 30]), 30);
    assert_eq!(
        p.call(SYS_READ, &[fd as u64, b, u64::MAX]),
        0,
        "clamped to MAX_RW, and the file is at its end"
    );
    assert_eq!(
        p.call(SYS_READ, &[fd as u64, u64::MAX - 3, 10]),
        neg(EFAULT),
        "a range that wraps is a fault"
    );
}

#[test]
fn ftruncate_and_fd_noops() {
    let mut p = Proc::new();
    let fd = p.open("/tmp/t", O_CREAT | O_RDWR);
    p.write_str(fd, "0123456789");
    assert_eq!(p.call(SYS_FTRUNCATE, &[fd as u64, 4]), 0);
    assert_eq!(p.be().file("/tmp/t").unwrap(), b"0123");
    assert_eq!(p.call(SYS_FTRUNCATE, &[fd as u64, 8]), 0);
    assert_eq!(p.be().file("/tmp/t").unwrap(), b"0123\0\0\0\0");
    assert_eq!(p.call(SYS_FTRUNCATE, &[fd as u64, u64::MAX]), neg(EINVAL));
    assert_eq!(p.call(SYS_FDATASYNC, &[fd as u64]), 0);
    assert_eq!(p.call(SYS_FLOCK, &[fd as u64, 2]), 0);
    assert_eq!(
        p.call(SYS_FTRUNCATE, &[1, 0]),
        neg(EINVAL),
        "not a regular file"
    );
    let path = p.cstr("/tmp/t");
    assert_eq!(p.call(SYS_UTIMENSAT, &[FD, path, 0, 0]), 0);
    assert_eq!(
        p.call(SYS_UTIMENSAT, &[FD, path, 0xdead_0000, 0]),
        neg(EFAULT)
    );
    let none = p.cstr("/tmp/none");
    assert_eq!(p.call(SYS_UTIMENSAT, &[FD, none, 0, 0]), neg(ENOENT));
    let sb = p.buf(120);
    assert_eq!(p.call(SYS_STATFS, &[path, sb]), 0);
    assert_eq!(p.u64_at(sb + 8), 4096, "f_bsize");
    assert_eq!(p.call(SYS_FSTATFS, &[fd as u64, sb]), 0);
    assert_eq!(p.call(SYS_FSTATFS, &[90, sb]), neg(EBADF));
}

#[test]
fn stat_fields_beyond_the_basics() {
    let mut p = Proc::new();
    let path = p.cstr("/etc/passwd");
    let b = p.buf(144);
    assert_eq!(p.call(SYS_STAT, &[path, b]), 0);
    assert_eq!(p.u64_at(b + 56), 4096, "st_blksize");
    assert_eq!(
        p.u64_at(b + 64),
        1,
        "st_blocks (512-byte units, rounded up)"
    );
    for at in [72, 88, 104] {
        assert_eq!(p.u64_at(b + at), 1_700_000_000, "time at {at}");
        assert_eq!(p.u64_at(b + at + 8), 0);
    }
    assert_eq!(p.u32_at(b + 32), 1001, "st_gid");
    let x = p.buf(256);
    assert_eq!(p.call(SYS_STATX, &[FD, path, 0, 0x7ff, x]), 0);
    assert_eq!(p.u32_at(x), 0x7ff, "stx_mask");
    assert_eq!(p.u32_at(x + 4), 4096, "stx_blksize");
    assert_eq!(p.u32_at(x + 16), 1, "stx_nlink");
    assert_eq!((p.u32_at(x + 20), p.u32_at(x + 24)), (1000, 1001));
    assert_eq!(p.u64_at(x + 32), p.u64_at(b + 8), "inode numbers agree");
    assert_eq!(p.u64_at(x + 48), 1, "stx_blocks");
    for at in [64, 80, 96, 112] {
        assert_eq!(p.u64_at(x + at), 1_700_000_000, "statx time at {at}");
    }
}

#[test]
fn getdents_offsets_count_entries() {
    let mut p = Proc::new();
    p.open("/home/a", O_CREAT | O_WRONLY);
    let d = p.open("/home", O_RDONLY | O_DIRECTORY);
    let buf = p.buf(1024);
    let n = p.call(SYS_GETDENTS64, &[d as u64, buf, 1024]) as usize;
    let raw = p.bytes(buf, n);
    let (mut at, mut i) = (0, 1u64);
    while at < n {
        assert_eq!(
            u64::from_le_bytes(raw[at + 8..at + 16].try_into().unwrap()),
            i,
            "d_off of entry {i}"
        );
        assert_ne!(
            u64::from_le_bytes(raw[at..at + 8].try_into().unwrap()),
            0,
            "d_ino"
        );
        at += u16::from_le_bytes([raw[at + 16], raw[at + 17]]) as usize;
        i += 1;
    }
    assert_eq!(i, 4, "three entries: . .. a");
}

#[test]
fn a_directory_too_deep_to_remember_is_refused() {
    let mut p = Proc::new();
    let mut path = String::new();
    while path.len() < 1100 {
        path.push_str("/dddddddddddddddddddddddddddddddddddddddddddddddd");
        let c = p.cstr(&path);
        assert_eq!(p.call(SYS_MKDIR, &[c, 0o755]), 0);
    }
    let live = p.be().live();
    assert_eq!(p.open(&path, O_RDONLY | O_DIRECTORY), neg(ENAMETOOLONG));
    assert_eq!(p.be().live(), live, "and nothing was left open");
    // one that fits still works
    assert!(p.open(&path[..path.len() - 49 * 8], O_RDONLY | O_DIRECTORY) >= 0);
}

#[test]
fn only_status_flags_are_kept_on_a_descriptor() {
    let mut p = Proc::new();
    let fd = p.open(
        "/tmp/s",
        O_CREAT | O_EXCL | O_TRUNC | O_RDWR | O_APPEND | O_CLOEXEC,
    );
    assert_eq!(
        p.call(SYS_FCNTL, &[fd as u64, F_GETFL, 0]) as u32,
        O_RDWR | O_APPEND
    );
    assert_eq!(p.call(SYS_FCNTL, &[fd as u64, F_GETFD, 0]), 1);
}

#[test]
fn an_absolute_path_ignores_the_directory_descriptor_even_when_it_is_bad() {
    let mut p = Proc::new();
    let abs = p.cstr("/etc/passwd");
    let rel = p.cstr("etc/passwd");
    for bad in [99u64, 0xffff_ffff, 1, 63] {
        // 1 is the terminal: open but not a directory; the others are not open at all
        let f = p.call(SYS_OPENAT, &[bad, abs, 0, 0]);
        assert!(f >= 0, "descriptor {bad}: {f}");
        p.call(SYS_CLOSE, &[f as u64]);
        assert!(
            p.call(SYS_OPENAT, &[bad, rel, 0, 0]) < 0,
            "a relative path needs a directory"
        );
    }
}

#[test]
fn creating_with_a_trailing_slash_creates_nothing() {
    let mut p = Proc::new();
    assert_eq!(p.open("/tmp/newdir/", O_CREAT | O_RDWR), neg(EISDIR));
    assert!(
        !p.be().exists("/tmp/newdir"),
        "no file appeared under the name"
    );
    assert_eq!(
        p.open("/tmp/newdir2/", O_CREAT | O_RDONLY | O_DIRECTORY),
        neg(EISDIR)
    );
    assert!(!p.be().exists("/tmp/newdir2"));
}

#[test]
fn a_trailing_slash_stops_unlink_from_removing_a_file() {
    let mut p = Proc::new();
    p.open("/tmp/keep", O_CREAT | O_WRONLY);
    let (file, dir, none) = (
        p.cstr("/tmp/keep/"),
        p.cstr("/home/"),
        p.cstr("/tmp/nothing/"),
    );
    assert_eq!(p.call(SYS_UNLINK, &[file]), neg(ENOTDIR));
    assert!(p.be().exists("/tmp/keep"), "the file is still there");
    assert_eq!(p.call(SYS_UNLINK, &[dir]), neg(EISDIR));
    assert_eq!(p.call(SYS_UNLINK, &[none]), neg(ENOENT));
}

#[test]
fn linkat_flags_are_not_provided() {
    let mut p = Proc::new();
    let (a, b) = (p.cstr("/etc/passwd"), p.cstr("/tmp/hard"));
    assert_eq!(
        p.call(SYS_LINKAT, &[FD, a, FD, b, 0x400]),
        neg(EINVAL),
        "AT_SYMLINK_FOLLOW"
    );
    assert_eq!(
        p.call(SYS_LINKAT, &[FD, a, FD, b, 0x1000]),
        neg(EINVAL),
        "AT_EMPTY_PATH"
    );
    assert!(!p.be().exists("/tmp/hard"));
    assert_eq!(p.call(SYS_LINKAT, &[FD, a, FD, b, 0]), 0);
}

#[test]
fn offsets_that_no_file_can_have_never_reach_the_backend() {
    let mut p = Proc::new();
    let fd = p.open("/tmp/o", O_CREAT | O_RDWR);
    p.write_str(fd, "0123456789");
    let before = p.be().file_writes.len();
    let b = p.put(b"0123456789");
    let neg_off = u64::MAX - 5; // a negative offset as a program passes it
    assert_eq!(
        p.call(SYS_PWRITE64, &[fd as u64, b, 4, neg_off]),
        neg(EINVAL)
    );
    assert_eq!(
        p.call(SYS_PREAD64, &[fd as u64, b, 4, neg_off]),
        neg(EINVAL)
    );
    assert_eq!(
        p.call(SYS_PWRITE64, &[fd as u64, b, 10, i64::MAX as u64 - 2]),
        neg(EFBIG),
        "would end past the largest offset"
    );
    assert_eq!(
        p.be().file_writes.len(),
        before,
        "none of the three reached the backend"
    );
    // ending exactly at the largest offset is the personality's to allow; the in-memory backend then
    // refuses for its own reasons
    assert_eq!(
        p.call(SYS_PWRITE64, &[fd as u64, b, 3, i64::MAX as u64 - 3]),
        neg(EFBIG)
    );
    assert_eq!(p.be().file_writes.len(), before + 1);
}

#[test]
fn the_working_directory_is_the_target_of_an_empty_path_with_at_fdcwd() {
    let mut p = Proc::new();
    let tmp = p.cstr("/tmp");
    p.call(SYS_CHDIR, &[tmp]);
    let (empty, x) = (p.cstr(""), p.buf(256));
    assert_eq!(
        p.call(SYS_STATX, &[FD, empty, u64::from(AT_EMPTY_PATH), 0x7ff, x]),
        0
    );
    let root = p.cstr("/tmp");
    let y = p.buf(256);
    assert_eq!(p.call(SYS_STATX, &[FD, root, 0, 0x7ff, y]), 0);
    assert_eq!(p.bytes(x, 256), p.bytes(y, 256), "the same directory");
    assert_eq!(
        p.call(SYS_STATX, &[FD, empty, 0, 0x7ff, x]),
        neg(ENOENT),
        "without AT_EMPTY_PATH an empty path is an error"
    );
}

#[test]
fn mkdir_applies_the_umask() {
    let mut p = Proc::new();
    let d = p.cstr("/tmp/m");
    assert_eq!(p.call(SYS_MKDIR, &[d, 0o777]), 0);
    let b = p.buf(144);
    p.call(SYS_STAT, &[d, b]);
    assert_eq!(p.u32_at(b + 24), S_IFDIR | 0o755);
}

#[test]
fn utimensat_without_a_path_means_the_descriptor() {
    let mut p = Proc::new();
    let fd = p.open("/etc/passwd", O_RDONLY);
    assert_eq!(p.call(SYS_UTIMENSAT, &[fd as u64, 0, 0, 0]), 0);
    assert_eq!(p.call(SYS_UTIMENSAT, &[77, 0, 0, 0]), neg(EBADF));
}

#[test]
fn nothing_is_asked_of_the_backend_that_it_cannot_do() {
    // The backend panics on reads or writes of a directory object and on listing a file;
    // the personality must refuse these first.
    let mut p = Proc::new();
    let d = p.open("/tmp", O_RDONLY | O_DIRECTORY);
    let f = p.open("/etc/passwd", O_RDONLY);
    let b = p.buf(64);
    assert_eq!(p.call(SYS_READ, &[d as u64, b, 8]), neg(EISDIR));
    assert_eq!(p.call(SYS_PREAD64, &[d as u64, b, 8, 0]), neg(EISDIR));
    assert_eq!(p.call(SYS_WRITE, &[d as u64, b, 8]), neg(EBADF));
    assert_eq!(p.call(SYS_GETDENTS64, &[f as u64, b, 64]), neg(ENOTDIR));
    let (root, tmp) = (p.cstr("/"), p.cstr("/tmp"));
    // operations on the root stop in the personality
    assert_eq!(p.call(SYS_MKDIR, &[root, 0o755]), neg(EEXIST));
    assert_eq!(p.call(SYS_RMDIR, &[root]), neg(EBUSY));
    assert_eq!(p.call(SYS_UNLINK, &[root]), neg(EBUSY));
    assert_eq!(p.call(SYS_SYMLINK, &[tmp, root]), neg(EEXIST));
    assert_eq!(p.call(SYS_RENAME, &[root, tmp]), neg(EBUSY));
    assert_eq!(p.call(SYS_RENAME, &[tmp, root]), neg(EBUSY));
    assert_eq!(p.call(SYS_LINKAT, &[FD, root, FD, tmp, 0]), neg(EPERM));
    assert_eq!(p.call(SYS_LINKAT, &[FD, tmp, FD, root, 0]), neg(EPERM));
}
