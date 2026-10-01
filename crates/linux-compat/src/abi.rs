//! Linux x86-64 flag values and the byte layouts of the structures that
//! cross the system-call boundary. Everything is written field by field,
//! little-endian, into a caller buffer: no Rust struct layout is trusted.

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub const AT_REMOVEDIR: u32 = 0x200;
pub const AT_EMPTY_PATH: u32 = 0x1000;

pub const O_ACCMODE: u32 = 3;
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_CREAT: u32 = 0o100;
pub const O_EXCL: u32 = 0o200;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_DIRECTORY: u32 = 0o200000;
pub const O_NOFOLLOW: u32 = 0o400000;
pub const O_CLOEXEC: u32 = 0o2000000;

pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

pub const F_DUPFD: u64 = 0;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_GETFL: u64 = 3;
pub const F_SETFL: u64 = 4;
pub const F_DUPFD_CLOEXEC: u64 = 1030;
pub const FD_CLOEXEC: u64 = 1;

pub const S_IFREG: u32 = 0o100_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFIFO: u32 = 0o010_000;
pub const S_IFCHR: u32 = 0o020_000;

pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

pub const POLLIN: u16 = 1;
pub const POLLOUT: u16 = 4;
pub const POLLNVAL: u16 = 0x20;

pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;

pub const FUTEX_WAIT: u32 = 0;
pub const FUTEX_WAKE: u32 = 1;
pub const FUTEX_PRIVATE_FLAG: u32 = 128;
pub const FUTEX_CMD_MASK: u32 = !(FUTEX_PRIVATE_FLAG | 256);

pub const RLIMIT_NOFILE: u32 = 7;
pub const RLIMIT_STACK: u32 = 3;
pub const RLIM_INFINITY: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Fifo,
    Char,
}

impl FileKind {
    pub fn mode_bits(self) -> u32 {
        match self {
            FileKind::File => S_IFREG,
            FileKind::Dir => S_IFDIR,
            FileKind::Symlink => S_IFLNK,
            FileKind::Fifo => S_IFIFO,
            FileKind::Char => S_IFCHR,
        }
    }
    pub fn dtype(self) -> u8 {
        match self {
            FileKind::File => DT_REG,
            FileKind::Dir => DT_DIR,
            FileKind::Symlink => DT_LNK,
            FileKind::Fifo => DT_FIFO,
            FileKind::Char => DT_CHR,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: FileKind,
    /// Permission bits only (no file type).
    pub perm: u32,
    pub size: u64,
    pub ino: u64,
    pub nlink: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
}

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

/// `struct stat` (144 bytes).
pub fn encode_stat(s: &Stat, uid: u32, gid: u32) -> [u8; 144] {
    let mut b = [0u8; 144];
    put(&mut b, 8, &s.ino.to_le_bytes());
    put(&mut b, 16, &u64::from(s.nlink).to_le_bytes());
    put(
        &mut b,
        24,
        &(s.kind.mode_bits() | (s.perm & 0o7777)).to_le_bytes(),
    );
    put(&mut b, 28, &uid.to_le_bytes());
    put(&mut b, 32, &gid.to_le_bytes());
    put(&mut b, 48, &s.size.to_le_bytes());
    put(&mut b, 56, &4096i64.to_le_bytes());
    put(&mut b, 64, &s.size.div_ceil(512).to_le_bytes());
    for at in [72usize, 88, 104] {
        put(&mut b, at, &s.mtime_sec.to_le_bytes());
        put(&mut b, at + 8, &u64::from(s.mtime_nsec).to_le_bytes());
    }
    b
}

/// `struct statx` (256 bytes) with the basic fields filled in.
pub fn encode_statx(s: &Stat, uid: u32, gid: u32) -> [u8; 256] {
    let mut b = [0u8; 256];
    put(&mut b, 0, &0x7ffu32.to_le_bytes()); // STATX_BASIC_STATS
    put(&mut b, 4, &4096u32.to_le_bytes());
    put(&mut b, 16, &s.nlink.to_le_bytes());
    put(&mut b, 20, &uid.to_le_bytes());
    put(&mut b, 24, &gid.to_le_bytes());
    put(
        &mut b,
        28,
        &((s.kind.mode_bits() | (s.perm & 0o7777)) as u16).to_le_bytes(),
    );
    put(&mut b, 32, &s.ino.to_le_bytes());
    put(&mut b, 40, &s.size.to_le_bytes());
    put(&mut b, 48, &s.size.div_ceil(512).to_le_bytes());
    for at in [64usize, 80, 96, 112] {
        put(&mut b, at, &s.mtime_sec.to_le_bytes());
        put(&mut b, at + 8, &s.mtime_nsec.to_le_bytes());
    }
    b
}

/// One `linux_dirent64` into `out`; returns its length (8-aligned) or None if it does not fit.
pub fn encode_dirent(out: &mut [u8], ino: u64, off: i64, dtype: u8, name: &[u8]) -> Option<usize> {
    let reclen = (19 + name.len() + 1).next_multiple_of(8);
    if reclen > out.len() {
        return None;
    }
    out[..reclen].fill(0);
    put(out, 0, &ino.to_le_bytes());
    put(out, 8, &off.to_le_bytes());
    put(out, 16, &(reclen as u16).to_le_bytes());
    out[18] = dtype;
    put(out, 19, name);
    Some(reclen)
}

/// `struct utsname`: six 65-byte fields.
pub fn encode_utsname(release: &[u8], version: &[u8]) -> [u8; 390] {
    let mut b = [0u8; 390];
    for (i, v) in [
        &b"Linux"[..],
        b"nanox",
        release,
        version,
        b"x86_64",
        b"(none)",
    ]
    .iter()
    .enumerate()
    {
        let n = v.len().min(64);
        put(&mut b, i * 65, &v[..n]);
    }
    b
}
