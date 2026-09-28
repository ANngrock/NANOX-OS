//! Behavioural NVMe controller model shared by the integration tests.
//!
//! The model decodes registers, queue entries and PRPs from raw memory on
//! its own (it uses only the crate's plain wire-format helpers), executes
//! admin and NVM commands against an in-memory block device and posts
//! completions with phase tags. It is strict: everything the driver does
//! that a real controller could misinterpret is recorded in `violations`
//! (doorbell covering entries not written since the last fetch, overwriting
//! unfetched entries, SQ overflow, CQ head beyond posted entries, queue
//! sizes above MQES, bad PRPs, transfers above MDTS, CC changes while
//! enabled, enabling before RDY cleared, ...). Tests assert it is empty.
//!
//! Faults: per-command actions from a hook (hang until aborted, lose the
//! completion, delay, error status, controller fatal, duplicate completion,
//! wrong CID, wrong SQ head) and controller-level faults (RDY never set or
//! cleared, CFS on enable, persistent CFS, shutdown never completes).

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};

use hw_nvme::command::{admin, nvm, Command, CompletionEntry};
use hw_nvme::status::{generic, specific, Status};
use hw_nvme::{
    Clock, Completion, Config, Controller, DataBuffer, DmaMemory, Mmio, Outcome, QueueMemory,
};

pub const PAGE: u64 = 4096;
pub const MEM_BASE: u64 = 0x4000_0000;
pub const MEM_SIZE: usize = 32 << 20;
pub const ADMIN_SQ: u64 = MEM_BASE;
pub const ADMIN_CQ: u64 = MEM_BASE + 0x1000;
pub const IO_SQ: u64 = MEM_BASE + 0x1_0000;
pub const IO_CQ: u64 = MEM_BASE + 0x2_0000;
pub const IDENTIFY: u64 = MEM_BASE + 0x3_0000;
pub const PRP_POOL: u64 = MEM_BASE + 0x10_0000;
pub const DATA: u64 = MEM_BASE + 0x40_0000;
pub const DATA_END: u64 = MEM_BASE + MEM_SIZE as u64;

const RDY: u32 = 1;
const CFS: u32 = 2;
const SHST_MASK: u32 = 3 << 2;
const SHN_MASK: u32 = 3 << 14;

#[derive(Clone, Debug)]
pub struct ModelConfig {
    pub mqes: u16,
    pub dstrd: u8,
    pub to: u8,
    pub css: u8,
    pub mpsmin: u8,
    pub mpsmax: u8,
    pub cqr: bool,
    pub cap_override: Option<u64>,
    pub version: u32,
    pub vid: u16,
    pub mdts: u8,
    pub acl: u8,
    pub nn: u32,
    pub sqes: u8,
    pub cqes: u8,
    pub vwc: bool,
    pub rtd3e_us: u32,
    pub nsze: u64,
    pub ncap: Option<u64>,
    pub lba_shift: u8,
    pub metadata: u16,
    pub max_io_queues: u16,
    pub tick_ns: u64,
    pub ready_delay_ns: u64,
    pub fetch_delay_ns: u64,
    pub latency_ns: u64,
    pub shutdown_delay_ns: u64,
    pub initially_enabled: bool,
}

impl Default for ModelConfig {
    /// Samsung PM981-class values where known (VID 144Dh, MQES 16383,
    /// MDTS 2 MiB, ACL 8); timing values are illustrative.
    fn default() -> Self {
        Self {
            mqes: 0x3FFF,
            dstrd: 0,
            to: 2,
            css: 1,
            mpsmin: 0,
            mpsmax: 4,
            cqr: true,
            cap_override: None,
            version: 0x0001_0300,
            vid: 0x144D,
            mdts: 9,
            acl: 7,
            nn: 1,
            sqes: 0x66,
            cqes: 0x44,
            vwc: true,
            rtd3e_us: 0,
            nsze: 16384,
            ncap: None,
            lba_shift: 9,
            metadata: 0,
            max_io_queues: 8,
            tick_ns: 10_000,
            ready_delay_ns: 50_000,
            fetch_delay_ns: 0,
            latency_ns: 20_000,
            shutdown_delay_ns: 100_000,
            initially_enabled: false,
        }
    }
}

