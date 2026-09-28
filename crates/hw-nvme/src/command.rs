//! Submission and completion queue entries and the commands the driver
//! issues.
//!
//! * Submission Queue Entry: NVMe Base 2.0 §4.1 (?): CDW0 = opcode (7:0),
//!   FUSE (9:8), PSDT (15:14, 00b = PRPs), CID (31:16); NSID in dword 1;
//!   MPTR in dwords 4–5; PRP1 in dwords 6–7; PRP2 in dwords 8–9;
//!   command dwords 10–15.
//! * Completion Queue Entry: NVMe Base 2.0 §4.2 (?): DW0 command specific,
//!   DW2 = SQ head (15:0) and SQ identifier (31:16), DW3 = CID (15:0),
//!   phase tag (16), status field (31:17).
//! * Admin commands: Abort §5.1 (?), Create I/O CQ §5.4 (?), Create I/O SQ
//!   §5.5 (?), Delete I/O CQ §5.6 (?), Delete I/O SQ §5.7 (?), Identify
//!   §5.17 (?), Set Features / Number of Queues §5.27.1.5 (?).
//! * I/O commands: Flush (Base 2.0 §7.1 (?)); Read and Write (NVM Command
//!   Set Specification 1.0 §3.2.4 / §3.2.6 (?)): SLBA in CDW10–11, NLB
//!   (0's based) in CDW12 bits 15:0.

use crate::status::Status;

/// Size of a submission queue entry in bytes.
pub const SQE_SIZE: u64 = 64;
/// Size of a completion queue entry in bytes.
pub const CQE_SIZE: u64 = 16;

/// Admin command opcodes.
pub mod admin {
    /// Delete I/O Submission Queue.
    pub const DELETE_IO_SQ: u8 = 0x00;
    /// Create I/O Submission Queue.
    pub const CREATE_IO_SQ: u8 = 0x01;
    /// Delete I/O Completion Queue.
    pub const DELETE_IO_CQ: u8 = 0x04;
    /// Create I/O Completion Queue.
    pub const CREATE_IO_CQ: u8 = 0x05;
    /// Identify.
    pub const IDENTIFY: u8 = 0x06;
    /// Abort.
    pub const ABORT: u8 = 0x08;
    /// Set Features.
    pub const SET_FEATURES: u8 = 0x09;
}

/// NVM command set I/O opcodes.
pub mod nvm {
    /// Flush.
    pub const FLUSH: u8 = 0x00;
    /// Write.
    pub const WRITE: u8 = 0x01;
    /// Read.
    pub const READ: u8 = 0x02;
}

/// Identify CNS values.
pub mod cns {
    /// Identify Namespace data structure for the NSID.
    pub const NAMESPACE: u8 = 0x00;
    /// Identify Controller data structure.
    pub const CONTROLLER: u8 = 0x01;
}

/// Feature identifier: Number of Queues.
pub const FEATURE_NUMBER_OF_QUEUES: u8 = 0x07;

/// A 64-byte submission queue entry as sixteen little-endian dwords.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Command {
    /// Command dwords 0–15.
    pub dw: [u32; 16],
}

impl Command {
    /// Entry with only the opcode set (PRPs, CID 0, no fusing).
    #[must_use]
    pub const fn new(opcode: u8) -> Self {
        let mut dw = [0; 16];
        dw[0] = opcode as u32;
        Self { dw }
    }

    /// Opcode.
    #[must_use]
    pub const fn opcode(&self) -> u8 {
        self.dw[0] as u8
    }

    /// Command identifier.
    #[must_use]
    pub const fn cid(&self) -> u16 {
        (self.dw[0] >> 16) as u16
    }

    /// Sets the command identifier.
    pub fn set_cid(&mut self, cid: u16) {
        self.dw[0] = (self.dw[0] & 0xFFFF) | (u32::from(cid) << 16);
    }

    /// Namespace identifier.
    #[must_use]
    pub const fn nsid(&self) -> u32 {
        self.dw[1]
    }

    /// PRP entry 1.
    #[must_use]
    pub const fn prp1(&self) -> u64 {
        self.dw[6] as u64 | (self.dw[7] as u64) << 32
    }

    /// PRP entry 2.
    #[must_use]
    pub const fn prp2(&self) -> u64 {
        self.dw[8] as u64 | (self.dw[9] as u64) << 32
    }

    /// Sets both PRP entries.
    pub fn set_prp(&mut self, prp1: u64, prp2: u64) {
        self.dw[6] = prp1 as u32;
        self.dw[7] = (prp1 >> 32) as u32;
        self.dw[8] = prp2 as u32;
        self.dw[9] = (prp2 >> 32) as u32;
    }

