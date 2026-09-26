//! Host models shared by the integration tests: physical memory, a frame
//! allocator with fault injection, a PRNG, ACPI fixture readers and
//! behavioural models of the VT-d and AMD-Vi translation hardware.
//!
//! The hardware models decode the raw bits in memory on their own,
//! following the specification layouts, instead of calling the crate's
//! decoders, so a layout mistake in the crate shows up as a translation
//! failure. Each model has an IOTLB (and a context/DTE cache) that is only
//! cleared by invalidation commands read from the command ring.

#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};

use hw_iommu::{Access, DmaRegion, FrameAlloc, PhysMem, PAGE_SIZE};

/// Base of the modelled physical memory.
pub const MEM_BASE: u64 = 0x10_0000;
/// Frames of modelled physical memory.
pub const MEM_FRAMES: u64 = 4096;
/// Command ring (1 frame, 256 entries).
pub const RING_BASE: u64 = MEM_BASE;
/// Status word of the invalidation queue.
pub const STATUS_ADDR: u64 = MEM_BASE + PAGE_SIZE;
/// Second status word (second queue).
pub const STATUS_ADDR_2: u64 = MEM_BASE + PAGE_SIZE + 8;
/// AMD-Vi device table (4 frames: DeviceIDs 0..512, buses 0 and 1).
pub const DEV_TABLE_BASE: u64 = MEM_BASE + 2 * PAGE_SIZE;
/// Pages of the device table.
pub const DEV_TABLE_PAGES: u32 = 4;
/// Region used for DMA buffers (64 frames).
pub const DMA_BASE: u64 = MEM_BASE + 16 * PAGE_SIZE;
/// First frame of the allocator pool.
pub const POOL_BASE: u64 = MEM_BASE + 128 * PAGE_SIZE;
/// Garbage that fills memory the crate has not initialised.
pub const GARBAGE: u64 = 0xDEAD_BEEF_A5A5_5A5A;

/// Word-addressed physical memory.
pub struct ArrayMem {
    words: Vec<u64>,
}

impl ArrayMem {
    pub fn new() -> Self {
        Self {
            words: vec![GARBAGE; (MEM_FRAMES * PAGE_SIZE / 8) as usize],
        }
    }

    fn index(pa: u64) -> usize {
        assert_eq!(pa % 8, 0, "unaligned access {pa:#x}");
        assert!(
            (MEM_BASE..MEM_BASE + MEM_FRAMES * PAGE_SIZE).contains(&pa),
            "access outside modelled memory {pa:#x}"
        );
        ((pa - MEM_BASE) / 8) as usize
    }

    /// Copy of one frame.
    pub fn frame(&self, pa: u64) -> Vec<u64> {
        let i = Self::index(pa);
        self.words[i..i + 512].to_vec()
    }

    pub fn contains(pa: u64) -> bool {
        (MEM_BASE..MEM_BASE + MEM_FRAMES * PAGE_SIZE).contains(&pa)
    }
}

impl PhysMem for ArrayMem {
    fn read_u64(&self, pa: u64) -> u64 {
        self.words[Self::index(pa)]
    }

    fn write_u64(&mut self, pa: u64, value: u64) {
        let i = Self::index(pa);
        self.words[i] = value;
    }
}

/// Frame allocator over the pool with failure injection and misuse checks.
pub struct TestAlloc {
    free: Vec<u64>,
    /// Frames currently handed out.
    pub live: BTreeSet<u64>,
    /// Successful allocations left before `alloc_frame` returns `None`.
    pub fail_after: Option<usize>,
    /// Next allocation returns this (bogus) address instead of a frame.
    pub bad_frame: Option<u64>,
    /// Total successful allocations.
    pub allocs: usize,
}

impl TestAlloc {
    pub fn new() -> Self {
        let first = (POOL_BASE - MEM_BASE) / PAGE_SIZE;
        let free = (first..MEM_FRAMES)
            .rev()
            .map(|f| MEM_BASE + f * PAGE_SIZE)
            .collect();
        Self {
            free,
            live: BTreeSet::new(),
            fail_after: None,
            bad_frame: None,
            allocs: 0,
        }
    }
}