impl ModelConfig {
    /// Silicon Motion SM2263-class profile (126Fh); limits illustrative.
    pub fn smi() -> Self {
        Self {
            vid: 0x126F,
            mqes: 0xFF,
            mdts: 5,
            acl: 3,
            dstrd: 0,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Normal,
    /// Never completes unless aborted.
    Hang,
    /// Executed, completion lost; cannot be aborted.
    Drop,
    /// Extra latency.
    Delay(u64),
    /// Completes with this status, no effect.
    Status(Status),
    /// Sets CSTS.CFS and stops processing.
    Fatal,
    /// Completion posted twice.
    Duplicate,
    /// Completion posted with another CID (the real one never completes).
    WrongCid(u16),
    /// Completion posted with a bogus SQ head.
    WrongSqHead(u16),
}

#[derive(Clone, Copy, Debug)]
pub struct CmdInfo {
    pub qid: u16,
    pub cid: u16,
    pub opcode: u8,
    pub seq: u64,
    pub slba: u64,
    pub nlb: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    pub rdy1_never: bool,
    pub rdy0_never: bool,
    pub cfs_on_enable: bool,
    pub cfs_persistent: bool,
    pub shutdown_never: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub fetched: u64,
    pub posted: u64,
    pub doorbells: u64,
    pub resets: u64,
    pub aborts_hit: u64,
    pub aborts_miss: u64,
    pub max_transfer: u64,
    pub max_prp_list_pages: u64,
}

struct Sq {
    base: u64,
    entries: u32,
    head: u32,
    tail: u32,
    cqid: u16,
    dirty: Vec<bool>,
    doorbell_at: u64,
}

struct Cq {
    base: u64,
    entries: u32,
    head: u32,
    tail: u32,
    phase: bool,
}

struct Pending {
    seq: u64,
    sqid: u16,
    cqid: u16,
    cmd: Command,
    ready_at: Option<u64>,
    forced: Option<Status>,
    aborted: bool,
    drop: bool,
    duplicate: bool,
    cid_override: Option<u16>,
    head_override: Option<u16>,
}

pub type Hook = Box<dyn FnMut(&CmdInfo) -> Action>;

pub struct Model {
    pub cfg: ModelConfig,
    pub faults: Faults,
    pub now: u64,
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    mem: Vec<u8>,
    pub disk: Vec<u8>,
    sqs: BTreeMap<u16, Sq>,
    cqs: BTreeMap<u16, Cq>,
    pending: Vec<Pending>,
    seq: u64,
    pub violations: Vec<String>,
    hook: Option<Hook>,
    en_at: Option<(bool, u64)>,
    shst_at: Option<u64>,
    fatal: bool,
    nq_set: Option<(u16, u16)>,
    pub stats: Stats,
}

fn gen(sc: u8) -> Status {
    Status::new(0, sc, false, true)
}

fn spec(sc: u8) -> Status {
    Status::new(1, sc, false, true)
}

impl Model {
    pub fn new(cfg: ModelConfig) -> Self {
        let disk_len = (cfg.nsze << cfg.lba_shift) as usize;
        let mut m = Self {
            faults: Faults::default(),
            now: 0,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq: 0,
            acq: 0,
            // Garbage everywhere so nothing works by accident.
            mem: vec![0xA5; MEM_SIZE],
            disk: vec![0; disk_len],
            sqs: BTreeMap::new(),
            cqs: BTreeMap::new(),
            pending: Vec::new(),
            seq: 0,
            violations: Vec::new(),
            hook: None,
            en_at: None,
            shst_at: None,
            fatal: false,
            nq_set: None,
            stats: Stats::default(),
            cfg,
        };
        if m.cfg.initially_enabled {
            m.cc = 1;
            m.csts = RDY;
        }
        m
    }

    pub fn set_hook(&mut self, f: impl FnMut(&CmdInfo) -> Action + 'static) {
        self.hook = Some(Box::new(f));
    }

    pub fn clear_hook(&mut self) {
        self.hook = None;
    }

    pub fn assert_clean(&self) {
        assert!(
            self.violations.is_empty(),
            "model violations: {:#?}",
            self.violations
        );
    }

    pub fn csts(&self) -> u32 {
        self.csts
    }

    pub fn cc(&self) -> u32 {
        self.cc
    }

    pub fn io_queues(&self) -> (usize, usize) {
        (
            self.sqs.keys().filter(|&&q| q != 0).count(),
            self.cqs.keys().filter(|&&q| q != 0).count(),
        )
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Makes the running controller fatal (CSTS.CFS) right now.
    pub fn trigger_fatal(&mut self) {
        self.fatal = true;
        self.csts |= CFS;
    }

    fn violation(&mut self, msg: String) {
        self.violations.push(msg);
    }

    fn cap_raw(&self) -> u64 {
        if let Some(raw) = self.cfg.cap_override {
            return raw;
        }
        let c = &self.cfg;
        u64::from(c.mqes)
            | u64::from(c.cqr) << 16
            | u64::from(c.to) << 24
            | u64::from(c.dstrd) << 32
            | u64::from(c.css) << 37
            | u64::from(c.mpsmin) << 48
            | u64::from(c.mpsmax) << 52
    }

    fn in_mem(&self, pa: u64, len: u64) -> bool {
        pa >= MEM_BASE
            && pa
                .checked_add(len)
                .is_some_and(|e| e <= MEM_BASE + MEM_SIZE as u64)
    }

    fn mem_read(&self, pa: u64, buf: &mut [u8]) -> bool {
        if !self.in_mem(pa, buf.len() as u64) {
            return false;
        }
        let o = (pa - MEM_BASE) as usize;
        buf.copy_from_slice(&self.mem[o..o + buf.len()]);
        true
    }

    fn mem_write(&mut self, pa: u64, data: &[u8]) -> bool {
        if !self.in_mem(pa, data.len() as u64) {
            return false;
        }
        let o = (pa - MEM_BASE) as usize;
        self.mem[o..o + data.len()].copy_from_slice(data);
        true
    }

    fn read_u64(&self, pa: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.mem_read(pa, &mut b).then(|| u64::from_le_bytes(b))
    }

    // ---- time ------------------------------------------------------------

    fn process(&mut self) {
        if let Some((target, at)) = self.en_at {
            if self.now >= at {
                self.en_at = None;
                if target {
                    self.csts |= RDY;
                    let asqs = (self.aqa & 0xFFF) + 1;
                    let acqs = ((self.aqa >> 16) & 0xFFF) + 1;
                    self.sqs.insert(
                        0,
                        Sq {
                            base: self.asq,
                            entries: asqs,
                            head: 0,
                            tail: 0,
                            cqid: 0,
                            dirty: vec![false; asqs as usize],
                            doorbell_at: 0,
                        },
                    );
                    self.cqs.insert(
                        0,
                        Cq {
                            base: self.acq,
                            entries: acqs,
                            head: 0,
                            tail: 0,
                            phase: true,
                        },
                    );
                } else {
                    self.csts &= !RDY;
                }
            }
        }
        if let Some(at) = self.shst_at {
            if self.now >= at {
                self.shst_at = None;
                self.csts = (self.csts & !SHST_MASK) | (2 << 2);
            }
        }
        if self.csts & RDY != 0 && !self.fatal {
            self.fetch();
            self.post();
        }
    }

    fn fetch(&mut self) {
        let qids: Vec<u16> = self.sqs.keys().copied().collect();
        for qid in qids {
            loop {
                if self.fatal {
                    return;
                }
                let (cmd, cqid) = {
                    let fetch_delay = self.cfg.fetch_delay_ns;
                    let now = self.now;
                    let Some(sq) = self.sqs.get_mut(&qid) else {
                        break;
                    };
                    if sq.head == sq.tail || now < sq.doorbell_at + fetch_delay {
                        break;
                    }
                    let idx = sq.head;
                    sq.head = (sq.head + 1) % sq.entries;
                    sq.dirty[idx as usize] = false;
                    let at = sq.base + u64::from(idx) * 64;
                    let cqid = sq.cqid;
                    let mut raw = [0u8; 64];
                    if !self.mem_read(at, &mut raw) {
                        self.violation(format!("SQ{qid} entry outside memory"));
                        continue;
                    }
                    (Command::from_bytes(&raw), cqid)
                };
                self.seq += 1;
                self.stats.fetched += 1;
                let info = CmdInfo {
                    qid,
                    cid: cmd.cid(),
                    opcode: cmd.opcode(),
                    seq: self.seq,
                    slba: u64::from(cmd.dw[10]) | u64::from(cmd.dw[11]) << 32,
                    nlb: (cmd.dw[12] & 0xFFFF) + 1,
                };
                let action = match self.hook.as_mut() {
                    Some(h) => h(&info),
                    None => Action::Normal,
                };
                let mut p = Pending {
                    seq: self.seq,
                    sqid: qid,
                    cqid,
                    cmd,
                    ready_at: Some(self.now + self.cfg.latency_ns),
                    forced: None,
                    aborted: false,
                    drop: false,
                    duplicate: false,
                    cid_override: None,
                    head_override: None,
                };
                match action {
                    Action::Normal => {}
                    Action::Hang => p.ready_at = None,
                    Action::Drop => p.drop = true,
                    Action::Delay(d) => p.ready_at = Some(self.now + self.cfg.latency_ns + d),
                    Action::Status(s) => p.forced = Some(s),
                    Action::Fatal => {
                        self.trigger_fatal();
                        return;
                    }
                    Action::Duplicate => p.duplicate = true,
                    Action::WrongCid(c) => p.cid_override = Some(c),
                    Action::WrongSqHead(h) => p.head_override = Some(h),
                }
                self.pending.push(p);
                if qid == 0 && info.opcode == admin::ABORT {
                    let aborts = self
                        .pending
                        .iter()
                        .filter(|p| p.sqid == 0 && p.cmd.opcode() == admin::ABORT)
                        .count();
                    if aborts > usize::from(self.cfg.acl) + 1 {
                        self.violation(format!("{aborts} aborts outstanding above ACL"));
                    }
                }
            }
        }
    }

    fn cq_full(&self, cqid: u16) -> bool {
        self.cqs
            .get(&cqid)
            .is_some_and(|cq| (cq.tail + 1) % cq.entries == cq.head)
    }

    fn post(&mut self) {
        loop {
            if self.fatal {
                return;
            }
            let mut best: Option<(u64, u64, usize)> = None;
            for (i, p) in self.pending.iter().enumerate() {
                let Some(t) = p.ready_at else { continue };
                if t > self.now || self.cq_full(p.cqid) {
                    continue;
                }
                if best.is_none_or(|(bt, bs, _)| (t, p.seq) < (bt, bs)) {
                    best = Some((t, p.seq, i));
                }
            }
            let Some((_, _, i)) = best else { return };
            let p = self.pending.remove(i);
            self.complete(p);
        }
    }

    fn complete(&mut self, p: Pending) {
        let (status, dw0) = if p.aborted {
            (Status::new(0, generic::ABORT_REQUESTED, false, false), 0)
        } else if let Some(s) = p.forced {
            (s, 0)
        } else if p.sqid == 0 {
            self.exec_admin(&p.cmd)
        } else {
            self.exec_io(&p.cmd)
        };
        if p.drop {
            return;
        }
        let sq_head = p
            .head_override
            .unwrap_or_else(|| self.sqs.get(&p.sqid).map_or(0, |s| s.head as u16));
        let e = CompletionEntry {
            dw0,
            dw1: 0,
            sq_head,
            sq_id: p.sqid,
            cid: p.cid_override.unwrap_or(p.cmd.cid()),
            phase: false,
            status,
        };
        self.post_cqe(p.cqid, e);
        if p.duplicate {
            self.post_cqe(p.cqid, e);
        }
    }

    fn post_cqe(&mut self, cqid: u16, mut e: CompletionEntry) {
        let Some(cq) = self.cqs.get(&cqid) else {
            self.violation(format!("completion for missing CQ{cqid}"));
            return;
        };
        if (cq.tail + 1) % cq.entries == cq.head {
            self.violation(format!("CQ{cqid} overflow"));
            return;
        }
        e.phase = cq.phase;
        let at = cq.base + u64::from(cq.tail) * 16;
        if !self.mem_write(at, &e.to_bytes()) {
            self.violation(format!("CQ{cqid} outside memory"));
        }
        let cq = self.cqs.get_mut(&cqid).expect("checked");
        cq.tail += 1;
        if cq.tail == cq.entries {
            cq.tail = 0;
            cq.phase = !cq.phase;
        }
        self.stats.posted += 1;
    }

    // ---- data transfer -----------------------------------------------------

    fn prp_segments(&mut self, prp1: u64, prp2: u64, len: u64) -> Result<Vec<(u64, u64)>, String> {
        if prp1 & 3 != 0 {
            return Err(format!("PRP1 {prp1:#x} not dword aligned"));
        }
        let mut segs = Vec::new();
        let first = len.min(PAGE - prp1 % PAGE);
        segs.push((prp1, first));
        let mut rem = len - first;
        if rem == 0 {
            if prp2 != 0 {
                return Err("PRP2 non-zero for a single-page transfer".into());
            }
        } else if rem <= PAGE {
            if !prp2.is_multiple_of(PAGE) {
                return Err(format!("PRP2 {prp2:#x} has an offset"));
            }
            segs.push((prp2, rem));
        } else {
            if prp2 & 7 != 0 {
                return Err(format!("PRP list pointer {prp2:#x} not qword aligned"));
            }
            let mut at = prp2;
            let mut list_pages = 1u64;
            while rem > 0 {
                if list_pages > 1024 {
                    return Err("PRP list chain too long".into());
                }
                let e = self
                    .read_u64(at)
                    .ok_or_else(|| format!("PRP list entry {at:#x} outside memory"))?;
                if at % PAGE == PAGE - 8 && rem > PAGE {
                    if e % PAGE != 0 {
                        return Err(format!("PRP list chain pointer {e:#x} has an offset"));
                    }
                    at = e;
                    list_pages += 1;
                    continue;
                }
                if e % PAGE != 0 {
                    return Err(format!("PRP list entry {e:#x} has an offset"));
                }
                let n = rem.min(PAGE);
                segs.push((e, n));
                rem -= n;
                at += 8;
            }
            self.stats.max_prp_list_pages = self.stats.max_prp_list_pages.max(list_pages);
        }
        for &(pa, n) in &segs {
            if !self.in_mem(pa, n) {
                return Err(format!("PRP segment {pa:#x}+{n} outside memory"));
            }
        }
        Ok(segs)
    }

    fn dma_out(&mut self, prp1: u64, prp2: u64, data: &[u8]) -> Result<(), ()> {
        match self.prp_segments(prp1, prp2, data.len() as u64) {
            Ok(segs) => {
                let mut off = 0usize;
                for (pa, n) in segs {
                    self.mem_write(pa, &data[off..off + n as usize]);
                    off += n as usize;
                }
                Ok(())
            }
            Err(msg) => {
                self.violation(msg);
                Err(())
            }
        }
    }

    // ---- commands -----------------------------------------------------------

    fn identify_controller(&self) -> Vec<u8> {
        let c = &self.cfg;
        let mut d = vec![0u8; 4096];
        d[0..2].copy_from_slice(&c.vid.to_le_bytes());
        d[2..4].copy_from_slice(&c.vid.to_le_bytes());
        let pad = |s: &[u8], n: usize| {
            let mut v = s.to_vec();
            v.resize(n, b' ');
            v
        };
        d[4..24].copy_from_slice(&pad(b"SN-NANOX-0001", 20));
        d[24..64].copy_from_slice(&pad(b"NANOX MODEL NVME", 40));
        d[64..72].copy_from_slice(&pad(b"FW01", 8));
        d[77] = c.mdts;
        d[78..80].copy_from_slice(&1u16.to_le_bytes());
        d[80..84].copy_from_slice(&c.version.to_le_bytes());
        d[88..92].copy_from_slice(&c.rtd3e_us.to_le_bytes());
        d[258] = c.acl;
        d[512] = c.sqes;
        d[513] = c.cqes;
        d[516..520].copy_from_slice(&c.nn.to_le_bytes());
        d[525] = u8::from(c.vwc);
        d
    }

    fn identify_namespace(&self) -> Vec<u8> {
        let c = &self.cfg;
        let mut d = vec![0u8; 4096];
        d[0..8].copy_from_slice(&c.nsze.to_le_bytes());
        d[8..16].copy_from_slice(&c.ncap.unwrap_or(c.nsze).to_le_bytes());
        d[16..24].copy_from_slice(&c.nsze.to_le_bytes());
        d[25] = 1;
        d[26] = 0;
        d[128..130].copy_from_slice(&c.metadata.to_le_bytes());
        d[130] = c.lba_shift;
        d[134] = 12;
        d
    }

    fn exec_admin(&mut self, cmd: &Command) -> (Status, u32) {
        let ok = Status::SUCCESS;
        let dw10 = cmd.dw[10];
        let dw11 = cmd.dw[11];
        match cmd.opcode() {
            admin::IDENTIFY => {
                let data = match dw10 & 0xFF {
                    1 => self.identify_controller(),
                    0 => {
                        let nsid = cmd.nsid();
                        if nsid == 0 || nsid > self.cfg.nn {
                            return (gen(generic::INVALID_NAMESPACE), 0);
                        }
                        self.identify_namespace()
                    }
                    _ => return (gen(generic::INVALID_FIELD), 0),
                };
                match self.dma_out(cmd.prp1(), cmd.prp2(), &data) {
                    Ok(()) => (ok, 0),
                    Err(()) => (gen(generic::INVALID_PRP_OFFSET), 0),
                }
            }
            admin::SET_FEATURES => {
                if dw10 & 0xFF != 7 {
                    return (gen(generic::INVALID_FIELD), 0);
                }
                if self.io_queues() != (0, 0) {
                    self.violation("Number of Queues set with I/O queues present".into());
                    return (gen(0x0C), 0);
                }
                let nsq = (dw11 & 0xFFFF) + 1;
                let ncq = (dw11 >> 16) + 1;
                if nsq > 0xFFFF || ncq > 0xFFFF {
                    return (gen(generic::INVALID_FIELD), 0);
                }
                let max = u32::from(self.cfg.max_io_queues);
                let (a, b) = (nsq.min(max) as u16, ncq.min(max) as u16);
                self.nq_set = Some((a, b));
                (ok, u32::from(b - 1) << 16 | u32::from(a - 1))
            }
            admin::CREATE_IO_CQ | admin::CREATE_IO_SQ => self.create_queue(cmd),
            admin::DELETE_IO_SQ => {
                let qid = dw10 as u16;
                if qid == 0 || self.sqs.remove(&qid).is_none() {
                    return (spec(specific::INVALID_QID), 0);
                }
                let now = self.now;
                for p in self.pending.iter_mut().filter(|p| p.sqid == qid) {
                    p.forced = Some(Status::new(0, generic::ABORTED_SQ_DELETION, false, false));
                    p.ready_at = Some(now);
                }
                (ok, 0)
            }
            admin::DELETE_IO_CQ => {
                let qid = dw10 as u16;
                if qid == 0 || !self.cqs.contains_key(&qid) {
                    return (spec(specific::INVALID_QID), 0);
                }
                if self.sqs.values().any(|s| s.cqid == qid) {
                    self.violation(format!("delete CQ{qid} while an SQ uses it"));
                    return (spec(specific::INVALID_QUEUE_DELETION), 0);
                }
                self.cqs.remove(&qid);
                (ok, 0)
            }
            admin::ABORT => {
                let sqid = dw10 as u16;
                let cid = (dw10 >> 16) as u16;
                let now = self.now;
                let hit = self.pending.iter_mut().find(|p| {
                    p.sqid == sqid
                        && p.cmd.cid() == cid
                        && !p.aborted
                        && !p.drop
                        && p.forced.is_none()
                });
                match hit {
                    Some(p) => {
                        p.aborted = true;
                        p.ready_at = Some(now);
                        self.stats.aborts_hit += 1;
                        (ok, 0)
                    }
                    None => {
                        self.stats.aborts_miss += 1;
                        (ok, 1)
                    }
                }
            }
            _ => (gen(generic::INVALID_OPCODE), 0),
        }
    }

    fn create_queue(&mut self, cmd: &Command) -> (Status, u32) {
        let is_cq = cmd.opcode() == admin::CREATE_IO_CQ;
        let name = if is_cq { "CQ" } else { "SQ" };
        let qid = cmd.dw[10] as u16;
        let qsize0 = (cmd.dw[10] >> 16) as u16;
        let pc = cmd.dw[11] & 1 != 0;
        let base = cmd.prp1();
        let Some((nsq, ncq)) = self.nq_set else {
            self.violation(format!("create I/O {name} before Number of Queues"));
            return (spec(specific::INVALID_QID), 0);
        };
        let limit = if is_cq { ncq } else { nsq };
        let exists = if is_cq {
            self.cqs.contains_key(&qid)
        } else {
            self.sqs.contains_key(&qid)
        };
        if qid == 0 || qid > limit || exists {
            return (spec(specific::INVALID_QID), 0);
        }
        if qsize0 == 0 || qsize0 > self.cfg.mqes {
            self.violation(format!(
                "I/O {name} size {} above MQES+1",
                u32::from(qsize0) + 1
            ));
            return (spec(specific::INVALID_QUEUE_SIZE), 0);
        }
        if !pc && self.cfg.cqr {
            self.violation(format!("non-contiguous I/O {name} with CAP.CQR"));
            return (gen(generic::INVALID_FIELD), 0);
        }
        let entries = u32::from(qsize0) + 1;
        let esize = if is_cq { 16 } else { 64 };
        if !base.is_multiple_of(PAGE) || !self.in_mem(base, u64::from(entries) * esize) {
            self.violation(format!(
                "I/O {name} base {base:#x} misaligned or outside memory"
            ));
            return (gen(generic::INVALID_PRP_OFFSET), 0);
        }
        let (field, want) = if is_cq { (20, 4) } else { (16, 6) };
        if (self.cc >> field) & 0xF != want {
            self.violation(format!("CC entry size for {name} not set"));
            return (gen(generic::INVALID_FIELD), 0);
        }
        if is_cq {
            self.cqs.insert(
                qid,
                Cq {
                    base,
                    entries,
                    head: 0,
                    tail: 0,
                    phase: true,
                },
            );
        } else {
            let cqid = (cmd.dw[11] >> 16) as u16;
            if cqid == 0 || !self.cqs.contains_key(&cqid) {
                return (spec(specific::INVALID_CQ), 0);
            }
            self.sqs.insert(
                qid,
                Sq {
                    base,
                    entries,
                    head: 0,
                    tail: 0,
                    cqid,
                    dirty: vec![false; entries as usize],
                    doorbell_at: 0,
                },
            );
        }
        (Status::SUCCESS, 0)
    }

    fn exec_io(&mut self, cmd: &Command) -> (Status, u32) {
        if cmd.nsid() != 1 {
            return (gen(generic::INVALID_NAMESPACE), 0);
        }
        match cmd.opcode() {
            nvm::FLUSH => (Status::SUCCESS, 0),
            nvm::READ | nvm::WRITE => {
                let slba = u64::from(cmd.dw[10]) | u64::from(cmd.dw[11]) << 32;
                let nlb = u64::from(cmd.dw[12] & 0xFFFF) + 1;
                if slba.checked_add(nlb).is_none_or(|e| e > self.cfg.nsze) {
                    return (gen(generic::LBA_OUT_OF_RANGE), 0);
                }
                let len = nlb << self.cfg.lba_shift;
                if self.cfg.mdts != 0 && len > PAGE << self.cfg.mdts {
                    self.violation(format!("transfer {len} above MDTS"));
                    return (gen(generic::INVALID_FIELD), 0);
                }
                let segs = match self.prp_segments(cmd.prp1(), cmd.prp2(), len) {
                    Ok(s) => s,
                    Err(msg) => {
                        self.violation(msg);
                        return (gen(generic::INVALID_PRP_OFFSET), 0);
                    }
                };
                self.stats.max_transfer = self.stats.max_transfer.max(len);
                let mut disk_off = (slba << self.cfg.lba_shift) as usize;
                for (pa, n) in segs {
                    let n = n as usize;
                    let o = (pa - MEM_BASE) as usize;
                    if cmd.opcode() == nvm::WRITE {
                        self.disk[disk_off..disk_off + n].copy_from_slice(&self.mem[o..o + n]);
                    } else {
                        self.mem[o..o + n].copy_from_slice(&self.disk[disk_off..disk_off + n]);
                    }
                    disk_off += n;
                }
                (Status::SUCCESS, 0)
            }
            _ => (gen(generic::INVALID_OPCODE), 0),
        }
    }

    // ---- registers ------------------------------------------------------------

    fn controller_reset(&mut self) {
        self.sqs.clear();
        self.cqs.clear();
        self.pending.clear();
        self.nq_set = None;
        self.shst_at = None;
        self.csts &= !SHST_MASK;
        if !self.faults.cfs_persistent {
            self.fatal = false;
            self.csts &= !CFS;
        }
        self.stats.resets += 1;
        self.en_at = if self.faults.rdy0_never {
            None
        } else {
            Some((false, self.now + self.cfg.ready_delay_ns))
        };
    }

    fn write_cc(&mut self, v: u32) {
        let old = self.cc;
        self.cc = v;
        match (old & 1 != 0, v & 1 != 0) {
            (false, true) => {
                if self.csts & RDY != 0 {
                    self.violation("CC.EN set while CSTS.RDY still 1".into());
                }
                let mps = ((v >> 7) & 0xF) as u8;
                if mps < self.cfg.mpsmin || mps > self.cfg.mpsmax {
                    self.violation(format!("CC.MPS {mps} outside CAP range"));
                }
                if (v >> 4) & 7 != 0 || (v >> 11) & 7 != 0 || v & SHN_MASK != 0 {
                    self.violation(format!("CC {v:#x}: CSS/AMS/SHN not zero on enable"));
                }
                if self.aqa & 0xFFF == 0 || (self.aqa >> 16) & 0xFFF == 0 {
                    self.violation("AQA sizes below 2".into());
                }
                if !self.asq.is_multiple_of(PAGE) || !self.acq.is_multiple_of(PAGE) {
                    self.violation("ASQ/ACQ not page aligned".into());
                }
                if self.faults.cfs_on_enable {
                    self.trigger_fatal();
                } else if !self.faults.rdy1_never {
                    self.en_at = Some((true, self.now + self.cfg.ready_delay_ns));
                }
            }
            (true, false) => self.controller_reset(),
            (true, true) => {
                if (old ^ v) & !SHN_MASK != 0 {
                    self.violation(format!("CC changed {old:#x} -> {v:#x} while enabled"));
                }
                if v & SHN_MASK != 0 && old & SHN_MASK == 0 {
                    self.csts = (self.csts & !SHST_MASK) | (1 << 2);
                    if !self.faults.shutdown_never {
                        self.shst_at = Some(self.now + self.cfg.shutdown_delay_ns);
                    }
                }
            }
            (false, false) => {}
        }
    }

    fn doorbell(&mut self, off: u32, v: u32) {
        self.stats.doorbells += 1;
        let stride = 4u64 << self.cfg.dstrd;
        let rel = u64::from(off) - 0x1000;
        if rel % stride != 0 {
            self.violation(format!("write {off:#x} between doorbells"));
            return;
        }
        if self.csts & RDY == 0 {
            self.violation(format!("doorbell {off:#x} while not ready"));
            return;
        }
        if self.csts & SHST_MASK == 2 << 2 {
            self.violation(format!("doorbell {off:#x} after shutdown"));
            return;
        }
        let idx = rel / stride;
        let qid = (idx / 2) as u16;
        let now = self.now;
        let mut errs = Vec::new();
        if idx.is_multiple_of(2) {
            match self.sqs.get_mut(&qid) {
                None => errs.push(format!("doorbell for missing SQ{qid}")),
                Some(sq) => {
                    let n = sq.entries;
                    if v >= n {
                        errs.push(format!("SQ{qid} tail {v} >= size {n}"));
                    } else {
                        let queued = (sq.tail + n - sq.head) % n;
                        let added = (v + n - sq.tail) % n;
                        if queued + added >= n {
                            errs.push(format!("SQ{qid} overflow"));
                        }
                        for k in 0..added {
                            let i = ((sq.tail + k) % n) as usize;
                            if !sq.dirty[i] {
                                errs.push(format!(
                                    "SQ{qid} doorbell covers entry {i} not written since last fetch"
                                ));
                            }
                        }
                        sq.tail = v;
                        sq.doorbell_at = now;
                    }
                }
            }
        } else {
            match self.cqs.get_mut(&qid) {
                None => errs.push(format!("doorbell for missing CQ{qid}")),
                Some(cq) => {
                    let n = cq.entries;
                    if v >= n {
                        errs.push(format!("CQ{qid} head {v} >= size {n}"));
                    } else {
                        let posted = (cq.tail + n - cq.head) % n;
                        let released = (v + n - cq.head) % n;
                        if released > posted {
                            errs.push(format!("CQ{qid} head {v} beyond posted entries"));
                        }
                        cq.head = v;
                    }
                }
            }
        }
        self.violations.extend(errs);
    }
}

impl Mmio for Model {
    fn read32(&mut self, off: u32) -> u32 {
        self.process();
        let cap = self.cap_raw();
        match off {
            0x00 => cap as u32,
            0x04 => (cap >> 32) as u32,
            0x08 => self.cfg.version,
            0x14 => self.cc,
            0x1C => self.csts,
            0x24 => self.aqa,
            0x28 => self.asq as u32,
            0x2C => (self.asq >> 32) as u32,
            0x30 => self.acq as u32,
            0x34 => (self.acq >> 32) as u32,
            _ => 0,
        }
    }

    fn write32(&mut self, off: u32, v: u32) {
        self.process();
        let enabled = self.cc & 1 != 0;
        match off {
            0x14 => self.write_cc(v),
            0x24 if enabled => self.violation("AQA written while enabled".into()),
            0x24 => self.aqa = v,
            0x0C | 0x10 => {}
            o if o >= 0x1000 => self.doorbell(o, v),
            o => self.violation(format!("write32 to {o:#x}")),
        }
    }

    fn read64(&mut self, off: u32) -> u64 {
        self.process();
        match off {
            0x00 => self.cap_raw(),
            0x28 => self.asq,
            0x30 => self.acq,
            o => {
                self.violation(format!("read64 of {o:#x}"));
                0
            }
        }
    }

    fn write64(&mut self, off: u32, v: u64) {
        self.process();
        if self.cc & 1 != 0 {
            self.violation(format!("write64 to {off:#x} while enabled"));
            return;
        }
        match off {
            0x28 => self.asq = v,
            0x30 => self.acq = v,
            o => self.violation(format!("write64 to {o:#x}")),
        }
    }
}

impl DmaMemory for Model {
    fn read(&mut self, pa: u64, buf: &mut [u8]) {
        if !self.mem_read(pa, buf) {
            self.violation(format!("host read {pa:#x}+{} outside memory", buf.len()));
        }
    }

    fn write(&mut self, pa: u64, data: &[u8]) {
        if !self.mem_write(pa, data) {
            self.violation(format!("host write {pa:#x}+{} outside memory", data.len()));
            return;
        }
        let end = pa + data.len() as u64;
        let mut errs = Vec::new();
        for (qid, sq) in &mut self.sqs {
            let qend = sq.base + u64::from(sq.entries) * 64;
            if pa >= qend || end <= sq.base {
                continue;
            }
            let first = (pa.max(sq.base) - sq.base) / 64;
            let last = (end.min(qend) - 1 - sq.base) / 64;
            let n = sq.entries;
            for i in first as u32..=last as u32 {
                if (i + n - sq.head) % n < (sq.tail + n - sq.head) % n {
                    errs.push(format!("host overwrote unfetched SQ{qid} entry {i}"));
                }
                sq.dirty[i as usize] = true;
            }
        }
        self.violations.extend(errs);
    }
}

impl Clock for Model {
    fn now_ns(&mut self) -> u64 {
        self.now += self.cfg.tick_ns;
        self.process();
        self.now
    }
}

// ---- driver-side helpers -----------------------------------------------------

pub fn config() -> Config {
    Config {
        bar_size: 0x4000,
        admin: QueueMemory {
            sq: ADMIN_SQ,
            cq: ADMIN_CQ,
            entries: 16,
        },
        io: QueueMemory {
            sq: IO_SQ,
            cq: IO_CQ,
            entries: 32,
        },
        identify_buffer: IDENTIFY,
        prp_pool: PRP_POOL,
        prp_pages_per_command: 2,
        nsid: 1,
        io_vector: None,
        admin_timeout_ns: 200_000_000,
        io_timeout_ns: 50_000_000,
        abort_timeout_ns: 20_000_000,
        shutdown_timeout_ns: 100_000_000,
    }
}

/// Model plus initialised controller.
pub fn ready(mcfg: ModelConfig, cfg: Config) -> (Model, Controller) {
    let mut m = Model::new(mcfg);
    let mut c = Controller::new(&mut m, cfg).expect("new");
    c.init(&mut m).expect("init");
    (m, c)
}

/// A data buffer described by its pages.
#[derive(Clone, Debug)]
pub struct Buf {
    pub pages: Vec<u64>,
    pub offset: u32,
    pub len: u32,
}

impl Buf {
    pub fn contiguous(base: u64, offset: u32, len: u32) -> Self {
        let n = (u64::from(offset) + u64::from(len)).div_ceil(PAGE);
        Self {
            pages: (0..n).map(|i| base + i * PAGE).collect(),
            offset,
            len,
        }
    }

    pub fn data(&self) -> DataBuffer<'_> {
        DataBuffer {
            pages: &self.pages,
            offset: self.offset,
            len: self.len,
        }
    }

    fn for_each(&self, mut f: impl FnMut(u64, std::ops::Range<usize>)) {
        let mut done = 0usize;
        let mut pos = u64::from(self.offset);
        while done < self.len as usize {
            let page = (pos / PAGE) as usize;
            let in_page = pos % PAGE;
            let n = ((PAGE - in_page) as usize).min(self.len as usize - done);
            f(self.pages[page] + in_page, done..done + n);
            done += n;
            pos += n as u64;
        }
    }

    pub fn fill(&self, m: &mut Model, data: &[u8]) {
        assert_eq!(data.len(), self.len as usize);
        self.for_each(|pa, r| m.write(pa, &data[r]));
    }

    pub fn read_back(&self, m: &mut Model) -> Vec<u8> {
        let mut out = vec![0u8; self.len as usize];
        self.for_each(|pa, r| m.read(pa, &mut out[r]));
        out
    }
}

/// Deterministic xorshift64* generator.
#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Polls until `tags` are all reported; panics on a duplicate report, on
/// a report for an unknown tag or when `max_polls` is exceeded.
pub fn wait_all(
    m: &mut Model,
    c: &mut Controller,
    tags: &[u64],
    max_polls: usize,
) -> HashMap<u64, Outcome> {
    let mut got: HashMap<u64, Outcome> = HashMap::new();
    for _ in 0..max_polls {
        let mut batch: Vec<Completion> = Vec::new();
        c.poll(m, &mut |x| batch.push(x)).expect("poll");
        for x in batch {
            assert!(tags.contains(&x.tag), "unexpected tag {}", x.tag);
            assert!(
                got.insert(x.tag, x.outcome).is_none(),
                "tag {} twice",
                x.tag
            );
        }
        if got.len() == tags.len() {
            return got;
        }
    }
    panic!("only {} of {} commands completed", got.len(), tags.len());
}

/// Runs one request to completion and returns its outcome.
pub fn run(m: &mut Model, c: &mut Controller, tag: u64, req: hw_nvme::Request<'_>) -> Outcome {
    c.submit(m, tag, req).expect("submit");
    wait_all(m, c, &[tag], 1_000_000)[&tag]
}

pub fn write_blocks(
    m: &mut Model,
    c: &mut Controller,
    buf: &Buf,
    lba: u64,
    data: &[u8],
) -> Outcome {
    buf.fill(m, data);
    let blocks = buf.len >> c.namespace().unwrap().lba_shift();
    run(
        m,
        c,
        0xAAAA,
        hw_nvme::Request::Write {
            lba,
            blocks,
            buffer: buf.data(),
        },
    )
}

pub fn read_blocks(m: &mut Model, c: &mut Controller, buf: &Buf, lba: u64) -> (Outcome, Vec<u8>) {
    let blocks = buf.len >> c.namespace().unwrap().lba_shift();
    let o = run(
        m,
        c,
        0xBBBB,
        hw_nvme::Request::Read {
            lba,
            blocks,
            buffer: buf.data(),
        },
    );
    (o, buf.read_back(m))
}

pub const OK: Outcome = Outcome::Success { dw0: 0 };
