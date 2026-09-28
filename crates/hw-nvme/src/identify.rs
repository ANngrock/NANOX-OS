//! Identify data structures.
//!
//! * Identify Controller: NVMe Base 2.0 §5.17.2.1 (?), Figure "Identify –
//!   Identify Controller Data Structure". Offsets used: VID 1:0, SSVID 3:2,
//!   SN 23:4, MN 63:24, FR 71:64, MDTS 77, CNTLID 79:78, VER 83:80,
//!   RTD3E 91:88, ACL 258, SQES 512, CQES 513, MAXCMD 515:514, NN 519:516,
//!   ONCS 521:520, VWC 525.
//! * Identify Namespace (NVM command set): NVM Command Set Specification
//!   1.0 §4.1.5.1 (?). Offsets used: NSZE 7:0, NCAP 15:8, NUSE 23:16,
//!   NSFEAT 24, NLBAF 25 (0's based), FLBAS 26 (format index in bits 3:0
//!   and 6:5, extended-LBA metadata in bit 4), LBA Format *n* at
//!   `128 + 4n`: MS 15:0, LBADS 23:16 (log2), RP 25:24.
//!
//! Serial number, model number and firmware revision are kept as raw bytes;
//! the `Debug` output of [`ControllerInfo`] omits them so they do not end up
//! in logs.

/// Bytes of an Identify data structure the controller transfers.
pub const IDENTIFY_SIZE: usize = 4096;
/// Prefix of the Identify data the parsers look at.
pub const IDENTIFY_PARSE_LEN: usize = 1024;
/// Most LBA formats a namespace can report.
pub const MAX_LBA_FORMATS: usize = 64;

/// Why Identify data was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentifyError {
    /// Fewer than [`IDENTIFY_PARSE_LEN`] bytes supplied.
    Truncated,
    /// SQES/CQES minimum above maximum or not covering 64/16-byte entries.
    EntrySize,
    /// NSZE = 0: the namespace is not active.
    InactiveNamespace,
    /// NCAP above NSZE, or the size in bytes overflows 64 bits.
    Capacity,
    /// NLBAF above 63.
    FormatCount,
    /// FLBAS selects a format beyond NLBAF.
    FormatIndex,
    /// LBA data size below 512 bytes or above 64 KiB.
    LbaSize,
}

fn le16(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([d[off], d[off + 1]])
}

fn le32(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

fn le64(d: &[u8], off: usize) -> u64 {
    u64::from(le32(d, off)) | u64::from(le32(d, off + 4)) << 32
}

fn copy<const N: usize>(d: &[u8], off: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&d[off..off + N]);
    out
}

/// Validated subset of the Identify Controller data structure.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ControllerInfo {
    /// PCI Vendor ID.
    pub vid: u16,
    /// PCI Subsystem Vendor ID.
    pub ssvid: u16,
    serial: [u8; 20],
    model: [u8; 40],
    firmware: [u8; 8],
    /// Maximum Data Transfer Size exponent (units of CAP.MPSMIN pages);
    /// 0 = no limit reported.
    pub mdts: u8,
    /// Controller ID.
    pub cntlid: u16,
    /// Version as reported in Identify (0 on NVMe 1.0/1.1 controllers).
    pub ver: u32,
    /// RTD3 Entry Latency in microseconds (0 = not reported).
    pub rtd3e_us: u32,
    /// Abort Command Limit, 0's based.
    pub acl: u8,
    /// Submission Queue Entry Size (min 3:0, max 7:4, log2).
    pub sqes: u8,
    /// Completion Queue Entry Size (min 3:0, max 7:4, log2).
    pub cqes: u8,
    /// Maximum outstanding commands (0 = not reported).
    pub maxcmd: u16,
    /// Number of Namespaces (largest valid NSID).
    pub nn: u32,
    /// Optional NVM Command Support.
    pub oncs: u16,
    /// Volatile Write Cache.
    pub vwc: u8,
}

impl ControllerInfo {
    /// Parses and validates the first [`IDENTIFY_PARSE_LEN`] bytes.
    pub fn parse(d: &[u8]) -> Result<Self, IdentifyError> {
        if d.len() < IDENTIFY_PARSE_LEN {
            return Err(IdentifyError::Truncated);
        }
        let info = Self {
            vid: le16(d, 0),
            ssvid: le16(d, 2),
            serial: copy(d, 4),
            model: copy(d, 24),
            firmware: copy(d, 64),
            mdts: d[77],
            cntlid: le16(d, 78),
            ver: le32(d, 80),
            rtd3e_us: le32(d, 88),
            acl: d[258],
            sqes: d[512],
            cqes: d[513],
            maxcmd: le16(d, 514),
            nn: le32(d, 516),
            oncs: le16(d, 520),
            vwc: d[525],
        };
        let fits = |v: u8, want: u8| (v & 0xF) <= want && want <= (v >> 4);
        if !fits(info.sqes, 6) || !fits(info.cqes, 4) {
            return Err(IdentifyError::EntrySize);
        }
        Ok(info)
    }

