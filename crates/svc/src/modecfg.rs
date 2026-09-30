//! The persisted mode record: 12 bytes with a magic, a version, the mode and
//! a CRC-32. A missing or damaged record boots the machine into the safe
//! server mode (only services marked `safe` run; no interactive part), not
//! into a refusal to boot (docs/specs/M11-SERVER.md §2, invariant 5).

use crate::Mode;

pub const LEN: usize = 12;
const MAGIC: [u8; 4] = *b"NXM0";
const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Length,
    Magic,
    Version,
    Mode,
    Crc,
}

/// How to boot: the mode and whether only `safe` services may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boot {
    pub mode: Mode,
    pub safe: bool,
    /// Why safe mode was chosen (None for a valid record).
    pub reason: Option<DecodeError>,
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                crc >> 1 ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub fn encode(mode: Mode) -> [u8; LEN] {
    let mut b = [0u8; LEN];
    b[..4].copy_from_slice(&MAGIC);
    b[4] = VERSION;
    b[5] = mode as u8;
    let crc = crc32(&b[..8]);
    b[8..].copy_from_slice(&crc.to_le_bytes());
    b
}

pub fn decode(b: &[u8]) -> Result<Mode, DecodeError> {
    if b.len() != LEN {
        return Err(DecodeError::Length);
    }
    if b[..4] != MAGIC {
        return Err(DecodeError::Magic);
    }
    if b[4] != VERSION {
        return Err(DecodeError::Version);
    }
    if b[6] != 0 || b[7] != 0 {
        return Err(DecodeError::Mode);
    }
    let want = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    if crc32(&b[..8]) != want {
        return Err(DecodeError::Crc);
    }
    match b[5] {
        0 => Ok(Mode::Desktop),
        1 => Ok(Mode::Hybrid),
        2 => Ok(Mode::Server),
        _ => Err(DecodeError::Mode),
    }
}

/// The boot decision for a stored record (None: nothing stored).
pub fn boot(stored: Option<&[u8]>) -> Boot {
    let Some(bytes) = stored else {
        return Boot {
            mode: Mode::Server,
            safe: true,
            reason: Some(DecodeError::Length),
        };
    };
    match decode(bytes) {
        Ok(mode) => Boot {
            mode,
            safe: false,
            reason: None,
        },
        Err(e) => Boot {
            mode: Mode::Server,
            safe: true,
            reason: Some(e),
        },
    }
}