    /// Little-endian wire image.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        for (chunk, dw) in out.chunks_exact_mut(4).zip(self.dw.iter()) {
            chunk.copy_from_slice(&dw.to_le_bytes());
        }
        out
    }

    /// Parses a wire image.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 64]) -> Self {
        let mut dw = [0u32; 16];
        for (d, chunk) in dw.iter_mut().zip(bytes.chunks_exact(4)) {
            *d = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        Self { dw }
    }

    /// Identify with the given CNS into the 4 KiB page at `buffer`.
    #[must_use]
    pub fn identify(cns: u8, nsid: u32, buffer: u64) -> Self {
        let mut c = Self::new(admin::IDENTIFY);
        c.dw[1] = nsid;
        c.set_prp(buffer, 0);
        c.dw[10] = u32::from(cns);
        c
    }

    /// Set Features / Number of Queues; counts are 0's based.
    #[must_use]
    pub fn set_num_queues(nsq0: u16, ncq0: u16) -> Self {
        let mut c = Self::new(admin::SET_FEATURES);
        c.dw[10] = u32::from(FEATURE_NUMBER_OF_QUEUES);
        c.dw[11] = u32::from(nsq0) | u32::from(ncq0) << 16;
        c
    }

    /// Create I/O Completion Queue, physically contiguous; `qsize0` is
    /// 0's based. With `vector`, interrupts are enabled on that vector.
    #[must_use]
    pub fn create_io_cq(qid: u16, qsize0: u16, base: u64, vector: Option<u16>) -> Self {
        let mut c = Self::new(admin::CREATE_IO_CQ);
        c.set_prp(base, 0);
        c.dw[10] = u32::from(qid) | u32::from(qsize0) << 16;
        c.dw[11] = 1 | vector.map_or(0, |v| 2 | u32::from(v) << 16);
        c
    }

    /// Create I/O Submission Queue, physically contiguous, bound to `cqid`;
    /// `qsize0` is 0's based.
    #[must_use]
    pub fn create_io_sq(qid: u16, qsize0: u16, base: u64, cqid: u16) -> Self {
        let mut c = Self::new(admin::CREATE_IO_SQ);
        c.set_prp(base, 0);
        c.dw[10] = u32::from(qid) | u32::from(qsize0) << 16;
        c.dw[11] = 1 | u32::from(cqid) << 16;
        c
    }

    /// Delete I/O Submission Queue.
    #[must_use]
    pub fn delete_io_sq(qid: u16) -> Self {
        let mut c = Self::new(admin::DELETE_IO_SQ);
        c.dw[10] = u32::from(qid);
        c
    }

    /// Delete I/O Completion Queue.
    #[must_use]
    pub fn delete_io_cq(qid: u16) -> Self {
        let mut c = Self::new(admin::DELETE_IO_CQ);
        c.dw[10] = u32::from(qid);
        c
    }

    /// Abort the command `cid` submitted to `sqid`.
    #[must_use]
    pub fn abort(sqid: u16, cid: u16) -> Self {
        let mut c = Self::new(admin::ABORT);
        c.dw[10] = u32::from(sqid) | u32::from(cid) << 16;
        c
    }

    fn rw(opcode: u8, nsid: u32, slba: u64, nlb0: u16) -> Self {
        let mut c = Self::new(opcode);
        c.dw[1] = nsid;
        c.dw[10] = slba as u32;
        c.dw[11] = (slba >> 32) as u32;
        c.dw[12] = u32::from(nlb0);
        c
    }

    /// Read `nlb0 + 1` blocks from `slba`; PRPs are set separately.
    #[must_use]
    pub fn read(nsid: u32, slba: u64, nlb0: u16) -> Self {
        Self::rw(nvm::READ, nsid, slba, nlb0)
    }

    /// Write `nlb0 + 1` blocks at `slba`; PRPs are set separately.
    #[must_use]
    pub fn write(nsid: u32, slba: u64, nlb0: u16) -> Self {
        Self::rw(nvm::WRITE, nsid, slba, nlb0)
    }

    /// Flush the volatile write cache of `nsid`.
    #[must_use]
    pub fn flush(nsid: u32) -> Self {
        let mut c = Self::new(nvm::FLUSH);
        c.dw[1] = nsid;
        c
    }
}

/// A decoded 16-byte completion queue entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionEntry {
    /// Command specific dword 0.
    pub dw0: u32,
    /// Command specific dword 1.
    pub dw1: u32,
    /// SQ head pointer at the time the entry was posted.
    pub sq_head: u16,
    /// Submission queue the command came from.
    pub sq_id: u16,
    /// Command identifier.
    pub cid: u16,
    /// Phase tag.
    pub phase: bool,
    /// Status field.
    pub status: Status,
}

impl CompletionEntry {
    /// Decodes a wire image.
    #[must_use]
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        let dw = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let dw2 = dw(8);
        let dw3 = dw(12);
        Self {
            dw0: dw(0),
            dw1: dw(4),
            sq_head: dw2 as u16,
            sq_id: (dw2 >> 16) as u16,
            cid: dw3 as u16,
            phase: dw3 & (1 << 16) != 0,
            status: Status::from_dw3(dw3),
        }
    }

    /// Encodes the wire image.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 16] {
        let dw2 = u32::from(self.sq_head) | u32::from(self.sq_id) << 16;
        let dw3 =
            u32::from(self.cid) | u32::from(self.phase) << 16 | u32::from(self.status.raw()) << 17;
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&self.dw0.to_le_bytes());
        out[4..8].copy_from_slice(&self.dw1.to_le_bytes());
        out[8..12].copy_from_slice(&dw2.to_le_bytes());
        out[12..16].copy_from_slice(&dw3.to_le_bytes());
        out
    }
}