impl FrameAlloc for TestAlloc {
    fn alloc_frame(&mut self) -> Option<u64> {
        if let Some(n) = self.fail_after {
            if n == 0 {
                return None;
            }
            self.fail_after = Some(n - 1);
        }
        if let Some(bad) = self.bad_frame.take() {
            self.live.insert(bad);
            return Some(bad);
        }
        let pa = self.free.pop()?;
        self.live.insert(pa);
        self.allocs += 1;
        Some(pa)
    }

    fn free_frame(&mut self, pa: u64) {
        assert!(
            self.live.remove(&pa),
            "free of a frame that is not allocated: {pa:#x}"
        );
        if pa.is_multiple_of(PAGE_SIZE) && ArrayMem::contains(pa) {
            self.free.push(pa);
        }
    }
}

/// Deterministic xorshift64* generator.
pub struct Prng(u64);

impl Prng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

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
}

/// DMA buffer handle used by the lifecycle tests.
#[derive(Debug, PartialEq, Eq)]
pub struct Buf {
    pub phys: u64,
    pub size: u64,
}

impl DmaRegion for Buf {
    fn phys(&self) -> u64 {
        self.phys
    }
    fn size(&self) -> u64 {
        self.size
    }
}

// ---------------------------------------------------------------------
// ACPI fixtures (tests/fixtures/acpi, captured from QEMU/OVMF and from the
// candidate machine). Minimal readers: just enough to take the register
// base and widths; full parsing belongs to hw-acpi.

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn checked_table<'a>(b: &'a [u8], sig: &[u8; 4]) -> &'a [u8] {
    assert_eq!(&b[0..4], sig);
    let len = u32_at(b, 4) as usize;
    assert_eq!(len, b.len());
    assert_eq!(b.iter().fold(0u8, |a, x| a.wrapping_add(*x)), 0, "checksum");
    b
}

/// DMAR: (host address width in bits, DRHD register bases).
pub fn dmar_drhd(bytes: &[u8]) -> (u32, Vec<u64>) {
    let b = checked_table(bytes, b"DMAR");
    let haw = u32::from(b[36]) + 1;
    let mut bases = Vec::new();
    let mut off = 48;
    while off + 4 <= b.len() {
        let (ty, len) = (u16_at(b, off), usize::from(u16_at(b, off + 2)));
        assert!(len >= 4 && off + len <= b.len());
        if ty == 0 {
            bases.push(u64_at(b, off + 8));
        }
        off += len;
    }
    (haw, bases)
}

/// One IVHD block of an IVRS table.
#[derive(Debug)]
pub struct Ivhd {
    pub ty: u8,
    pub devid: u16,
    pub base: u64,
    /// EFR image (types 11h and 40h).
    pub efr: Option<u64>,
}

/// IVRS: (IVinfo, IVHD blocks).
pub fn ivrs(bytes: &[u8]) -> (u32, Vec<Ivhd>) {
    let b = checked_table(bytes, b"IVRS");
    let ivinfo = u32_at(b, 36);
    let mut blocks = Vec::new();
    let mut off = 48;
    while off + 4 <= b.len() {
        let (ty, len) = (b[off], usize::from(u16_at(b, off + 2)));
        assert!(len >= 4 && off + len <= b.len());
        if matches!(ty, 0x10 | 0x11 | 0x40) {
            let efr = if ty == 0x10 {
                None
            } else {
                Some(u64_at(b, off + 24))
            };
            blocks.push(Ivhd {
                ty,
                devid: u16_at(b, off + 4),
                base: u64_at(b, off + 8),
                efr,
            });
        }
        off += len;
    }
    (ivinfo, blocks)
}

// ---------------------------------------------------------------------
// Hardware models.

/// DMA outcome reported by the hardware models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// VT-d root entry P = 0.
    RootNotPresent,
    /// VT-d context entry P = 0.
    ContextNotPresent,
    /// Page-table entry not present.
    NotPresent,
    /// Read without read permission.
    ReadDenied,
    /// Write without write permission.
    WriteDenied,
    /// IOVA beyond the address width.
    AddressWidth,
    /// AMD-Vi DTE blocks the device (Mode 0 without permission).
    TargetAbort,
    /// DeviceID beyond the device table.
    DevTableRange,
    /// Structure the model does not accept.
    Malformed,
}

