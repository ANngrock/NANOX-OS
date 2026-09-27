//! Bounds-checked little-endian field readers.
//!
//! Every reader returns [`AcpiError::Truncated`] instead of panicking when the
//! field would leave the slice, so parsers never index raw table bytes.

use crate::AcpiError;

pub(crate) fn array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], AcpiError> {
    let end = offset.checked_add(N).ok_or(AcpiError::Truncated)?;
    let slice = bytes.get(offset..end).ok_or(AcpiError::Truncated)?;
    <[u8; N]>::try_from(slice).map_err(|_| AcpiError::Truncated)
}

pub(crate) fn u8_at(bytes: &[u8], offset: usize) -> Result<u8, AcpiError> {
    bytes.get(offset).copied().ok_or(AcpiError::Truncated)
}

pub(crate) fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, AcpiError> {
    array(bytes, offset).map(u16::from_le_bytes)
}

pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, AcpiError> {
    array(bytes, offset).map(u32::from_le_bytes)
}

pub(crate) fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, AcpiError> {
    array(bytes, offset).map(u64::from_le_bytes)
}

/// Byte sum modulo 256; a valid ACPI checksum region sums to zero.
pub(crate) fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

/// The tail of `bytes` after `len`, or an empty slice. Used by iterators whose
/// records were validated already, so the fallback is never taken.
pub(crate) fn tail(bytes: &[u8], len: usize) -> &[u8] {
    bytes.get(len..).unwrap_or(&[])
}
