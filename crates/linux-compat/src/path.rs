//! Path resolution confined to the delegated root.
//!
//! The program sees a root directory `/`; what that is, is decided by the
//! capability the personality was given. Every path is normalized
//! **lexically** before it goes to the backend: `.` and empty components
//! vanish, `..` removes the previous component and at the root stays at the
//! root, so no path can name anything above the delegated root. The result is
//! absolute, has no `.`, `..` or repeated slashes and no trailing slash
//! (except the root itself). Symbolic links are the backend business, inside
//! its root; resolving `..` lexically before following links is a deliberate
//! deviation from Linux (documented in M10-ROUTES.md).

use crate::errno::{Errno, EINVAL, ENAMETOOLONG, ENOENT};
use crate::mem::PATH_MAX;

pub const NAME_MAX: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Length of the normalized path in the output buffer.
    pub len: usize,
    /// The path ended in `/`, `/.` or `/..`: it must name a directory.
    pub dir_only: bool,
}

/// Resolves `path` against `cwd` (an absolute normalized path) into `out`.
pub fn resolve(cwd: &[u8], path: &[u8], out: &mut [u8; PATH_MAX]) -> Result<Resolved, Errno> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    if path.len() >= PATH_MAX {
        return Err(ENAMETOOLONG);
    }
    let mut len = 0usize;
    if path[0] != b'/' {
        if cwd.first() != Some(&b'/') || cwd.len() >= PATH_MAX {
            return Err(EINVAL);
        }
        // The root is the empty prefix: "/" stays "" until a component follows.
        let c = if cwd.len() == 1 { &cwd[..0] } else { cwd };
        out[..c.len()].copy_from_slice(c);
        len = c.len();
    }
    let mut last_special = false;
    for comp in path.split(|b| *b == b'/') {
        match comp {
            b"" => {}
            b"." => last_special = true,
            b".." => {
                last_special = true;
                while len > 0 && out[len - 1] != b'/' {
                    len -= 1;
                }
                len = len.saturating_sub(1);
            }
            c => {
                last_special = false;
                if c.len() > NAME_MAX {
                    return Err(ENAMETOOLONG);
                }
                if len + 1 + c.len() >= PATH_MAX {
                    return Err(ENAMETOOLONG);
                }
                out[len] = b'/';
                out[len + 1..len + 1 + c.len()].copy_from_slice(c);
                len += 1 + c.len();
            }
        }
    }
    if len == 0 {
        out[0] = b'/';
        len = 1;
    }
    let dir_only = path.last() == Some(&b'/') || last_special;
    Ok(Resolved { len, dir_only })
}

/// The parent directory and the last component of a normalized path
/// (`/a/b` gives (`/a`, `b`); `/a` gives (`/`, `a`)); `None` for the root.
pub fn split_last(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path == b"/" || path.is_empty() {
        return None;
    }
    let i = path.iter().rposition(|b| *b == b'/')?;
    Some((if i == 0 { &path[..1] } else { &path[..i] }, &path[i + 1..]))
}
