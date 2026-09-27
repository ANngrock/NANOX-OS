//! M9 ACPI table parser (docs/specs/M9-HARDWARE.md §3.1).
//!
//! Covers the RSDP (revision 0 and 2+), RSDT/XSDT, MADT, FADT, HPET, MCFG,
//! DMAR and IVRS. The crate never touches physical memory: callers either
//! pass table bytes directly or implement [`PhysRead`] for the walk from the
//! RSDP, and every table buffer comes from the caller. Nothing is allocated.
//!
//! Every parser validates the whole table (length, checksum, revision and
//! every record inside it) before it returns, so the typed views and
//! iterators it hands out cannot fail afterwards. Malformed input is an
//! error, never a panic or a partial result. Multi-byte fields are
//! little-endian and read byte-wise, so tables need no alignment.

#![no_std]
#![forbid(unsafe_code)]

mod bytes;
pub mod dmar;
pub mod fadt;
pub mod hpet;
pub mod ivrs;
pub mod madt;
pub mod mcfg;
pub mod rsdp;
pub mod walk;

pub use dmar::Dmar;
pub use fadt::Fadt;
pub use hpet::Hpet;
pub use ivrs::Ivrs;
pub use madt::Madt;
pub use mcfg::Mcfg;
pub use rsdp::{RootKind, RootPointer, RootTable, Rsdp};
pub use walk::{load_table, read_rsdp, AcpiTables, PhysRead, PhysReadError, PHYS_LIMIT};

/// Length of the common System Description Table header.
pub const SDT_HEADER_LEN: usize = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiError {
    /// The buffer ends before the structure it must contain.
    Truncated,
    /// A signature does not match the expected table.
    BadSignature,
    /// A declared length is below the structure minimum or does not hold a
    /// whole number of fixed-size entries.
    BadLength,
    /// The bytes covered by a checksum do not sum to zero.
    BadChecksum,
    /// The revision is outside the range this parser understands.
    UnsupportedRevision,
    /// A record (MADT entry, DMAR structure or device scope, IVRS block or
    /// entry) declares length 0.
    ZeroLengthRecord,
    /// A record is shorter than the minimum for its type.
    ShortRecord,
    /// A record extends past the end of its enclosing table or structure.
    RecordOverrun,
    /// A field holds a value the specification forbids: an inverted or
    /// overlapping range, an impossible PCI path, a repeated singleton.
    InvalidField,
    /// A table pointer is zero.
    NullPointer,
    /// The RSDT/XSDT lists the same physical address twice.
    DuplicatePointer,
    /// A pointer leads back into the RSDP or a root table.
    PointerCycle,
    /// An address range wraps around or crosses [`PHYS_LIMIT`].
    AddressOverflow,
    /// The caller's buffer cannot hold the table.
    BufferTooSmall,
    /// The caller's physical-memory reader reported a failure.
    ReadFailed,
    /// The RSDP names neither an RSDT nor an XSDT.
    NoRootTable,
}

/// Common 36-byte header of every System Description Table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdtHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: [u8; 4],
    pub creator_revision: u32,
}

impl SdtHeader {
    /// Decode the first 36 bytes. Only checks that the declared length can
    /// hold the header; the checksum covers the whole table and is verified
    /// by [`Sdt::parse`].
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        if bytes.len() < SDT_HEADER_LEN {
            return Err(AcpiError::Truncated);
        }
        let header = Self {
            signature: bytes::array(bytes, 0)?,
            length: bytes::u32_at(bytes, 4)?,
            revision: bytes::u8_at(bytes, 8)?,
            checksum: bytes::u8_at(bytes, 9)?,
            oem_id: bytes::array(bytes, 10)?,
            oem_table_id: bytes::array(bytes, 16)?,
            oem_revision: bytes::u32_at(bytes, 24)?,
            creator_id: bytes::array(bytes, 28)?,
            creator_revision: bytes::u32_at(bytes, 32)?,
        };
        if (header.length as usize) < SDT_HEADER_LEN {
            return Err(AcpiError::BadLength);
        }
        Ok(header)
    }
}

/// A checksum-verified table. `bytes` is exactly `header.length` long; a
/// larger input buffer is trimmed.
#[derive(Clone, Copy, Debug)]
pub struct Sdt<'a> {
    header: SdtHeader,
    bytes: &'a [u8],
}

impl<'a> Sdt<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        let header = SdtHeader::parse(bytes)?;
        let table = bytes
            .get(..header.length as usize)
            .ok_or(AcpiError::Truncated)?;
        if bytes::sum(table) != 0 {
            return Err(AcpiError::BadChecksum);
        }
        Ok(Self {
            header,
            bytes: table,
        })
    }

    pub fn header(&self) -> &SdtHeader {
        &self.header
    }

    pub fn signature(&self) -> [u8; 4] {
        self.header.signature
    }

    /// The whole table, header included.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The table after the common header.
    pub fn body(&self) -> &'a [u8] {
        bytes::tail(self.bytes, SDT_HEADER_LEN)
    }

    /// Check the signature and a table-specific minimum length.
    pub(crate) fn expect(self, signature: [u8; 4], min_len: usize) -> Result<Self, AcpiError> {
        if self.header.signature != signature {
            return Err(AcpiError::BadSignature);
        }
        if self.bytes.len() < min_len {
            return Err(AcpiError::BadLength);
        }
        Ok(self)
    }
}

/// ACPI Generic Address Structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gas {
    pub address_space: u8,
    pub bit_width: u8,
    pub bit_offset: u8,
    /// 0 undefined (legacy), 1 byte, 2 word, 3 dword, 4 qword.
    pub access_size: u8,
    pub address: u64,
}

impl Gas {
    pub const LEN: usize = 12;
    pub const SYSTEM_MEMORY: u8 = 0;
    pub const SYSTEM_IO: u8 = 1;
    pub const PCI_CONFIG: u8 = 2;

    /// Decode the 12 bytes at the start of `bytes`.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        Self::at(bytes, 0)
    }

    pub(crate) fn at(bytes: &[u8], offset: usize) -> Result<Self, AcpiError> {
        Ok(Self {
            address_space: bytes::u8_at(bytes, offset)?,
            bit_width: bytes::u8_at(bytes, offset + 1)?,
            bit_offset: bytes::u8_at(bytes, offset + 2)?,
            access_size: bytes::u8_at(bytes, offset + 3)?,
            address: bytes::u64_at(bytes, offset + 4)?,
        })
    }
}
