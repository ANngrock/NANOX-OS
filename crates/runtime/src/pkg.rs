//! The reader of NXPK archives (tools/native/nxpk.py has the layout and the
//! writer): the cross-built native programs of route C, shipped as one
//! file. The format has exactly one valid encoding per content, so
//! [`Archive::parse`] checks the structure strictly (sorted names, canonical
//! offsets and padding, nothing left over) and [`Archive::verify`] checks
//! every hash. A program is only handed out of an archive that passed both.
//!
//! No allocation, no unsafe code.

use crate::sha256::{sha256, Sha256};

pub const MAGIC: &[u8; 4] = b"NXPK";
pub const VERSION: u16 = 1;
pub const MAX_ENTRIES: u32 = 256;
pub const MAX_NAME: usize = 64;
pub const ALIGN: usize = 16;
const HEADER: usize = 16;
const TRAILER: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PkgError {
    TooShort,
    BadMagic,
    UnknownVersion,
    UnknownFlags,
    TooManyEntries,
    DirectoryOutOfRange,
    BadName,
    NamesNotSorted,
    UnknownKind,
    BadMode,
    NotCanonical,
    DataOutOfRange,
    PaddingNotZero,
    TrailingBytes,
    HashMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Program,
}

/// One program of the archive.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub name: &'a str,
    pub kind: Kind,
    pub mode: u16,
    pub sha256: [u8; 32],
    pub data: &'a [u8],
}

/// A structurally valid archive over borrowed bytes.
#[derive(Clone, Copy, Debug)]
pub struct Archive<'a> {
    bytes: &'a [u8],
    count: usize,
    dir_end: usize,
}