const ADDR: u64 = 0x000F_FFFF_FFFF_F000;

#[derive(Clone, Copy, Debug)]
struct Tlb {
    page: u64,
    read: bool,
    write: bool,
}

fn check(read: bool, write: bool, access: Access) -> Result<(), Fault> {
    match access {
        Access::Read if !read => Err(Fault::ReadDenied),
        Access::Write if !write => Err(Fault::WriteDenied),
        _ => Ok(()),
    }
}

/// Minimal command-ring consumer shared by both models.
fn consume_ring(
    mem: &mut ArrayMem,
    base: u64,
    entries: u64,
    head: &mut u64,
    tail_offset: u64,
    budget: usize,
    mut exec: impl FnMut(&mut ArrayMem, [u64; 2]),
) -> u64 {
    let tail = tail_offset / 16;
    let mut done = 0;
    while *head != tail && done < budget {
        let slot = base + *head * 16;
        let desc = [mem.read_u64(slot), mem.read_u64(slot + 8)];
        exec(mem, desc);
        *head = (*head + 1) % entries;
        done += 1;
    }
    *head * 16
}

#[derive(Clone, Copy, Debug)]
struct VtdCtx {
    did: u16,
    tt: u64,
    slpt: u64,
    aw: u64,
}

/// VT-d remapping unit (legacy mode, second-level translation only).
pub struct VtdHw {
    /// RTADDR_REG.
    pub rtaddr: u64,
    /// MGAW + 1 from the modelled CAP register.
    pub mgaw: u32,
    ctx_cache: HashMap<u16, VtdCtx>,
    iotlb: HashMap<(u16, u64), Tlb>,
    head: u64,
    /// IOTLB hits (to show that a stale translation was used).
    pub iotlb_hits: u64,
}

impl VtdHw {
    pub fn new(rtaddr: u64, mgaw: u32) -> Self {
        Self {
            rtaddr,
            mgaw,
            ctx_cache: HashMap::new(),
            iotlb: HashMap::new(),
            head: 0,
            iotlb_hits: 0,
        }
    }

    pub fn iotlb_len(&self) -> usize {
        self.iotlb.len()
    }

    /// Queue re-initialisation: the head returns to 0.
    pub fn reset_queue(&mut self) {
        self.head = 0;
    }

    fn context(&mut self, mem: &ArrayMem, sid: u16) -> Result<VtdCtx, Fault> {
        if let Some(c) = self.ctx_cache.get(&sid) {
            return Ok(*c);
        }
        let root = self.rtaddr + u64::from(sid >> 8) * 16;
        let rlo = mem.read_u64(root);
        if rlo & 1 == 0 {
            return Err(Fault::RootNotPresent);
        }
        let ctx = (rlo & ADDR) + u64::from(sid & 0xff) * 16;
        let (lo, hi) = (mem.read_u64(ctx), mem.read_u64(ctx + 8));
        if lo & 1 == 0 {
            return Err(Fault::ContextNotPresent);
        }
        let c = VtdCtx {
            did: (hi >> 8) as u16,
            tt: (lo >> 2) & 3,
            slpt: lo & ADDR,
            aw: hi & 7,
        };
        self.ctx_cache.insert(sid, c);
        Ok(c)
    }

