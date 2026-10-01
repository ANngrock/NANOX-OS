//! The process startup contract: what a native process finds when it starts.
//!
//! The loader passes a pointer and a length in RDI and RSI to a read-only
//! blob (proposal; docs/specs/M10-NATIVE.md, to be agreed with the M2
//! loader). Little-endian, no padding surprises:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `NXSI` |
//! | 4 | 2 | version, 1 |
//! | 6 | 2 | header length, 32 |
//! | 8 | 4 | total length of the blob |
//! | 12 | 4 | flags, must be 0 |
//! | 16 | 4 | arguments: offset |
//! | 20 | 4 | arguments: length |
//! | 24 | 4 | handles: offset |
//! | 28 | 4 | handles: count |
//!
//! Arguments are NUL-terminated byte strings back to back. The handle table
//! has 24-byte entries: a 16-byte name (printable ASCII, NUL-padded) and a
//! non-zero 64-bit handle. Names, not slot numbers, say what a handle is, so
//! the loader and the program need not agree on an order. Everything is
//! bounds-checked; a malformed blob is refused, never partly used.

pub const MAGIC: [u8; 4] = *b"NXSI";
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 32;
pub const HANDLE_ENTRY: usize = 24;
pub const NAME_LEN: usize = 16;
pub const MAX_TOTAL: usize = 64 * 1024;
pub const MAX_HANDLES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    TooShort,
    Magic,
    Version,
    HeaderLen,
    /// Total length beyond the limit or beyond the bytes supplied.
    TotalLen,
    Flags,
    /// An area lies outside the blob or before the end of the header.
    Bounds,
    /// The arguments and the handle table overlap.
    Overlap,
    /// A non-empty argument area whose last byte is not NUL.
    ArgsUnterminated,
    TooManyHandles,
    /// A name that is empty, not printable ASCII, or padded with non-NULs.
    BadName,
    DuplicateName,
    ZeroHandle,
    /// The output buffer of [`encode`] is too small.
    NoSpace,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn valid_name(name: &[u8]) -> bool {
    let n = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    n > 0
        && name[..n].iter().all(|b| (0x21..=0x7E).contains(b))
        && name[n..].iter().all(|b| *b == 0)
}

/// A validated startup blob.
#[derive(Clone, Copy, Debug)]
pub struct StartInfo<'a> {
    args: &'a [u8],
    handles: &'a [u8],
}

impl<'a> StartInfo<'a> {
    /// Validates `bytes` (which may be longer than the blob).
    pub fn parse(bytes: &'a [u8]) -> Result<Self, StartError> {
        if bytes.len() < HEADER_LEN {
            return Err(StartError::TooShort);
        }
        if bytes[..4] != MAGIC {
            return Err(StartError::Magic);
        }
        if u16_at(bytes, 4) != VERSION {
            return Err(StartError::Version);
        }
        if usize::from(u16_at(bytes, 6)) != HEADER_LEN {
            return Err(StartError::HeaderLen);
        }
        let total = u32_at(bytes, 8) as usize;
        if !(HEADER_LEN..=MAX_TOTAL).contains(&total) || total > bytes.len() {
            return Err(StartError::TotalLen);
        }
        if u32_at(bytes, 12) != 0 {
            return Err(StartError::Flags);
        }
        let (args_off, args_len) = (u32_at(bytes, 16) as usize, u32_at(bytes, 20) as usize);
        let (h_off, h_count) = (u32_at(bytes, 24) as usize, u32_at(bytes, 28) as usize);
        if h_count > MAX_HANDLES {
            return Err(StartError::TooManyHandles);
        }
        let h_len = h_count * HANDLE_ENTRY;
        let area = |off: usize, len: usize| -> Result<&'a [u8], StartError> {
            let end = off.checked_add(len).ok_or(StartError::Bounds)?;
            if len > 0 && (off < HEADER_LEN || end > total) {
                return Err(StartError::Bounds);
            }
            Ok(if len == 0 { &[] } else { &bytes[off..end] })
        };
        let args = area(args_off, args_len)?;
        let handles = area(h_off, h_len)?;
        if args_len > 0 && h_len > 0 && args_off < h_off + h_len && h_off < args_off + args_len {
            return Err(StartError::Overlap);
        }
        if args.last().is_some_and(|b| *b != 0) {
            return Err(StartError::ArgsUnterminated);
        }
        for (i, e) in handles.chunks_exact(HANDLE_ENTRY).enumerate() {
            if !valid_name(&e[..NAME_LEN]) {
                return Err(StartError::BadName);
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&e[NAME_LEN..]);
            if u64::from_le_bytes(raw) == 0 {
                return Err(StartError::ZeroHandle);
            }
            let seen = handles.chunks_exact(HANDLE_ENTRY).take(i);
            if seen.into_iter().any(|p| p[..NAME_LEN] == e[..NAME_LEN]) {
                return Err(StartError::DuplicateName);
            }
        }
        Ok(Self { args, handles })
    }

    /// Reads the blob at `ptr`.
    ///
    /// # Safety
    /// `ptr` must point to `len` readable bytes that stay unchanged for the
    /// lifetime of the result.
    pub unsafe fn from_raw(ptr: *const u8, len: usize) -> Result<StartInfo<'static>, StartError> {
        // SAFETY: by the contract above.
        let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
        StartInfo::parse(bytes)
    }

    pub fn arg_count(&self) -> usize {
        self.args.iter().filter(|b| **b == 0).count()
    }

    /// The arguments, without their terminators.
    pub fn args(&self) -> impl Iterator<Item = &'a [u8]> {
        self.args.split(|b| *b == 0).take(self.arg_count())
    }

    /// Named handles, in table order.
    pub fn handles(&self) -> impl Iterator<Item = (&'a str, u64)> {
        self.handles.chunks_exact(HANDLE_ENTRY).map(|e| {
            let n = e[..NAME_LEN]
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(NAME_LEN);
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&e[NAME_LEN..]);
            // Names were validated as printable ASCII.
            (
                core::str::from_utf8(&e[..n]).unwrap_or(""),
                u64::from_le_bytes(raw),
            )
        })
    }

    pub fn handle(&self, name: &str) -> Option<u64> {
        self.handles().find(|(n, _)| *n == name).map(|(_, h)| h)
    }
}