fn align(n: usize) -> Option<usize> {
    n.checked_add(ALIGN - 1).map(|v| v & !(ALIGN - 1))
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn valid_name(n: &[u8]) -> bool {
    (1..=MAX_NAME).contains(&n.len())
        && n.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// The directory entry at `pos`: the entry (data from `bytes`), where its
/// data starts, and where the next directory entry begins.
fn read_entry(bytes: &[u8], pos: usize) -> Result<(Entry<'_>, usize, usize), PkgError> {
    let n = usize::from(*bytes.get(pos).ok_or(PkgError::DirectoryOutOfRange)?);
    let name = bytes
        .get(pos + 1..pos + 1 + n)
        .ok_or(PkgError::DirectoryOutOfRange)?;
    if !valid_name(name) {
        return Err(PkgError::BadName);
    }
    let at = pos + 1 + n;
    let kind = match bytes.get(at) {
        Some(1) => Kind::Program,
        Some(_) => return Err(PkgError::UnknownKind),
        None => return Err(PkgError::DirectoryOutOfRange),
    };
    let mode = u16_at(bytes, at + 1).ok_or(PkgError::DirectoryOutOfRange)?;
    if mode > 0o7777 {
        return Err(PkgError::BadMode);
    }
    let size = u64_at(bytes, at + 3).ok_or(PkgError::DirectoryOutOfRange)?;
    let offset = u64_at(bytes, at + 11).ok_or(PkgError::DirectoryOutOfRange)?;
    let digest: [u8; 32] = bytes
        .get(at + 19..at + 51)
        .and_then(|d| d.try_into().ok())
        .ok_or(PkgError::DirectoryOutOfRange)?;
    let start = usize::try_from(offset).map_err(|_| PkgError::DataOutOfRange)?;
    let len = usize::try_from(size).map_err(|_| PkgError::DataOutOfRange)?;
    let end = start.checked_add(len).ok_or(PkgError::DataOutOfRange)?;
    let data = bytes.get(start..end).ok_or(PkgError::DataOutOfRange)?;
    // `name` is ASCII by `valid_name`, so this cannot fail.
    let name = core::str::from_utf8(name).map_err(|_| PkgError::BadName)?;
    Ok((
        Entry {
            name,
            kind,
            mode,
            sha256: digest,
            data,
        },
        start,
        at + 51,
    ))
}

impl<'a> Archive<'a> {
    /// Checks the structure: header, a directory of sorted valid names, data
    /// exactly where the canonical layout puts it with zero padding, and
    /// nothing between the last entry and the trailer. Hashes are `verify`'s job.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PkgError> {
        if bytes.len() < HEADER + TRAILER {
            return Err(PkgError::TooShort);
        }
        if &bytes[..4] != MAGIC {
            return Err(PkgError::BadMagic);
        }
        if u16_at(bytes, 4) != Some(VERSION) {
            return Err(PkgError::UnknownVersion);
        }
        if u16_at(bytes, 6) != Some(0) {
            return Err(PkgError::UnknownFlags);
        }
        let count = u32_at(bytes, 8).ok_or(PkgError::TooShort)?;
        if count > MAX_ENTRIES {
            return Err(PkgError::TooManyEntries);
        }
        let dir_len = u32_at(bytes, 12).ok_or(PkgError::TooShort)? as usize;
        let body_end = bytes.len() - TRAILER;
        let dir_end = HEADER
            .checked_add(dir_len)
            .filter(|e| *e <= body_end)
            .ok_or(PkgError::DirectoryOutOfRange)?;
        let mut next = align(dir_end).ok_or(PkgError::DirectoryOutOfRange)?;
        let zero = |from: usize, to: usize| {
            bytes
                .get(from..to)
                .is_some_and(|s| s.iter().all(|b| *b == 0))
        };
        if !zero(dir_end, next) {
            return Err(PkgError::PaddingNotZero);
        }
        let (mut pos, mut prev): (usize, Option<&[u8]>) = (HEADER, None);
        for _ in 0..count {
            if pos >= dir_end {
                return Err(PkgError::DirectoryOutOfRange);
            }
            let (entry, start, after) = read_entry(&bytes[..body_end], pos)?;
            if prev.is_some_and(|p| p >= entry.name.as_bytes()) {
                return Err(PkgError::NamesNotSorted);
            }
            prev = Some(entry.name.as_bytes());
            // Where this entry's data starts is not stored twice: it must be where the layout puts it.
            if start != next {
                return Err(PkgError::NotCanonical);
            }
            let stop = start + entry.data.len();
            let padded = align(stop)
                .filter(|p| *p <= body_end)
                .ok_or(PkgError::DataOutOfRange)?;
            if !zero(stop, padded) {
                return Err(PkgError::PaddingNotZero);
            }
            next = padded;
            pos = after;
        }
        if pos != dir_end {
            return Err(PkgError::DirectoryOutOfRange);
        }
        if next != body_end {
            return Err(PkgError::TrailingBytes);
        }
        Ok(Self {
            bytes,
            count: count as usize,
            dir_end,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The programs in name order.
    pub fn entries(&self) -> Entries<'a> {
        Entries {
            bytes: &self.bytes[..self.bytes.len() - TRAILER],
            pos: HEADER,
            end: self.dir_end,
        }
    }

    pub fn find(&self, name: &str) -> Option<Entry<'a>> {
        self.entries().find(|e| e.name == name)
    }

    /// The digest the archive carries for itself (its last 32 bytes).
    pub fn digest(&self) -> [u8; 32] {
        let mut d = [0u8; 32];
        d.copy_from_slice(&self.bytes[self.bytes.len() - TRAILER..]);
        d
    }

    /// Checks the trailer against the whole archive and every program against its digest.
    pub fn verify(&self) -> Result<(), PkgError> {
        let body = &self.bytes[..self.bytes.len() - TRAILER];
        if sha256(body) != self.digest() {
            return Err(PkgError::HashMismatch);
        }
        for e in self.entries() {
            let mut h = Sha256::new();
            h.update(e.data);
            if h.finalize() != e.sha256 {
                return Err(PkgError::HashMismatch);
            }
        }
        Ok(())
    }
}

/// Iterates over the directory of an archive that `parse` accepted.
#[derive(Clone, Copy, Debug)]
pub struct Entries<'a> {
    bytes: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Entry<'a>> {
        if self.pos >= self.end {
            return None;
        }
        let (entry, _, after) = read_entry(self.bytes, self.pos).ok()?;
        self.pos = after;
        Some(entry)
    }
}