    /// Translates one DMA access of requester `sid` to IOVA `iova`.
    pub fn translate(
        &mut self,
        mem: &ArrayMem,
        sid: u16,
        iova: u64,
        access: Access,
    ) -> Result<u64, Fault> {
        let c = self.context(mem, sid)?;
        match c.tt {
            0 => {}
            2 => return Ok(iova),
            _ => return Err(Fault::Malformed),
        }
        let levels = match c.aw {
            1 => 3,
            2 => 4,
            _ => return Err(Fault::Malformed),
        };
        let width = (12 + 9 * levels).min(self.mgaw);
        if iova >> width != 0 {
            return Err(Fault::AddressWidth);
        }
        let key = (c.did, iova >> 12);
        if let Some(t) = self.iotlb.get(&key) {
            self.iotlb_hits += 1;
            check(t.read, t.write, access)?;
            return Ok(t.page | (iova & 0xfff));
        }
        let (mut table, mut read, mut write) = (c.slpt, true, true);
        for level in (1..=levels).rev() {
            let e = mem.read_u64(table + ((iova >> (12 + 9 * (level - 1))) & 0x1ff) * 8);
            if e & 3 == 0 {
                return Err(Fault::NotPresent);
            }
            read &= e & 1 != 0;
            write &= e & 2 != 0;
            if level > 1 && e & (1 << 7) != 0 {
                return Err(Fault::Malformed);
            }
            table = e & ADDR;
        }
        check(read, write, access)?;
        self.iotlb.insert(
            key,
            Tlb {
                page: table,
                read,
                write,
            },
        );
        Ok(table | (iova & 0xfff))
    }

    /// Executes up to `budget` descriptors between the model's head and
    /// `tail_offset`; returns the new head offset (IQH).
    pub fn run_queue(
        &mut self,
        mem: &mut ArrayMem,
        ring_base: u64,
        tail_offset: u64,
        budget: usize,
    ) -> u64 {
        let mut head = self.head;
        let (ctx_cache, iotlb) = (&mut self.ctx_cache, &mut self.iotlb);
        let h = consume_ring(
            mem,
            ring_base,
            256,
            &mut head,
            tail_offset,
            budget,
            |mem, [lo, hi]| {
                let gran = (lo >> 4) & 3;
                let did = (lo >> 16) as u16;
                match lo & 0xf {
                    0x1 => match gran {
                        1 => ctx_cache.clear(),
                        2 => ctx_cache.retain(|_, c| c.did != did),
                        3 => {
                            ctx_cache.remove(&((lo >> 32) as u16));
                        }
                        _ => panic!("bad context-cache granularity"),
                    },
                    0x2 => match gran {
                        1 => iotlb.clear(),
                        2 => iotlb.retain(|k, _| k.0 != did),
                        3 => {
                            let am = hi & 0x3f;
                            let first = (hi & !0xfff) >> 12;
                            assert_eq!(
                                first % (1 << am),
                                0,
                                "page-selective address not aligned to mask"
                            );
                            iotlb.retain(|k, _| {
                                k.0 != did || k.1 < first || k.1 >= first + (1 << am)
                            });
                        }
                        _ => panic!("bad IOTLB granularity"),
                    },
                    0x5 => {
                        if lo & (1 << 5) != 0 {
                            let addr = hi & !3;
                            let word = addr & !7;
                            let shift = (addr & 4) * 8;
                            let old = mem.read_u64(word);
                            let new = (old & !(0xffff_ffff << shift)) | ((lo >> 32) << shift);
                            mem.write_u64(word, new);
                        }
                    }
                    t => panic!("unexpected VT-d descriptor type {t:#x}"),
                }
            },
        );
        self.head = head;
        h
    }
}

/// AMD-Vi IOMMU (host translation only).
pub struct AmdHw {
    /// Device Table Base Address Register.
    pub dev_tab_reg: u64,
    dte_cache: HashMap<u16, [u64; 2]>,
    iotlb: HashMap<(u16, u64), Tlb>,
    head: u64,
    /// IOTLB hits (to show that a stale translation was used).
    pub iotlb_hits: u64,
}

impl AmdHw {
    pub fn new(dev_tab_reg: u64) -> Self {
        Self {
            dev_tab_reg,
            dte_cache: HashMap::new(),
            iotlb: HashMap::new(),
            head: 0,
            iotlb_hits: 0,
        }
    }

    pub fn iotlb_len(&self) -> usize {
        self.iotlb.len()
    }

    /// Queue re-initialisation: the head returns to 0.
    pub fn reset_queue(&mut self) {
        self.head = 0;
    }

