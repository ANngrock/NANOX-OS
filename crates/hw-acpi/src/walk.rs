//! Walk from the RSDP through the RSDT/XSDT with the caller's memory reader.
//!
//! The crate never dereferences a physical address. [`PhysRead`] is the only
//! way bytes enter, every range is checked for wrap-around and the 52-bit
//! physical limit before it is read, and every table lands in a buffer the
//! caller owns.

use crate::bytes::u32_at;
use crate::rsdp::{RootTable, Rsdp, RSDP_V1_LEN, RSDP_V2_LEN, RSDT_SIGNATURE, XSDT_SIGNATURE};
use crate::{AcpiError, Sdt, SDT_HEADER_LEN};

/// Exclusive upper bound for every physical range in a walk: the x86-64
/// architectural physical-address width is at most 52 bits.
pub const PHYS_LIMIT: u64 = 1 << 52;
/// Largest RSDP `length` the walker reads (ACPI 6.5 defines 36 bytes).
pub const RSDP_MAX_LEN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysReadError;

/// Physical memory access supplied by the caller (kernel mapping or test model).
pub trait PhysRead {
    /// Copy `buf.len()` bytes starting at physical address `phys` into `buf`.
    /// The walker has already checked that `phys..phys + buf.len()` neither
    /// wraps nor crosses [`PHYS_LIMIT`]; the reader decides whether the range
    /// is mapped and readable.
    fn read(&self, phys: u64, buf: &mut [u8]) -> Result<(), PhysReadError>;
}

/// Half-open physical range `[start, end)`.
type Range = (u64, u64);

fn phys_range(phys: u64, len: usize) -> Result<Range, AcpiError> {
    if phys == 0 {
        return Err(AcpiError::NullPointer);
    }
    let len = u64::try_from(len).map_err(|_| AcpiError::AddressOverflow)?;
    let end = phys.checked_add(len).ok_or(AcpiError::AddressOverflow)?;
    if end > PHYS_LIMIT {
        return Err(AcpiError::AddressOverflow);
    }
    Ok((phys, end))
}

fn overlaps(a: Range, b: Range) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn read_exact<R: PhysRead + ?Sized>(
    reader: &R,
    phys: u64,
    buf: &mut [u8],
) -> Result<(), AcpiError> {
    phys_range(phys, buf.len())?;
    reader.read(phys, buf).map_err(|_| AcpiError::ReadFailed)
}

/// Read and validate the RSDP at `phys`.
pub fn read_rsdp<R: PhysRead + ?Sized>(reader: &R, phys: u64) -> Result<Rsdp, AcpiError> {
    let mut buf = [0u8; RSDP_MAX_LEN];
    read_exact(reader, phys, &mut buf[..RSDP_V1_LEN])?;
    if buf[15] < 2 {
        return Rsdp::parse(&buf[..RSDP_V1_LEN]);
    }
    // Revision 2+: the length field decides how much the extended checksum covers.
    read_exact(reader, phys, &mut buf[..RSDP_V2_LEN])?;
    let length = u32_at(&buf, 20)? as usize;
    if length > RSDP_MAX_LEN {
        return Err(AcpiError::BadLength);
    }
    if length > RSDP_V2_LEN {
        read_exact(reader, phys, &mut buf[..length])?;
    }
    Rsdp::parse(&buf[..length.max(RSDP_V2_LEN)])
}

/// Read the table at `phys` into `buf` and verify its header and checksum.
/// `buf` must hold the whole table; its first `length` bytes are overwritten.
pub fn load_table<'b, R: PhysRead + ?Sized>(
    reader: &R,
    phys: u64,
    buf: &'b mut [u8],
) -> Result<Sdt<'b>, AcpiError> {
    let mut head = [0u8; SDT_HEADER_LEN];
    read_exact(reader, phys, &mut head)?;
    let header = crate::SdtHeader::parse(&head)?;
    let length = header.length as usize;
    phys_range(phys, length)?;
    let table = buf.get_mut(..length).ok_or(AcpiError::BufferTooSmall)?;
    read_exact(reader, phys, table)?;
    let table: &'b [u8] = table;
    Sdt::parse(table)
}

fn is_root_signature(signature: [u8; 4]) -> bool {
    signature == RSDT_SIGNATURE || signature == XSDT_SIGNATURE || signature == *b"RSD "
}