/// Writes a blob (what the loader does). Returns its length.
pub fn encode(
    args: &[&[u8]],
    handles: &[(&str, u64)],
    out: &mut [u8],
) -> Result<usize, StartError> {
    if handles.len() > MAX_HANDLES {
        return Err(StartError::TooManyHandles);
    }
    let args_len: usize = args.iter().map(|a| a.len() + 1).sum();
    let h_off = (HEADER_LEN + args_len + 7) & !7;
    let total = h_off + handles.len() * HANDLE_ENTRY;
    if total > MAX_TOTAL {
        return Err(StartError::TotalLen);
    }
    let o = out.get_mut(..total).ok_or(StartError::NoSpace)?;
    o.fill(0);
    o[..4].copy_from_slice(&MAGIC);
    o[4..6].copy_from_slice(&VERSION.to_le_bytes());
    o[6..8].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
    o[8..12].copy_from_slice(&(total as u32).to_le_bytes());
    o[16..20].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    o[20..24].copy_from_slice(&(args_len as u32).to_le_bytes());
    o[24..28].copy_from_slice(&(h_off as u32).to_le_bytes());
    o[28..32].copy_from_slice(&(handles.len() as u32).to_le_bytes());
    let mut at = HEADER_LEN;
    for a in args {
        o[at..at + a.len()].copy_from_slice(a);
        at += a.len() + 1;
    }
    for (i, (name, handle)) in handles.iter().enumerate() {
        let e = h_off + i * HANDLE_ENTRY;
        let n = name.as_bytes();
        if n.len() > NAME_LEN {
            return Err(StartError::BadName);
        }
        o[e..e + n.len()].copy_from_slice(n);
        o[e + NAME_LEN..e + HANDLE_ENTRY].copy_from_slice(&handle.to_le_bytes());
    }
    Ok(total)
}

#[cfg(all(feature = "entry", target_arch = "x86_64"))]
mod entry {
    use super::StartInfo;

    // RDI and RSI already hold the blob pointer and length when the kernel
    // enters user mode; _start only clears the frame pointer and aligns the
    // stack for the call (the callee sees rsp = 8 mod 16, as the ABI wants).
    core::arch::global_asm!(
        ".section .text.entry,\"ax\",@progbits",
        ".global _start",
        "_start:",
        "xor ebp, ebp",
        "and rsp, -16",
        "call {rust_entry}",
        "ud2",
        rust_entry = sym rust_entry,
    );

    extern "Rust" {
        /// Defined by [`nanox_entry!`](crate::nanox_entry).
        fn nanox_main(info: &StartInfo<'_>) -> i32;
    }

    extern "C" {
        /// Ends the process. Provided by the syscall layer (`thread_exit`).
        fn nanox_exit(code: i32) -> !;
    }

    extern "C" fn rust_entry(info: *const u8, len: usize) -> ! {
        // SAFETY: the loader hands over a readable blob (M10-NATIVE.md).
        let code = match unsafe { StartInfo::from_raw(info, len) } {
            // SAFETY: nanox_main is defined by the program through the macro.
            Ok(i) => unsafe { nanox_main(&i) },
            Err(_) => 127,
        };
        // SAFETY: provided by the syscall layer.
        unsafe { nanox_exit(code) }
    }
}

/// Names the function a native program starts in:
/// `nanox_entry!(main);` with `fn main(info: &StartInfo) -> i32`.
#[macro_export]
macro_rules! nanox_entry {
    ($f:path) => {
        #[no_mangle]
        pub fn nanox_main(info: &$crate::StartInfo<'_>) -> i32 {
            $f(info)
        }
    };
}
