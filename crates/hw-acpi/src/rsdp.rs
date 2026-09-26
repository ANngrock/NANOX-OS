//! Root System Description Pointer and the RSDT/XSDT root tables.

use crate::bytes::{self, u32_at, u64_at, u8_at};
use crate::{AcpiError, Sdt, SdtHeader, SDT_HEADER_LEN};

pub const RSDP_SIGNATURE: [u8; 8] = *b"RSD PTR ";
/// ACPI 1.0 structure, covered by the first checksum.
pub const RSDP_V1_LEN: usize = 20;
/// ACPI 2.0+ structure, covered by the extended checksum.
pub const RSDP_V2_LEN: usize = 36;
pub const RSDT_SIGNATURE: [u8; 4] = *b"RSDT";
pub const XSDT_SIGNATURE: [u8; 4] = *b"XSDT";

/// Decoded RSDP. For revision 0 `length` is 20 and `xsdt_address` is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    pub oem_id: [u8; 6],
    pub revision: u8,
    pub rsdt_address: u32,
    pub length: u32,
    pub xsdt_address: u64,
}

impl Rsdp {
    /// Revision 0 needs 20 bytes; revision 2+ needs `length` (>= 36) bytes
    /// and a valid extended checksum. Revision 1 was never defined and is
    /// rejected.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        let v1 = bytes.get(..RSDP_V1_LEN).ok_or(AcpiError::Truncated)?;
        if bytes::array::<8>(v1, 0)? != RSDP_SIGNATURE {
            return Err(AcpiError::BadSignature);
        }
        if bytes::sum(v1) != 0 {
            return Err(AcpiError::BadChecksum);
        }
        let oem_id = bytes::array(v1, 9)?;
        let revision = u8_at(v1, 15)?;
        let rsdt_address = u32_at(v1, 16)?;
        match revision {
            0 => Ok(Self {
                oem_id,
                revision,
                rsdt_address,
                length: RSDP_V1_LEN as u32,
                xsdt_address: 0,
            }),
            1 => Err(AcpiError::UnsupportedRevision),
            _ => {
                let length = u32_at(bytes, 20)?;
                if (length as usize) < RSDP_V2_LEN {
                    return Err(AcpiError::BadLength);
                }
                let full = bytes.get(..length as usize).ok_or(AcpiError::Truncated)?;
                if bytes::sum(full) != 0 {
                    return Err(AcpiError::BadChecksum);
                }
                Ok(Self {
                    oem_id,
                    revision,
                    rsdt_address,
                    length,
                    xsdt_address: u64_at(full, 24)?,
                })
            }
        }
    }

    /// The XSDT wins when the revision defines it and it is non-zero;
    /// otherwise the RSDT. QEMU q35 publishes revision 0 with an RSDT only.
    pub fn root_pointer(&self) -> Result<RootPointer, AcpiError> {
        if self.revision >= 2 && self.xsdt_address != 0 {
            Ok(RootPointer {
                kind: RootKind::Xsdt,
                address: self.xsdt_address,
            })
        } else if self.rsdt_address != 0 {
            Ok(RootPointer {
                kind: RootKind::Rsdt,
                address: u64::from(self.rsdt_address),
            })
        } else {
            Err(AcpiError::NoRootTable)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    /// 32-bit entries.
    Rsdt,
    /// 64-bit entries.
    Xsdt,
}

impl RootKind {
    pub fn signature(self) -> [u8; 4] {
        match self {
            Self::Rsdt => RSDT_SIGNATURE,
            Self::Xsdt => XSDT_SIGNATURE,
        }
    }

    pub fn entry_size(self) -> usize {
        match self {
            Self::Rsdt => 4,
            Self::Xsdt => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootPointer {
    pub kind: RootKind,
    pub address: u64,
}

/// A verified RSDT or XSDT.
#[derive(Clone, Copy, Debug)]
pub struct RootTable<'a> {
    sdt: Sdt<'a>,
    kind: RootKind,
}

impl<'a> RootTable<'a> {
    pub fn parse(bytes: &'a [u8], kind: RootKind) -> Result<Self, AcpiError> {
        Self::from_sdt(Sdt::parse(bytes)?, kind)
    }

    pub fn from_sdt(sdt: Sdt<'a>, kind: RootKind) -> Result<Self, AcpiError> {
        let sdt = sdt.expect(kind.signature(), SDT_HEADER_LEN)?;
        if !sdt.body().len().is_multiple_of(kind.entry_size()) {
            return Err(AcpiError::BadLength);
        }
        Ok(Self { sdt, kind })
    }

    pub fn header(&self) -> &SdtHeader {
        self.sdt.header()
    }

    pub fn kind(&self) -> RootKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.sdt.body().len() / self.kind.entry_size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Physical addresses of the listed tables, widened to 64 bits.
    pub fn entries(&self) -> RootEntries<'a> {
        RootEntries {
            rest: self.sdt.body(),
            size: self.kind.entry_size(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RootEntries<'a> {
    rest: &'a [u8],
    size: usize,
}

impl Iterator for RootEntries<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        let chunk = self.rest.get(..self.size)?;
        self.rest = bytes::tail(self.rest, self.size);
        if self.size == 4 {
            u32_at(chunk, 0).ok().map(u64::from)
        } else {
            u64_at(chunk, 0).ok()
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.rest.len() / self.size;
        (n, Some(n))
    }
}

impl ExactSizeIterator for RootEntries<'_> {}
impl core::iter::FusedIterator for RootEntries<'_> {}