/// A validated RSDP plus its root table, ready to look tables up.
///
/// Construction rejects a root table that lists a zero address, the same
/// address twice, an address inside the RSDP or the root table itself, or a
/// range that overflows. Lookups only read the signature of unrelated
/// tables; a table is fully validated when it is loaded.
pub struct AcpiTables<'r, 'b, R: PhysRead + ?Sized> {
    reader: &'r R,
    rsdp: Rsdp,
    rsdp_range: Range,
    root_range: Range,
    root: RootTable<'b>,
}

impl<'r, 'b, R: PhysRead + ?Sized> AcpiTables<'r, 'b, R> {
    /// Read the RSDP at `rsdp_phys` and its root table into `root_buf`.
    pub fn new(reader: &'r R, rsdp_phys: u64, root_buf: &'b mut [u8]) -> Result<Self, AcpiError> {
        let rsdp = read_rsdp(reader, rsdp_phys)?;
        let rsdp_range = phys_range(rsdp_phys, rsdp.length as usize)?;
        let pointer = rsdp.root_pointer()?;
        if overlaps(phys_range(pointer.address, SDT_HEADER_LEN)?, rsdp_range) {
            return Err(AcpiError::PointerCycle);
        }
        let root =
            RootTable::from_sdt(load_table(reader, pointer.address, root_buf)?, pointer.kind)?;
        let root_range = phys_range(pointer.address, root.header().length as usize)?;
        if overlaps(root_range, rsdp_range) {
            return Err(AcpiError::PointerCycle);
        }
        for (index, entry) in root.entries().enumerate() {
            let range = phys_range(entry, SDT_HEADER_LEN)?;
            if overlaps(range, rsdp_range) || overlaps(range, root_range) {
                return Err(AcpiError::PointerCycle);
            }
            if root.entries().take(index).any(|earlier| earlier == entry) {
                return Err(AcpiError::DuplicatePointer);
            }
        }
        Ok(Self {
            reader,
            rsdp,
            rsdp_range,
            root_range,
            root,
        })
    }

    pub fn rsdp(&self) -> &Rsdp {
        &self.rsdp
    }

    pub fn root(&self) -> &RootTable<'b> {
        &self.root
    }

    pub fn root_address(&self) -> u64 {
        self.root_range.0
    }

    /// Physical address of the `index`-th table (0-based, in root-table
    /// order) whose signature is `signature`.
    pub fn find_address(&self, signature: [u8; 4], index: usize) -> Result<Option<u64>, AcpiError> {
        let mut seen = 0usize;
        for entry in self.root.entries() {
            let found = self.signature_at(entry)?;
            if is_root_signature(found) {
                return Err(AcpiError::PointerCycle);
            }
            if found == signature {
                if seen == index {
                    return Ok(Some(entry));
                }
                seen += 1;
            }
        }
        Ok(None)
    }

    /// Number of listed tables with `signature` (SSDTs may repeat).
    pub fn count(&self, signature: [u8; 4]) -> Result<usize, AcpiError> {
        let mut seen = 0usize;
        for entry in self.root.entries() {
            let found = self.signature_at(entry)?;
            if is_root_signature(found) {
                return Err(AcpiError::PointerCycle);
            }
            seen += usize::from(found == signature);
        }
        Ok(seen)
    }

    /// Load the `index`-th table with `signature` into `buf`.
    pub fn find<'t>(
        &self,
        signature: [u8; 4],
        index: usize,
        buf: &'t mut [u8],
    ) -> Result<Option<Sdt<'t>>, AcpiError> {
        let Some(phys) = self.find_address(signature, index)? else {
            return Ok(None);
        };
        let sdt = self.load(phys, buf)?;
        if sdt.signature() != signature {
            // The table changed between the signature probe and the load.
            return Err(AcpiError::BadSignature);
        }
        Ok(Some(sdt))
    }

    /// Load a table by address (e.g. the DSDT named by the FADT). Rejects a
    /// table that overlaps the RSDP or the root table.
    pub fn load<'t>(&self, phys: u64, buf: &'t mut [u8]) -> Result<Sdt<'t>, AcpiError> {
        let sdt = load_table(self.reader, phys, buf)?;
        let range = phys_range(phys, sdt.bytes().len())?;
        if overlaps(range, self.rsdp_range) || overlaps(range, self.root_range) {
            return Err(AcpiError::PointerCycle);
        }
        Ok(sdt)
    }

    fn signature_at(&self, phys: u64) -> Result<[u8; 4], AcpiError> {
        let mut signature = [0u8; 4];
        read_exact(self.reader, phys, &mut signature)?;
        Ok(signature)
    }
}