    /// Translates one DMA access of DeviceID `devid` to IOVA `iova`.
    pub fn translate(
        &mut self,
        mem: &ArrayMem,
        devid: u16,
        iova: u64,
        access: Access,
    ) -> Result<u64, Fault> {
        let size = ((self.dev_tab_reg & 0x1ff) + 1) * 4096;
        if u64::from(devid) * 32 >= size {
            return Err(Fault::DevTableRange);
        }
        let [q0, q1] = match self.dte_cache.get(&devid) {
            Some(d) => *d,
            None => {
                let slot = (self.dev_tab_reg & ADDR) + u64::from(devid) * 32;
                let d = [mem.read_u64(slot), mem.read_u64(slot + 8)];
                self.dte_cache.insert(devid, d);
                d
            }
        };
        if q0 & 1 == 0 {
            // V = 0: no translation, no checks.
            return Ok(iova);
        }
        if q0 & 2 == 0 {
            return Err(Fault::Malformed);
        }
        let (dte_r, dte_w) = (q0 >> 61 & 1 != 0, q0 >> 62 & 1 != 0);
        let mode = (q0 >> 9) & 7;
        if mode == 0 {
            return match check(dte_r, dte_w, access) {
                Ok(()) => Ok(iova),
                Err(_) => Err(Fault::TargetAbort),
            };
        }
        if mode > 6 {
            return Err(Fault::Malformed);
        }
        let width = 12 + 9 * mode;
        if width < 64 && iova >> width != 0 {
            return Err(Fault::AddressWidth);
        }
        let domid = q1 as u16;
        let key = (domid, iova >> 12);
        if let Some(t) = self.iotlb.get(&key) {
            self.iotlb_hits += 1;
            check(t.read, t.write, access)?;
            return Ok(t.page | (iova & 0xfff));
        }
        let (mut table, mut read, mut write) = (q0 & ADDR, dte_r, dte_w);
        for level in (1..=mode).rev() {
            let e = mem.read_u64(table + ((iova >> (12 + 9 * (level - 1))) & 0x1ff) * 8);
            if e & 1 == 0 {
                return Err(Fault::NotPresent);
            }
            if (e >> 9) & 7 != level - 1 {
                return Err(Fault::Malformed);
            }
            read &= e >> 61 & 1 != 0;
            write &= e >> 62 & 1 != 0;
            table = e & ADDR;
        }
        check(read, write, access)?;
        self.iotlb.insert(
            key,
            Tlb {
                page: table,
                read,
                write,
            },
        );
        Ok(table | (iova & 0xfff))
    }

    /// Executes up to `budget` commands; returns the new head offset.
    pub fn run_queue(
        &mut self,
        mem: &mut ArrayMem,
        ring_base: u64,
        tail_offset: u64,
        budget: usize,
    ) -> u64 {
        let mut head = self.head;
        let (dte_cache, iotlb) = (&mut self.dte_cache, &mut self.iotlb);
        let h = consume_ring(
            mem,
            ring_base,
            256,
            &mut head,
            tail_offset,
            budget,
            |mem, [q0, q1]| match q0 >> 60 {
                0x1 => {
                    if q0 & 1 != 0 {
                        mem.write_u64(q0 & 0x000F_FFFF_FFFF_FFF8, q1);
                    }
                }
                0x2 => {
                    dte_cache.remove(&(q0 as u16));
                }
                0x3 => {
                    let domid = (q0 >> 32) as u16;
                    let (first, count) = decode_pages_range(q1);
                    iotlb.retain(|k, _| k.0 != domid || k.1 < first || k.1 - first >= count);
                }
                0x8 => {
                    dte_cache.clear();
                    iotlb.clear();
                }
                op => panic!("unexpected AMD-Vi command opcode {op:#x}"),
            },
        );
        self.head = head;
        h
    }
}

/// Decodes the address/S fields of INVALIDATE_IOMMU_PAGES into
/// (first page number, page count) following §2.4.3: with S = 1 the
/// lowest clear bit at or above bit 12 gives the size.
pub fn decode_pages_range(q1: u64) -> (u64, u64) {
    let addr = q1 & !0xfff;
    if q1 & 1 == 0 {
        return (addr >> 12, 1);
    }
    let zero = (!(addr | 0xfff)).trailing_zeros();
    if zero + 1 >= 64 {
        return (0, u64::MAX);
    }
    let size = 1u64 << (zero + 1);
    ((addr & !(size - 1)) >> 12, size >> 12)
}