    /// Serial number bytes (ASCII, space padded).
    #[must_use]
    pub const fn serial(&self) -> &[u8; 20] {
        &self.serial
    }

    /// Model number bytes (ASCII, space padded).
    #[must_use]
    pub const fn model(&self) -> &[u8; 40] {
        &self.model
    }

    /// Firmware revision bytes (ASCII, space padded).
    #[must_use]
    pub const fn firmware(&self) -> &[u8; 8] {
        &self.firmware
    }

    /// Volatile write cache present.
    #[must_use]
    pub const fn volatile_write_cache(&self) -> bool {
        self.vwc & 1 != 0
    }

    /// MDTS in bytes for 4 KiB minimum pages; `None` when unlimited (or too
    /// large to matter).
    #[must_use]
    pub fn max_transfer_bytes(&self) -> Option<u64> {
        if self.mdts == 0 || self.mdts >= 52 {
            return None;
        }
        Some(crate::PAGE_SIZE << self.mdts)
    }
}

impl core::fmt::Debug for ControllerInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ControllerInfo")
            .field("vid", &self.vid)
            .field("ssvid", &self.ssvid)
            .field("serial", &"<redacted>")
            .field("model", &"<redacted>")
            .field("firmware", &"<redacted>")
            .field("mdts", &self.mdts)
            .field("cntlid", &self.cntlid)
            .field("ver", &self.ver)
            .field("rtd3e_us", &self.rtd3e_us)
            .field("acl", &self.acl)
            .field("sqes", &self.sqes)
            .field("cqes", &self.cqes)
            .field("maxcmd", &self.maxcmd)
            .field("nn", &self.nn)
            .field("oncs", &self.oncs)
            .field("vwc", &self.vwc)
            .finish()
    }
}

/// One LBA format descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LbaFormat {
    /// Metadata bytes per LBA.
    pub metadata_size: u16,
    /// log2 of the LBA data size (0 = format not available).
    pub lba_shift: u8,
    /// Relative performance (0 = best).
    pub relative_performance: u8,
}

/// Validated subset of the Identify Namespace data structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceInfo {
    /// Namespace size in logical blocks.
    pub nsze: u64,
    /// Namespace capacity in logical blocks.
    pub ncap: u64,
    /// Namespace utilisation in logical blocks.
    pub nuse: u64,
    /// Namespace features.
    pub nsfeat: u8,
    /// Index of the format in use.
    pub format_index: u8,
    /// Metadata transferred at the end of each LBA (FLBAS bit 4).
    pub extended_metadata: bool,
    format_count: u8,
    formats: [LbaFormat; MAX_LBA_FORMATS],
}

impl NamespaceInfo {
    /// Parses and validates the first [`IDENTIFY_PARSE_LEN`] bytes.
    pub fn parse(d: &[u8]) -> Result<Self, IdentifyError> {
        if d.len() < IDENTIFY_PARSE_LEN {
            return Err(IdentifyError::Truncated);
        }
        let nsze = le64(d, 0);
        let ncap = le64(d, 8);
        if nsze == 0 {
            return Err(IdentifyError::InactiveNamespace);
        }
        if ncap > nsze {
            return Err(IdentifyError::Capacity);
        }
        let nlbaf = d[25];
        if usize::from(nlbaf) >= MAX_LBA_FORMATS {
            return Err(IdentifyError::FormatCount);
        }
        let flbas = d[26];
        let format_index = (flbas & 0xF) | ((flbas >> 5) & 3) << 4;
        if format_index > nlbaf {
            return Err(IdentifyError::FormatIndex);
        }
        let mut formats = [LbaFormat::default(); MAX_LBA_FORMATS];
        for (i, f) in formats.iter_mut().enumerate().take(usize::from(nlbaf) + 1) {
            let off = 128 + 4 * i;
            *f = LbaFormat {
                metadata_size: le16(d, off),
                lba_shift: d[off + 2],
                relative_performance: d[off + 3] & 3,
            };
        }
        let shift = formats[usize::from(format_index)].lba_shift;
        if !(9..=16).contains(&shift) {
            return Err(IdentifyError::LbaSize);
        }
        if nsze.leading_zeros() < u32::from(shift) {
            return Err(IdentifyError::Capacity);
        }
        Ok(Self {
            nsze,
            ncap,
            nuse: le64(d, 16),
            nsfeat: d[24],
            format_index,
            extended_metadata: flbas & 0x10 != 0,
            format_count: nlbaf + 1,
            formats,
        })
    }

    /// Format in use.
    #[must_use]
    pub fn current_format(&self) -> LbaFormat {
        self.formats[usize::from(self.format_index)]
    }

    /// Reported LBA formats.
    #[must_use]
    pub fn formats(&self) -> &[LbaFormat] {
        &self.formats[..usize::from(self.format_count)]
    }

    /// log2 of the logical block size in use.
    #[must_use]
    pub fn lba_shift(&self) -> u8 {
        self.current_format().lba_shift
    }

    /// Logical block size in bytes.
    #[must_use]
    pub fn lba_size(&self) -> u32 {
        1 << self.lba_shift()
    }
}
