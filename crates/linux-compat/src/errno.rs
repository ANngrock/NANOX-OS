//! Linux error numbers and the mapping from NANOX outcomes (ABI-NCI section 7).

/// A Linux errno (positive). System calls return `-errno`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i32);

pub const EPERM: Errno = Errno(1);
pub const ENOENT: Errno = Errno(2);
pub const ESRCH: Errno = Errno(3);
pub const EINTR: Errno = Errno(4);
pub const EIO: Errno = Errno(5);
pub const ENXIO: Errno = Errno(6);
pub const E2BIG: Errno = Errno(7);
pub const ENOEXEC: Errno = Errno(8);
pub const EBADF: Errno = Errno(9);
pub const ECHILD: Errno = Errno(10);
pub const EAGAIN: Errno = Errno(11);
pub const ENOMEM: Errno = Errno(12);
pub const EACCES: Errno = Errno(13);
pub const EFAULT: Errno = Errno(14);
pub const EBUSY: Errno = Errno(16);
pub const EEXIST: Errno = Errno(17);
pub const EXDEV: Errno = Errno(18);
pub const ENODEV: Errno = Errno(19);
pub const ENOTDIR: Errno = Errno(20);
pub const EISDIR: Errno = Errno(21);
pub const EINVAL: Errno = Errno(22);
pub const ENFILE: Errno = Errno(23);
pub const EMFILE: Errno = Errno(24);
pub const ENOTTY: Errno = Errno(25);
pub const EFBIG: Errno = Errno(27);
pub const ENOSPC: Errno = Errno(28);
pub const ESPIPE: Errno = Errno(29);
pub const EROFS: Errno = Errno(30);
pub const EPIPE: Errno = Errno(32);
pub const ERANGE: Errno = Errno(34);
pub const ENAMETOOLONG: Errno = Errno(36);
pub const ENOSYS: Errno = Errno(38);
pub const ENOTEMPTY: Errno = Errno(39);
pub const ELOOP: Errno = Errno(40);
pub const EOVERFLOW: Errno = Errno(75);
pub const ENOTSOCK: Errno = Errno(88);
pub const EOPNOTSUPP: Errno = Errno(95);
pub const EAFNOSUPPORT: Errno = Errno(97);
pub const ECONNRESET: Errno = Errno(104);
pub const EISCONN: Errno = Errno(106);
pub const ETIMEDOUT: Errno = Errno(110);

/// The outcomes a NANOX operation can end in (ABI-NCI section 7), when a
/// backend has nothing more specific to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NxError {
    InvalidArgument,
    BadHandle,
    Denied,
    Unsupported,
    Conflict,
    NotFound,
    Timeout,
    Cancelled,
    PeerDied,
    ResourceExhausted,
    IoError,
    OutcomeUnknown,
}

impl From<NxError> for Errno {
    fn from(e: NxError) -> Self {
        match e {
            NxError::InvalidArgument => EINVAL,
            NxError::BadHandle => EBADF,
            NxError::Denied => EACCES,
            NxError::Unsupported => EOPNOTSUPP,
            NxError::Conflict => EEXIST,
            NxError::NotFound => ENOENT,
            NxError::Timeout => ETIMEDOUT,
            NxError::Cancelled => EINTR,
            NxError::PeerDied => EPIPE,
            NxError::ResourceExhausted => ENOMEM,
            NxError::IoError | NxError::OutcomeUnknown => EIO,
        }
    }
}

/// The value a system call hands back in RAX.
pub fn ret(r: Result<u64, Errno>) -> i64 {
    match r {
        Ok(v) => v as i64,
        Err(Errno(e)) => -i64::from(e),
    }
}
