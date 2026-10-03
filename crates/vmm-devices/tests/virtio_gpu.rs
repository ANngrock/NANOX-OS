//! virtio-gpu through the platform bus, driven the way a guest driver does it:
//! PCI configuration cycles to find and enable the function, BAR 0 accesses
//! for the handshake and the queues, commands written into guest RAM, a
//! notification, and the response in the used ring. The display is a mock
//! that keeps real pixels and panics on every call the device must never
//! make (a resource it does not know, a run outside the resource, a flush of
//! what is not on display, a rectangle outside what is shown).

use std::collections::HashMap;

use vmm_devices::ioapic;
use vmm_devices::lapic::{self, reg};
use vmm_devices::machine::*;
use vmm_devices::virtio::*;
use vmm_devices::virtio_gpu::{
    Rect, Scanout, CHUNK, DEFAULT_HEIGHT, DEFAULT_WIDTH, FORMATS, HEADER, MAX_BACKING,
    MAX_DIMENSION, MAX_RESOURCES, SCANOUTS,
};

const T0: i64 = 1_790_944_496;
const BLK_BAR: u64 = 0xC000_0000;
const NET_BAR: u64 = 0xC000_4000;
const GPU_BAR: u64 = 0xC000_8000;

// Guest RAM: the two queues, the request and response buffers of one command
// at a time, the pages that back resources, and the buffers of a batch.
const CTL: u64 = 0x1000;
const CUR: u64 = 0x4000;
const REQ: u64 = 0x8000;
const RSP: u64 = 0xE000;
const PAGES: u64 = 0x1_0000;
const BREQ: u64 = 0x3_8000;
const BRSP: u64 = 0x3_C000;
const RAM: usize = 0x4_0000;

const OK: u32 = 0x1100;
const INFO: u32 = 0x1101;
const UNSPEC: u32 = 0x1200;
const NOMEM: u32 = 0x1201;
const BAD_SCANOUT: u32 = 0x1202;
const BAD_RES: u32 = 0x1203;
const BAD_PARAM: u32 = 0x1205;

const OFF: Rect = Rect {
    x: 0,
    y: 0,
    w: 0,
    h: 0,
};

fn rc(x: u32, y: u32, w: u32, h: u32) -> Rect {
    Rect { x, y, w, h }
}

struct Ram(Vec<u8>);

impl GuestMemory for Ram {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        let Some(end) = gpa.checked_add(buf.len() as u64) else {
            return false;
        };
        match self.0.get(gpa as usize..end as usize) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        let Some(end) = gpa.checked_add(data.len() as u64) else {
            return false;
        };
        match self.0.get_mut(gpa as usize..end as usize) {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

// ------------------------------------------------------------------ the display

#[derive(Clone, Debug, PartialEq)]
enum Ev {
    Create(u32, u32, u32, u32),
    Destroy(u32),
    Put(u32, u32, u32, Vec<u8>),
    Show(u32, u32, Rect),
    Present(u32, u32, Rect),
}

struct Surface {
    width: u32,
    height: u32,
    px: Vec<u8>,
}

#[derive(Default)]
struct Host {
    res: HashMap<u32, Surface>,
    log: Vec<Ev>,
    shown: u32,
    window: Rect,
    refuse_create: bool,
}

impl Host {
    fn take(&mut self) -> Vec<Ev> {
        std::mem::take(&mut self.log)
    }

    fn fb(&self, id: u32) -> &[u8] {
        &self.res[&id].px
    }
}

impl Scanout for Host {
    fn create(&mut self, resource: u32, width: u32, height: u32, format: u32) -> bool {
        assert_ne!(resource, 0, "resource 0 does not exist");
        assert!(!self.res.contains_key(&resource), "{resource} exists");
        assert!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "size {width}x{height}"
        );
        assert!(
            [1, 2, 3, 4, 67, 68, 121, 134].contains(&format),
            "format {format}"
        );
        self.log.push(Ev::Create(resource, width, height, format));
        if self.refuse_create {
            return false;
        }
        let px = if u64::from(width) * u64::from(height) <= 1 << 20 {
            vec![0; 4 * (width * height) as usize]
        } else {
            Vec::new()
        };
        self.res.insert(resource, Surface { width, height, px });
        true
    }

    fn destroy(&mut self, resource: u32) {
        assert!(
            self.res.remove(&resource).is_some(),
            "destroy of {resource}"
        );
        self.log.push(Ev::Destroy(resource));
    }

    fn put(&mut self, resource: u32, x: u32, y: u32, row: &[u8]) {
        let s = self
            .res
            .get_mut(&resource)
            .expect("put to an unknown resource");
        let n = row.len();
        assert!(
            n > 0 && n.is_multiple_of(4) && n <= 4096,
            "a run of {n} bytes"
        );
        assert!(
            u64::from(x) + (n / 4) as u64 <= u64::from(s.width) && y < s.height,
            "run of {n} bytes at ({x}, {y}) leaves the {}x{} resource",
            s.width,
            s.height
        );
        let at = 4 * (y * s.width + x) as usize;
        s.px[at..at + n].copy_from_slice(row);
        self.log.push(Ev::Put(resource, x, y, row.to_vec()));
    }

    fn show(&mut self, scanout: u32, resource: u32, rect: Rect) {
        assert_eq!(scanout, 0, "there is one scanout");
        if resource == 0 {
            assert_eq!(rect, OFF, "turning off carries no rectangle");
        } else {
            let s = self
                .res
                .get(&resource)
                .expect("showing an unknown resource");
            assert!(rect.w > 0 && rect.h > 0, "empty window");
            assert!(
                rect.x + rect.w <= s.width && rect.y + rect.h <= s.height,
                "window outside the resource"
            );
        }
        self.shown = resource;
        self.window = rect;
        self.log.push(Ev::Show(scanout, resource, rect));
    }

    fn present(&mut self, scanout: u32, resource: u32, rect: Rect) {
        assert_eq!(scanout, 0, "there is one scanout");
        assert!(
            self.shown != 0 && resource == self.shown,
            "flush of {resource}, which is not on display"
        );
        assert!(rect.w > 0 && rect.h > 0, "empty flush");
        let w = self.window;
        assert!(
            rect.x >= w.x
                && rect.y >= w.y
                && rect.x + rect.w <= w.x + w.w
                && rect.y + rect.h <= w.y + w.h,
            "flush {rect:?} outside the window {w:?}"
        );
        self.log.push(Ev::Present(scanout, resource, rect));
    }
}

// ------------------------------------------------------------- the driver side

fn cfg_addr(dev: u8, off: u32) -> u32 {
    (1 << 31) | (u32::from(dev) << 11) | (off & !3)
}

fn cfg_read(m: &mut Machine, dev: u8, off: u32, size: u8) -> u32 {
    m.io_out(PCI_ADDRESS, 4, cfg_addr(dev, off), 0);
    m.io_in(PCI_DATA + (off & 3) as u16, size, 0)
}

fn cfg_write(m: &mut Machine, dev: u8, off: u32, size: u8, v: u32) {
    m.io_out(PCI_ADDRESS, 4, cfg_addr(dev, off), 0);
    m.io_out(PCI_DATA + (off & 3) as u16, size, v, 0);
}

fn w(m: &mut Machine, bar: u64, off: u64, size: u8, v: u32) {
    m.mmio_write(bar + off, size, u64::from(v), 0);
}

fn r(m: &mut Machine, bar: u64, off: u64, size: u8) -> u32 {
    m.mmio_read(bar + off, size, 0) as u32
}

fn machine() -> Machine {
    Machine::new(T0, 100_000_000)
}

/// A virtqueue in guest RAM with the layout a driver would allocate.
struct Q {
    n: u16,
    desc: u64,
    avail: u64,
    used: u64,
    avail_idx: u16,
    next_desc: u16,
    seen_used: u16,
}

impl Q {
    fn new(base: u64, n: u16) -> Self {
        Self {
            n,
            desc: base,
            avail: base + 0x1000,
            used: base + 0x2000,
            avail_idx: 0,
            next_desc: 0,
            seen_used: 0,
        }
    }

    /// Writes a chain of (addr, len, device-writes) descriptors and makes it available; returns the head.
    fn add(&mut self, ram: &mut Ram, bufs: &[(u64, u32, bool)]) -> u16 {
        let head = self.next_desc;
        for (i, &(addr, len, write)) in bufs.iter().enumerate() {
            let idx = self.next_desc;
            self.next_desc = (self.next_desc + 1) % self.n;
            let last = i + 1 == bufs.len();
            let flags: u16 = (if write { 2 } else { 0 }) | (if last { 0 } else { 1 });
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&addr.to_le_bytes());
            d[8..12].copy_from_slice(&len.to_le_bytes());
            d[12..14].copy_from_slice(&flags.to_le_bytes());
            d[14..16].copy_from_slice(&self.next_desc.to_le_bytes());
            assert!(ram.write(self.desc + 16 * u64::from(idx), &d));
        }
        let slot = self.avail + 4 + 2 * u64::from(self.avail_idx % self.n);
        assert!(ram.write(slot, &head.to_le_bytes()));
        self.avail_idx = self.avail_idx.wrapping_add(1);
        assert!(ram.write(self.avail + 2, &self.avail_idx.to_le_bytes()));
        head
    }

    /// The (head, written) entries the device returned since the last call.
    fn take_used(&mut self, ram: &Ram) -> Vec<(u32, u32)> {
        let mut idx = [0u8; 2];
        assert!(ram.read(self.used + 2, &mut idx));
        let idx = u16::from_le_bytes(idx);
        let mut out = Vec::new();
        while self.seen_used != idx {
            let mut e = [0u8; 8];
            assert!(ram.read(
                self.used + 4 + 8 * u64::from(self.seen_used % self.n),
                &mut e
            ));
            out.push((
                u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
                u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            ));
            self.seen_used = self.seen_used.wrapping_add(1);
        }
        out
    }
}

/// Places BAR 0, enables memory and bus mastering, and runs the handshake up to DRIVER_OK.
fn bring_up(m: &mut Machine, dev: u8, bar: u64, queues: &[&Q], accept: u64) -> u8 {
    handshake(m, dev, bar, queues, accept, true)
}

/// The same, optionally stopping short of DRIVER_OK.
fn handshake(
    m: &mut Machine,
    dev: u8,
    bar: u64,
    queues: &[&Q],
    accept: u64,
    driver_ok: bool,
) -> u8 {
    cfg_write(m, dev, 0x10, 4, bar as u32);
    cfg_write(m, dev, 0x04, 2, 0x0006);
    w(m, bar, 0x14, 1, 0);
    w(m, bar, 0x14, 1, u32::from(STATUS_ACKNOWLEDGE));
    w(
        m,
        bar,
        0x14,
        1,
        u32::from(STATUS_ACKNOWLEDGE | STATUS_DRIVER),
    );
    w(m, bar, 0x08, 4, 0);
    w(m, bar, 0x0C, 4, accept as u32);
    w(m, bar, 0x08, 4, 1);
    w(m, bar, 0x0C, 4, (accept >> 32) as u32);
    w(
        m,
        bar,
        0x14,
        1,
        u32::from(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK),
    );
    for (i, q) in queues.iter().enumerate() {
        w(m, bar, 0x16, 2, i as u32);
        w(m, bar, 0x18, 2, u32::from(q.n));
        w(m, bar, 0x20, 4, q.desc as u32);
        w(m, bar, 0x24, 4, (q.desc >> 32) as u32);
        w(m, bar, 0x28, 4, q.avail as u32);
        w(m, bar, 0x2C, 4, (q.avail >> 32) as u32);
        w(m, bar, 0x30, 4, q.used as u32);
        w(m, bar, 0x34, 4, (q.used >> 32) as u32);
        w(m, bar, 0x1C, 2, 1);
    }
    let s = r(m, bar, 0x14, 1) as u8;
    if s & STATUS_FEATURES_OK != 0 && driver_ok {
        w(m, bar, 0x14, 1, u32::from(s | STATUS_DRIVER_OK));
    }
    r(m, bar, 0x14, 1) as u8
}

// ------------------------------------------------------------------ commands

fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put_rect(v: &mut Vec<u8>, r: Rect) {
    for x in [r.x, r.y, r.w, r.h] {
        put32(v, x);
    }
}

fn hdr(kind: u32) -> Vec<u8> {
    let mut v = vec![0u8; HEADER];
    v[..4].copy_from_slice(&kind.to_le_bytes());
    v
}

/// The same command with the fence flag and a fence id.
fn fenced(mut v: Vec<u8>, id: u64) -> Vec<u8> {
    v[4..8].copy_from_slice(&1u32.to_le_bytes());
    v[8..16].copy_from_slice(&id.to_le_bytes());
    v
}

fn display_info() -> Vec<u8> {
    hdr(0x100)
}

fn create(id: u32, format: u32, w: u32, h: u32) -> Vec<u8> {
    let mut v = hdr(0x101);
    for x in [id, format, w, h] {
        put32(&mut v, x);
    }
    v
}

fn unref(id: u32) -> Vec<u8> {
    let mut v = hdr(0x102);
    put32(&mut v, id);
    put32(&mut v, 0);
    v
}

fn scanout_cmd(r: Rect, scanout: u32, id: u32) -> Vec<u8> {
    let mut v = hdr(0x103);
    put_rect(&mut v, r);
    put32(&mut v, scanout);
    put32(&mut v, id);
    v
}

fn flush(r: Rect, id: u32) -> Vec<u8> {
    let mut v = hdr(0x104);
    put_rect(&mut v, r);
    put32(&mut v, id);
    put32(&mut v, 0);
    v
}

fn transfer(r: Rect, offset: u64, id: u32) -> Vec<u8> {
    let mut v = hdr(0x105);
    put_rect(&mut v, r);
    put64(&mut v, offset);
    put32(&mut v, id);
    put32(&mut v, 0);
    v
}

/// The fixed part of RESOURCE_ATTACH_BACKING; `n` entries are said to follow.
fn attach_head(id: u32, n: u32) -> Vec<u8> {
    let mut v = hdr(0x106);
    put32(&mut v, id);
    put32(&mut v, n);
    v
}

fn entries(e: &[(u64, u32)]) -> Vec<u8> {
    let mut v = Vec::new();
    for &(addr, len) in e {
        put64(&mut v, addr);
        put32(&mut v, len);
        put32(&mut v, 0);
    }
    v
}

fn attach(id: u32, e: &[(u64, u32)]) -> Vec<u8> {
    let mut v = attach_head(id, e.len() as u32);
    v.extend(entries(e));
    v
}

fn detach(id: u32) -> Vec<u8> {
    let mut v = hdr(0x107);
    put32(&mut v, id);
    put32(&mut v, 0);
    v
}

/// UPDATE_CURSOR (0x300) or MOVE_CURSOR (0x301): 56 bytes, no response.
fn cursor_cmd(kind: u32) -> Vec<u8> {
    let mut v = hdr(kind);
    for x in [0u32, 17, 23, 0, 5, 3, 4, 0] {
        put32(&mut v, x);
    }
    v
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Guest data that tells every byte apart from its neighbours and its seed.
fn pat(i: usize) -> u8 {
    (i.wrapping_mul(131) ^ (i >> 7) ^ 0x5A) as u8
}

fn data(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| pat(i + 977 * seed)).collect()
}

/// What a host surface `w` pixels wide holds after the rectangle `r` of `bytes` (rows one resource row
/// apart from `offset` on) was copied over `before`.
fn expect_fb(before: &[u8], w: u32, r: Rect, offset: usize, bytes: &[u8]) -> Vec<u8> {
    let mut fb = before.to_vec();
    let stride = 4 * w as usize;
    for line in 0..r.h as usize {
        for k in 0..4 * r.w as usize {
            fb[(r.y as usize + line) * stride + 4 * r.x as usize + k] =
                bytes[offset + line * stride + k];
        }
    }
    fb
}

/// The runs the host must be given for that transfer: rows in order, each cut into runs of 1024 pixels.
fn expect_puts(id: u32, r: Rect, offset: usize, w: u32, bytes: &[u8]) -> Vec<Ev> {
    let stride = 4 * w as usize;
    let mut out = Vec::new();
    for line in 0..r.h as usize {
        let mut x = 0usize;
        while x < r.w as usize {
            let n = (r.w as usize - x).min(1024);
            let from = offset + line * stride + 4 * x;
            out.push(Ev::Put(
                id,
                r.x + x as u32,
                r.y + line as u32,
                bytes[from..from + 4 * n].to_vec(),
            ));
            x += n;
        }
    }
    out
}

// ---------------------------------------------------------------------- the rig

struct Out {
    /// The bytes of every response buffer, back to back (0xEE where the device wrote nothing).
    bytes: Vec<u8>,
    used: Vec<(u32, u32)>,
    served: (u32, u32),
}

impl Out {
    fn code(&self) -> u32 {
        le32(&self.bytes, 0)
    }
    fn flags(&self) -> u32 {
        le32(&self.bytes, 4)
    }
    fn fence(&self) -> u64 {
        u64::from(le32(&self.bytes, 12)) << 32 | u64::from(le32(&self.bytes, 8))
    }
    /// The context id, ring index and padding of the response header.
    fn rest(&self) -> &[u8] {
        &self.bytes[16..HEADER]
    }
    /// The one completion of a command: how many bytes it says the device wrote.
    fn written(&self) -> u32 {
        assert_eq!(self.used.len(), 1, "one completion");
        self.used[0].1
    }
}

struct Rig {
    m: Machine,
    ram: Ram,
    host: Host,
    ctl: Q,
    cur: Q,
    n: u16,
}

fn rig() -> Rig {
    rig_with(16)
}

fn rig_with(n: u16) -> Rig {
    let mut m = machine();
    let ctl = Q::new(CTL, n);
    let cur = Q::new(CUR, n);
    assert_eq!(
        bring_up(&mut m, 5, GPU_BAR, &[&ctl, &cur], F_VERSION_1),
        0xF
    );
    Rig {
        m,
        ram: Ram(vec![0; RAM]),
        host: Host::default(),
        ctl,
        cur,
        n,
    }
}

impl Rig {
    fn kick(&mut self) -> (u32, u32) {
        w(&mut self.m, GPU_BAR, 0x3000, 4, 0);
        self.m.service_gpu(&mut self.ram, &mut self.host, 0)
    }

    fn kick_cursor(&mut self) -> (u32, u32) {
        w(&mut self.m, GPU_BAR, 0x3004, 4, 0);
        self.m.service_gpu(&mut self.ram, &mut self.host, 0)
    }

    fn status(&self) -> u8 {
        self.m.gpu.t.status()
    }

    fn needs_reset(&self) -> bool {
        self.status() & STATUS_NEEDS_RESET != 0
    }

    fn clear(&mut self) {
        self.host.take();
    }

    /// One control command: request buffers at the given places, response buffers of the given sizes.
    fn submit_at(&mut self, req: &[(u64, u32)], caps: &[u32]) -> Out {
        let mut bufs: Vec<(u64, u32, bool)> = req.iter().map(|&(a, l)| (a, l, false)).collect();
        for (j, &cap) in caps.iter().enumerate() {
            let at = RSP + 0x200 * j as u64;
            assert!(self.ram.write(at, &vec![0xEE; cap as usize]));
            bufs.push((at, cap, true));
        }
        self.ctl.add(&mut self.ram, &bufs);
        let served = self.kick();
        let mut bytes = Vec::new();
        for (j, &cap) in caps.iter().enumerate() {
            let mut b = vec![0u8; cap as usize];
            assert!(self.ram.read(RSP + 0x200 * j as u64, &mut b));
            bytes.extend(b);
        }
        Out {
            bytes,
            used: self.ctl.take_used(&self.ram),
            served,
        }
    }

    /// The request in one buffer per part (up to six), a response buffer per entry of `caps`.
    fn submit(&mut self, parts: &[&[u8]], caps: &[u32]) -> Out {
        let mut req = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            let at = REQ + 0x1000 * i as u64;
            assert!(self.ram.write(at, p));
            req.push((at, p.len() as u32));
        }
        self.submit_at(&req, caps)
    }

    fn cmd(&mut self, req: &[u8]) -> Out {
        self.submit(&[req], &[24])
    }

    fn code(&mut self, req: &[u8]) -> u32 {
        self.cmd(req).code()
    }

    fn ok(&mut self, req: &[u8]) {
        let o = self.cmd(req);
        assert_eq!(o.code(), OK, "command {:#x}", le32(req, 0));
    }

    /// A chain of raw buffers on the control queue; how many commands the device completed.
    fn chain(&mut self, bufs: &[(u64, u32, bool)]) -> (u32, u32) {
        self.ctl.add(&mut self.ram, bufs);
        self.kick()
    }

    /// Queues a command without a kick: request and response in the batch area, `slot` below six.
    fn queue(&mut self, req: &[u8], slot: u64) -> u16 {
        let (rq, rs) = (BREQ + 0x100 * slot, BRSP + 0x40 * slot);
        assert!(self.ram.write(rq, req));
        assert!(self.ram.write(rs, &[0xEE; 24]));
        self.ctl.add(
            &mut self.ram,
            &[(rq, req.len() as u32, false), (rs, 24, true)],
        )
    }

    fn queued_code(&self, slot: u64) -> u32 {
        let mut b = [0u8; 4];
        assert!(self.ram.read(BRSP + 0x40 * slot, &mut b));
        u32::from_le_bytes(b)
    }

    /// Writes `bytes` over the segments, in order.
    fn scatter(&mut self, segs: &[(u64, u32)], bytes: &[u8]) {
        let mut at = 0;
        for &(addr, len) in segs {
            let n = len as usize;
            assert!(self.ram.write(addr, &bytes[at..at + n]));
            at += n;
        }
        assert_eq!(at, bytes.len());
    }

    fn create_res(&mut self, id: u32, w: u32, h: u32) {
        self.ok(&create(id, 2, w, h));
    }

    /// Resource `id` of `w` x `h` pixels backed by the segments, which hold seeded data; returns the data.
    fn make_scattered(
        &mut self,
        id: u32,
        w: u32,
        h: u32,
        segs: &[(u64, u32)],
        seed: usize,
    ) -> Vec<u8> {
        self.create_res(id, w, h);
        let d = data(4 * (w * h) as usize, seed);
        self.scatter(segs, &d);
        self.ok(&attach(id, segs));
        d
    }

    /// The same with one contiguous backing at PAGES.
    fn make(&mut self, id: u32, w: u32, h: u32) -> Vec<u8> {
        self.make_scattered(id, w, h, &[(PAGES, 4 * w * h)], id as usize)
    }

    /// The driver resets the device and sets it up again on fresh queues.
    fn restart(&mut self) {
        assert!(self.ram.write(CTL, &vec![0u8; 0x6000]));
        self.ctl = Q::new(CTL, self.n);
        self.cur = Q::new(CUR, self.n);
        assert_eq!(
            bring_up(
                &mut self.m,
                5,
                GPU_BAR,
                &[&self.ctl, &self.cur],
                F_VERSION_1
            ),
            0xF
        );
    }
}

// ------------------------------------------------------------ identification

#[test]
fn the_function_identifies_as_a_modern_virtio_gpu_at_slot_5() {
    let mut m = machine();
    assert_eq!(cfg_read(&mut m, 5, 0, 4), 0x1050_1AF4);
    assert_eq!(
        cfg_read(&mut m, 5, 0x08, 4) >> 8,
        0x03_8000,
        "display controller"
    );
    assert_eq!(cfg_read(&mut m, 5, 0x08, 1), 1, "revision 1: modern only");
    assert_eq!(cfg_read(&mut m, 5, 0x2C, 4), 0x0010_1AF4);
    assert_eq!(cfg_read(&mut m, 5, 0x3C, 1), 5, "interrupt line: IRQ 5");
    assert_eq!(cfg_read(&mut m, 5, 0x3D, 1), 1, "INTA");
    assert_eq!(slot::GPU, 5);
    assert_ne!(cfg_read(&mut m, 5, 0x06, 2) & 0x10, 0, "capability list");
    let mut p = cfg_read(&mut m, 5, 0x34, 1);
    let mut seen = Vec::new();
    while p != 0 {
        assert_eq!(cfg_read(&mut m, 5, p, 1), 9, "vendor capability");
        let kind = cfg_read(&mut m, 5, p + 3, 1);
        let bar = cfg_read(&mut m, 5, p + 4, 1);
        let off = cfg_read(&mut m, 5, p + 8, 4);
        let len = cfg_read(&mut m, 5, p + 12, 4);
        let mult = (cfg_read(&mut m, 5, p + 2, 1) == 20).then(|| cfg_read(&mut m, 5, p + 16, 4));
        seen.push((kind, bar, off, len, mult));
        p = cfg_read(&mut m, 5, p + 1, 1);
    }
    assert_eq!(
        seen,
        [
            (1, 0, 0x0000, 0x38, None),
            (2, 0, 0x3000, 8, Some(4)),
            (3, 0, 0x1000, 4, None),
            (4, 0, 0x2000, 64, None),
        ],
        "two queues, so eight bytes of notification registers"
    );
    cfg_write(&mut m, 5, 0x10, 4, 0xFFFF_FFFF);
    assert_eq!(
        cfg_read(&mut m, 5, 0x10, 4),
        0xFFFF_C000,
        "16 KiB of memory"
    );
}

#[test]
fn only_version_1_is_offered_and_nothing_else_is_accepted() {
    let mut m = machine();
    cfg_write(&mut m, 5, 0x10, 4, GPU_BAR as u32);
    cfg_write(&mut m, 5, 0x04, 2, 2);
    assert_eq!(r(&mut m, GPU_BAR, 0x12, 2), 2, "controlq and cursorq");
    for (sel, want) in [(0, 0), (1, 1), (2, 0), (3, 0)] {
        w(&mut m, GPU_BAR, 0x00, 4, sel);
        assert_eq!(r(&mut m, GPU_BAR, 0x04, 4), want, "feature word {sel}");
    }
    assert_eq!(bring_up(&mut m, 5, GPU_BAR, &[], F_VERSION_1), 0xF);
    // virgl (0), EDID (1), resource UUID (2), blob (3) and context init (4) are not offered
    for bit in 0..5 {
        let s = handshake(&mut m, 5, GPU_BAR, &[], F_VERSION_1 | 1 << bit, true);
        assert_eq!(s & STATUS_FEATURES_OK, 0, "feature bit {bit} refused");
    }
}

#[test]
fn the_device_configuration_has_one_scanout_and_no_capability_sets() {
    let mut m = machine();
    cfg_write(&mut m, 5, 0x10, 4, GPU_BAR as u32);
    cfg_write(&mut m, 5, 0x04, 2, 2);
    for (i, want) in [0, 0, 1, 0].into_iter().enumerate() {
        assert_eq!(
            r(&mut m, GPU_BAR, 0x2000 + 4 * i as u64, 4),
            want,
            "word {i}"
        );
    }
    assert_eq!(r(&mut m, GPU_BAR, 0x2008, 1), 1);
    assert_eq!(r(&mut m, GPU_BAR, 0x2009, 1), 0);
    assert_eq!(r(&mut m, GPU_BAR, 0x2010, 4), 0, "the rest is zeros");
    assert_eq!(r(&mut m, GPU_BAR, 0x203C, 4), 0);
}

#[test]
fn the_limits_are_the_documented_ones() {
    assert_eq!(SCANOUTS, 1);
    assert_eq!((DEFAULT_WIDTH, DEFAULT_HEIGHT), (1024, 768));
    assert_eq!(MAX_RESOURCES, 8);
    assert_eq!(MAX_BACKING, 128);
    assert_eq!(MAX_DIMENSION, 8192);
    assert_eq!(CHUNK, 4096);
    assert_eq!(HEADER, 24);
    assert_eq!(FORMATS, [1, 2, 3, 4, 67, 68, 121, 134]);
}

// ------------------------------------------------------------ display info

#[test]
fn display_info_describes_one_enabled_scanout_of_1024_by_768() {
    let mut t = rig();
    let o = t.submit(&[&display_info()], &[408]);
    assert_eq!(o.code(), INFO);
    assert_eq!(o.written(), 408, "header and sixteen scanouts");
    assert_eq!(o.used, [(0, 408)]);
    for i in 0..16 {
        let at = HEADER + 24 * i;
        let mode: Vec<u32> = (0..6).map(|k| le32(&o.bytes, at + 4 * k)).collect();
        if i == 0 {
            assert_eq!(
                mode,
                [0, 0, 1024, 768, 1, 0],
                "x, y, width, height, enabled, flags"
            );
        } else {
            assert_eq!(mode, [0; 6], "scanout {i}");
        }
    }
    assert_eq!(t.m.gpu.commands, 1);
    assert_eq!(t.m.gpu.failed, 0);
}

#[test]
fn the_display_size_is_configurable_within_bounds() {
    let mut t = rig();
    assert!(t.m.gpu.set_display(1920, 1080));
    let o = t.submit(&[&display_info()], &[408]);
    assert_eq!(
        (le32(&o.bytes, 24 + 8), le32(&o.bytes, 24 + 12)),
        (1920, 1080)
    );
    for (width, height) in [
        (0, 600),
        (800, 0),
        (8193, 600),
        (800, 8193),
        (0, 0),
        (u32::MAX, 1),
    ] {
        assert!(!t.m.gpu.set_display(width, height), "{width}x{height}");
    }
    let o = t.submit(&[&display_info()], &[408]);
    assert_eq!(
        (le32(&o.bytes, 24 + 8), le32(&o.bytes, 24 + 12)),
        (1920, 1080),
        "refused sizes changed nothing"
    );
    assert!(t.m.gpu.set_display(1, 8192));
    assert!(t.m.gpu.set_display(8192, 1));
    let o = t.submit(&[&display_info()], &[408]);
    assert_eq!((le32(&o.bytes, 24 + 8), le32(&o.bytes, 24 + 12)), (8192, 1));
}

#[test]
fn the_display_info_response_needs_its_full_room() {
    let mut t = rig();
    let o = t.submit(&[&display_info()], &[407]);
    assert_eq!(o.code(), BAD_PARAM);
    assert_eq!(o.written(), 24, "an error is a bare header");
    assert!(
        o.bytes[24..].iter().all(|&b| b == 0xEE),
        "nothing past the header"
    );
    let o = t.submit(&[&display_info()], &[500]);
    assert_eq!(o.code(), INFO);
    assert_eq!(
        o.written(),
        408,
        "the used length is what was written, not the room"
    );
    assert!(o.bytes[408..].iter().all(|&b| b == 0xEE));
    // the room may be several buffers: 407 bytes in two is too little, 408 is enough
    assert_eq!(t.submit(&[&display_info()], &[200, 207]).code(), BAD_PARAM);
    let whole = t.submit(&[&display_info()], &[408]);
    let split = t.submit(&[&display_info()], &[100, 200, 108]);
    assert_eq!(split.code(), INFO);
    assert_eq!(split.bytes, whole.bytes);
    let split = t.submit(&[&display_info()], &[24, 384]);
    assert_eq!(split.bytes, whole.bytes);
}

// ------------------------------------------------------- RESOURCE_CREATE_2D

#[test]
fn every_listed_format_is_accepted_and_neighbours_are_not() {
    let mut t = rig();
    let mut refused = 0u64;
    for f in FORMATS {
        assert_eq!(t.code(&create(1, f, 3, 2)), OK, "format {f}");
        assert_eq!(t.host.take(), [Ev::Create(1, 3, 2, f)]);
        t.ok(&unref(1));
        t.clear();
        for other in [f - 1, f + 1] {
            if !FORMATS.contains(&other) {
                assert_eq!(t.code(&create(1, other, 3, 2)), BAD_PARAM, "format {other}");
                refused += 1;
            }
        }
    }
    for f in [0, 5, 66, 69, 133, 135, 0x1_0001, u32::MAX] {
        assert_eq!(t.code(&create(1, f, 3, 2)), BAD_PARAM, "format {f}");
        refused += 1;
    }
    assert!(
        t.host.take().is_empty(),
        "the host never heard of a refused resource"
    );
    assert_eq!(
        t.m.gpu.failed, refused,
        "every refusal is counted, nothing else"
    );
}

#[test]
fn resource_sizes_are_limited_on_both_sides() {
    let mut t = rig();
    assert_eq!(t.code(&create(1, 2, 8192, 1)), OK);
    assert_eq!(t.code(&create(2, 2, 1, 8192)), OK);
    assert_eq!(
        t.host.take(),
        [Ev::Create(1, 8192, 1, 2), Ev::Create(2, 1, 8192, 2)]
    );
    for (width, height) in [
        (8193, 1),
        (1, 8193),
        (0, 1),
        (1, 0),
        (0, 0),
        (u32::MAX, 1),
        (1, u32::MAX),
    ] {
        assert_eq!(
            t.code(&create(3, 2, width, height)),
            BAD_PARAM,
            "{width}x{height}"
        );
    }
    assert!(t.host.take().is_empty());
    assert_eq!(t.code(&create(3, 2, 1, 1)), OK, "a refused id stays free");
}

#[test]
fn resource_ids_are_nonzero_and_unique() {
    let mut t = rig();
    assert_eq!(t.code(&create(0, 2, 4, 4)), BAD_RES);
    assert_eq!(
        t.code(&create(0, 0xBAD, 4, 4)),
        BAD_RES,
        "the id is looked at first"
    );
    t.ok(&create(1, 2, 4, 4));
    t.clear();
    assert_eq!(t.code(&create(1, 2, 4, 4)), BAD_RES, "duplicate");
    assert_eq!(
        t.code(&create(1, 0xBAD, 4, 4)),
        BAD_RES,
        "the id is looked at before the format"
    );
    assert!(t.host.take().is_empty());
    // id 0 is not a resource for any other command either
    assert_eq!(t.code(&unref(0)), BAD_RES);
    assert_eq!(t.code(&detach(0)), BAD_RES);
    assert_eq!(t.code(&attach(0, &[(PAGES, 64)])), BAD_RES);
    assert_eq!(t.code(&flush(rc(0, 0, 0, 0), 0)), BAD_RES);
    assert_eq!(t.code(&transfer(rc(0, 0, 0, 0), 0, 0)), BAD_RES);
    assert_eq!(
        t.code(&scanout_cmd(rc(0, 0, 1, 1), 0, 0)),
        OK,
        "resource 0 in SET_SCANOUT turns it off"
    );
}

#[test]
fn the_pool_holds_eight_resources_and_frees_slots_again() {
    let mut t = rig();
    for id in 1..=MAX_RESOURCES as u32 {
        assert_eq!(t.code(&create(id, 2, 2, 2)), OK, "resource {id}");
    }
    t.clear();
    assert_eq!(t.code(&create(100, 2, 2, 2)), NOMEM);
    assert!(
        t.host.take().is_empty(),
        "the host is not asked when the pool is full"
    );
    t.ok(&unref(3));
    assert_eq!(t.code(&create(100, 2, 2, 2)), OK, "the freed slot");
    assert_eq!(t.code(&create(101, 2, 2, 2)), NOMEM);
    assert_eq!(t.host.take(), [Ev::Destroy(3), Ev::Create(100, 2, 2, 2)]);
}

#[test]
fn a_host_that_cannot_make_room_costs_no_slot() {
    let mut t = rig();
    t.host.refuse_create = true;
    assert_eq!(t.code(&create(1, 2, 2, 2)), NOMEM);
    assert_eq!(t.host.take(), [Ev::Create(1, 2, 2, 2)], "it was asked");
    t.host.refuse_create = false;
    for id in 1..=MAX_RESOURCES as u32 {
        assert_eq!(
            t.code(&create(id, 2, 2, 2)),
            OK,
            "resource {id}, the same id as the refused one included"
        );
    }
}

// ------------------------------------------------------------ RESOURCE_UNREF

#[test]
fn unref_tells_the_host_and_forgets_the_resource() {
    let mut t = rig();
    assert_eq!(t.code(&unref(9)), BAD_RES);
    t.make(1, 4, 4);
    t.clear();
    assert_eq!(t.code(&unref(1)), OK);
    assert_eq!(t.host.take(), [Ev::Destroy(1)]);
    assert_eq!(t.code(&unref(1)), BAD_RES, "gone");
    assert_eq!(t.code(&transfer(rc(0, 0, 1, 1), 0, 1)), BAD_RES);
    assert_eq!(t.code(&flush(rc(0, 0, 1, 1), 1)), BAD_RES);
    assert_eq!(t.code(&attach(1, &[(PAGES, 64)])), BAD_RES);
    assert_eq!(t.code(&scanout_cmd(rc(0, 0, 1, 1), 0, 1)), BAD_RES);
}

#[test]
fn a_new_resource_with_an_old_id_starts_without_backing() {
    let mut t = rig();
    t.make(1, 4, 4);
    t.ok(&unref(1));
    t.create_res(1, 4, 4);
    t.clear();
    assert_eq!(
        t.code(&transfer(rc(0, 0, 4, 4), 0, 1)),
        UNSPEC,
        "no backing"
    );
    t.ok(&attach(1, &[(PAGES, 64)]));
}

#[test]
fn unref_of_the_resource_on_display_turns_the_scanout_off_first() {
    let mut t = rig();
    t.make(1, 4, 4);
    t.make_scattered(2, 4, 4, &[(PAGES + 0x1000, 64)], 2);
    t.ok(&scanout_cmd(rc(0, 0, 4, 4), 0, 1));
    t.clear();
    t.ok(&unref(2));
    assert_eq!(
        t.host.take(),
        [Ev::Destroy(2)],
        "another resource: the scanout stays"
    );
    t.ok(&flush(rc(0, 0, 4, 4), 1));
    assert_eq!(t.host.take(), [Ev::Present(0, 1, rc(0, 0, 4, 4))]);
    t.ok(&unref(1));
    assert_eq!(t.host.take(), [Ev::Show(0, 0, OFF), Ev::Destroy(1)]);
    // the same id again is not on display
    t.create_res(1, 4, 4);
    t.clear();
    t.ok(&flush(rc(0, 0, 4, 4), 1));
    assert!(t.host.take().is_empty());
}

// ------------------------------------------------- ATTACH / DETACH_BACKING

#[test]
fn backing_in_any_buffer_layout_gives_the_same_pages_in_order() {
    // three entries that are not in address order, 12 + 4 + 16 bytes for a 4 x 2 resource
    let segs = [(PAGES + 0x500, 12), (PAGES + 0x100, 4), (PAGES + 0x900, 16)];
    let mut t = rig();
    let d = t.make_scattered(1, 4, 2, &segs, 1);
    t.clear();
    let full = rc(0, 0, 4, 2);
    for k in 0..=attach(1, &segs).len() {
        let req = attach(1, &segs);
        assert_eq!(
            t.submit(&[&req[..k], &req[k..]], &[24]).code(),
            UNSPEC,
            "already attached"
        );
        t.ok(&detach(1));
        assert_eq!(
            t.submit(&[&req[..k], &req[k..]], &[24]).code(),
            OK,
            "split at {k}"
        );
        t.ok(&transfer(full, 0, 1));
        assert_eq!(
            t.host.take(),
            expect_puts(1, full, 0, 4, &d),
            "split at {k}"
        );
    }
    // four small buffers, one of them empty, and the entries on their own
    let req = attach(1, &segs);
    t.ok(&detach(1));
    let (a, b, c) = (&req[..10], &req[10..40], &req[40..]);
    assert_eq!(t.submit(&[a, &[], b, c], &[24]).code(), OK);
    t.ok(&transfer(full, 0, 1));
    assert_eq!(t.host.take(), expect_puts(1, full, 0, 4, &d));
}

#[test]
fn zero_length_entries_are_skipped_in_the_stream() {
    let mut t = rig();
    let d = t.make_scattered(
        1,
        4,
        2,
        &[
            (PAGES + 0x700, 0),
            (PAGES + 0x100, 16),
            (PAGES + 0x200, 0),
            (PAGES + 0x300, 16),
        ],
        1,
    );
    t.clear();
    t.ok(&transfer(rc(0, 0, 4, 2), 0, 1));
    assert_eq!(t.host.take(), expect_puts(1, rc(0, 0, 4, 2), 0, 4, &d));
}

#[test]
fn attach_refuses_what_it_cannot_take() {
    let mut t = rig();
    assert_eq!(t.code(&attach(7, &[(PAGES, 64)])), BAD_RES);
    t.create_res(1, 4, 4);
    assert_eq!(t.code(&attach(1, &[])), BAD_PARAM, "no entries");
    assert_eq!(
        t.code(&attach_head(1, 129)),
        NOMEM,
        "more than the table holds, even before the entries are looked at"
    );
    assert_eq!(t.code(&attach_head(1, u32::MAX)), NOMEM);
    assert_eq!(t.code(&attach_head(1, 3)), BAD_PARAM, "entries missing");
    let mut short = attach(1, &[(PAGES, 32), (PAGES + 0x100, 32), (PAGES + 0x200, 32)]);
    short.pop();
    assert_eq!(
        t.code(&short),
        BAD_PARAM,
        "one byte of the last entry missing"
    );
    // exactly the most entries is fine, a hundred and twenty-nine are not
    let many: Vec<(u64, u32)> = (0..MAX_BACKING as u64)
        .map(|i| (PAGES + 16 * i, 0))
        .collect();
    assert_eq!(t.code(&attach(1, &many)), OK);
    t.ok(&detach(1));
    let mut too_many = many.clone();
    too_many.push((PAGES, 0));
    assert_eq!(t.code(&attach(1, &too_many)), NOMEM);
    let mut one_short = attach(1, &many);
    one_short.truncate(one_short.len() - 16);
    assert_eq!(
        t.code(&one_short),
        BAD_PARAM,
        "the last of 128 entries missing"
    );
    // bytes after the entries are of no interest
    let mut long = attach(1, &many);
    long.extend([0xAA; 40]);
    assert_eq!(t.code(&long), OK);
    assert!(t.host.take().iter().all(|e| matches!(e, Ev::Create(..))));
}

#[test]
fn a_second_attach_is_refused_and_leaves_the_first_backing_alone() {
    let mut t = rig();
    let d = t.make(1, 4, 2);
    assert_eq!(t.code(&attach(1, &[(PAGES + 0x1000, 32)])), UNSPEC);
    t.clear();
    t.ok(&transfer(rc(0, 0, 4, 2), 0, 1));
    assert_eq!(
        t.host.take(),
        expect_puts(1, rc(0, 0, 4, 2), 0, 4, &d),
        "still the first backing"
    );
}

#[test]
fn an_unreadable_entry_fails_the_attach_and_attaches_nothing() {
    let mut t = rig();
    t.create_res(1, 4, 2);
    // the header and the first entry are guest memory, the second entry runs off the end of RAM
    let req = attach(1, &[(PAGES, 16), (PAGES + 0x100, 16)]);
    assert!(t.ram.write(REQ, &req[..32]));
    assert!(t.ram.write(RAM as u64 - 24, &req[32..48]));
    let o = t.submit_at(&[(REQ, 32), (RAM as u64 - 24, 32)], &[24]);
    assert_eq!(o.code(), UNSPEC);
    // nothing stuck: an attach with readable entries is not "already attached"
    let d = data(32, 3);
    t.scatter(&[(PAGES, 16), (PAGES + 0x100, 16)], &d);
    t.ok(&req);
    t.clear();
    t.ok(&transfer(rc(0, 0, 4, 2), 0, 1));
    assert_eq!(t.host.take(), expect_puts(1, rc(0, 0, 4, 2), 0, 4, &d));
}

#[test]
fn detach_drops_the_backing_and_allows_another() {
    let mut t = rig();
    assert_eq!(t.code(&detach(9)), BAD_RES);
    t.make(1, 4, 2);
    assert_eq!(t.code(&detach(1)), OK);
    assert_eq!(
        t.code(&transfer(rc(0, 0, 4, 2), 0, 1)),
        UNSPEC,
        "no backing now"
    );
    assert_eq!(t.code(&detach(1)), OK, "detaching nothing is fine");
    // pages elsewhere, other data
    let d = data(32, 7);
    t.scatter(&[(PAGES + 0x2000, 32)], &d);
    t.ok(&attach(1, &[(PAGES + 0x2000, 32)]));
    t.clear();
    t.ok(&transfer(rc(0, 0, 4, 2), 0, 1));
    assert_eq!(t.host.take(), expect_puts(1, rc(0, 0, 4, 2), 0, 4, &d));
}

// ------------------------------------------------------------- SET_SCANOUT

#[test]
fn set_scanout_shows_a_window_of_the_resource() {
    let mut t = rig();
    t.make(1, 10, 6);
    t.clear();
    for window in [
        rc(0, 0, 10, 6),
        rc(2, 1, 5, 4),
        rc(9, 5, 1, 1),
        rc(0, 0, 1, 1),
    ] {
        assert_eq!(t.code(&scanout_cmd(window, 0, 1)), OK, "{window:?}");
        assert_eq!(t.host.take(), [Ev::Show(0, 1, window)]);
    }
}

#[test]
fn a_window_outside_the_resource_or_empty_is_refused_and_changes_nothing() {
    let mut t = rig();
    t.make(1, 10, 6);
    t.ok(&scanout_cmd(rc(2, 1, 5, 4), 0, 1));
    t.clear();
    for window in [
        rc(0, 0, 11, 6),
        rc(0, 0, 10, 7),
        rc(10, 0, 1, 1),
        rc(0, 6, 1, 1),
        rc(5, 0, 6, 1),
        rc(0, 3, 1, 4),
        rc(u32::MAX, 0, 2, 1),
        rc(0, u32::MAX, 1, 2),
        rc(1, 1, 0, 1),
        rc(1, 1, 1, 0),
        rc(0, 0, 0, 0),
    ] {
        assert_eq!(t.code(&scanout_cmd(window, 0, 1)), BAD_PARAM, "{window:?}");
    }
    assert!(t.host.take().is_empty());
    t.ok(&flush(rc(0, 0, 10, 6), 1));
    assert_eq!(
        t.host.take(),
        [Ev::Present(0, 1, rc(2, 1, 5, 4))],
        "the old window is still shown"
    );
}

#[test]
fn only_scanout_zero_exists_and_it_is_checked_first() {
    let mut t = rig();
    t.make(1, 4, 4);
    t.clear();
    for scanout in [1, 2, 15, 16, u32::MAX] {
        assert_eq!(
            t.code(&scanout_cmd(rc(0, 0, 4, 4), scanout, 1)),
            BAD_SCANOUT,
            "scanout {scanout}"
        );
        assert_eq!(
            t.code(&scanout_cmd(rc(0, 0, 4, 4), scanout, 99)),
            BAD_SCANOUT,
            "scanout {scanout}, no such resource"
        );
        assert_eq!(
            t.code(&scanout_cmd(rc(0, 0, 4, 4), scanout, 0)),
            BAD_SCANOUT,
            "scanout {scanout}, turned off"
        );
    }
    assert_eq!(t.code(&scanout_cmd(rc(0, 0, 4, 4), 0, 99)), BAD_RES);
    assert!(t.host.take().is_empty());
}

#[test]
fn resource_zero_turns_the_scanout_off_and_flushes_stop() {
    let mut t = rig();
    t.make(1, 4, 4);
    t.ok(&scanout_cmd(rc(0, 0, 4, 4), 0, 1));
    t.clear();
    // the rectangle of an off command means nothing
    assert_eq!(t.code(&scanout_cmd(rc(3, 3, 9, 9), 0, 0)), OK);
    assert_eq!(t.host.take(), [Ev::Show(0, 0, OFF)]);
    t.ok(&flush(rc(0, 0, 4, 4), 1));
    assert!(t.host.take().is_empty(), "nothing is on display");
    assert_eq!(t.code(&scanout_cmd(rc(0, 0, 4, 4), 0, 0)), OK, "off again");
    assert_eq!(t.host.take(), [Ev::Show(0, 0, OFF)], "told every time");
}

#[test]
fn another_resource_on_the_scanout_takes_over_the_flushes() {
    let mut t = rig();
    t.make(1, 4, 4);
    t.make_scattered(2, 8, 2, &[(PAGES + 0x1000, 64)], 2);
    t.ok(&scanout_cmd(rc(0, 0, 4, 4), 0, 1));
    t.ok(&scanout_cmd(rc(1, 0, 6, 2), 0, 2));
    t.clear();
    t.ok(&flush(rc(0, 0, 4, 4), 1));
    assert!(
        t.host.take().is_empty(),
        "resource 1 is no longer on display"
    );
    t.ok(&flush(rc(0, 0, 8, 2), 2));
    assert_eq!(t.host.take(), [Ev::Present(0, 2, rc(1, 0, 6, 2))]);
}

// ---------------------------------------------------------- RESOURCE_FLUSH

#[test]
fn a_flush_reaches_the_host_clipped_to_the_window_in_resource_coordinates() {
    let mut t = rig();
    t.make(1, 10, 6);
    t.ok(&scanout_cmd(rc(2, 1, 5, 4), 0, 1));
    t.clear();
    // window: x 2..7, y 1..5
    for (flush_rect, shown) in [
        (rc(0, 0, 10, 6), rc(2, 1, 5, 4)),
        (rc(3, 2, 2, 2), rc(3, 2, 2, 2)),
        (rc(0, 0, 4, 3), rc(2, 1, 2, 2)),
        (rc(6, 4, 4, 2), rc(6, 4, 1, 1)),
        (rc(2, 1, 5, 4), rc(2, 1, 5, 4)),
        (rc(4, 0, 1, 6), rc(4, 1, 1, 4)),
        (rc(0, 2, 10, 1), rc(2, 2, 5, 1)),
    ] {
        assert_eq!(t.code(&flush(flush_rect, 1)), OK);
        assert_eq!(
            t.host.take(),
            [Ev::Present(0, 1, shown)],
            "flush {flush_rect:?}"
        );
    }
}

#[test]
fn a_flush_that_misses_the_window_or_is_empty_is_not_forwarded() {
    let mut t = rig();
    t.make(1, 10, 6);
    t.ok(&scanout_cmd(rc(2, 1, 5, 4), 0, 1));
    t.clear();
    for flush_rect in [
        rc(7, 0, 3, 6),
        rc(0, 0, 2, 6),
        rc(0, 5, 10, 1),
        rc(0, 0, 10, 1),
        rc(3, 2, 0, 2),
        rc(3, 2, 2, 0),
        rc(10, 6, 0, 0),
    ] {
        assert_eq!(t.code(&flush(flush_rect, 1)), OK, "{flush_rect:?}");
    }
    assert!(t.host.take().is_empty());
}

#[test]
fn a_flush_must_lie_inside_its_resource() {
    let mut t = rig();
    t.make(1, 10, 6);
    t.ok(&scanout_cmd(rc(0, 0, 10, 6), 0, 1));
    t.clear();
    for flush_rect in [
        rc(0, 0, 11, 6),
        rc(0, 0, 10, 7),
        rc(11, 0, 0, 0),
        rc(0, 7, 0, 0),
        rc(u32::MAX, 0, 2, 1),
        rc(0, u32::MAX, 1, 2),
    ] {
        assert_eq!(t.code(&flush(flush_rect, 1)), BAD_PARAM, "{flush_rect:?}");
    }
    assert_eq!(t.code(&flush(rc(0, 0, 1, 1), 9)), BAD_RES);
    assert!(t.host.take().is_empty());
}

#[test]
fn a_flush_of_a_resource_that_is_not_on_display_is_acknowledged_and_dropped() {
    let mut t = rig();
    t.make(1, 4, 4);
    t.make_scattered(2, 4, 4, &[(PAGES + 0x1000, 64)], 2);
    t.ok(&scanout_cmd(rc(0, 0, 4, 4), 0, 1));
    t.clear();
    assert_eq!(t.code(&flush(rc(0, 0, 4, 4), 2)), OK);
    assert!(t.host.take().is_empty());
}

// --------------------------------------------------------- TRANSFER_TO_HOST_2D

#[test]
fn a_rectangle_arrives_row_by_row_at_its_place_and_nothing_else_changes() {
    let mut t = rig();
    let d = t.make(1, 10, 6);
    t.clear();
    // the usual offset: where the rectangle's first pixel is in the backing
    let rect = rc(3, 2, 4, 3);
    let o = t.cmd(&transfer(rect, 2 * 40 + 3 * 4, 1));
    assert_eq!(o.code(), OK);
    assert_eq!(o.written(), 24);
    let want = expect_puts(1, rect, 92, 10, &d);
    assert_eq!(want.len(), 3);
    assert_eq!(t.host.take(), want);
    assert_eq!(t.host.fb(1), &expect_fb(&[0; 240], 10, rect, 92, &d)[..]);
    // spot checks that do not share the helper's logic: pixel (3, 2) is the backing's pixel (3, 2)
    assert_eq!(&t.host.fb(1)[(2 * 10 + 3) * 4..][..4], &d[92..96]);
    assert_eq!(
        &t.host.fb(1)[(4 * 10 + 6) * 4..][..4],
        &d[(4 * 10 + 6) * 4..][..4]
    );
    assert_eq!(
        &t.host.fb(1)[(2 * 10 + 2) * 4..][..4],
        &[0; 4],
        "left of the rectangle"
    );
    assert_eq!(
        &t.host.fb(1)[(2 * 10 + 7) * 4..][..4],
        &[0; 4],
        "right of it"
    );
    assert_eq!(&t.host.fb(1)[(5 * 10 + 3) * 4..][..4], &[0; 4], "below it");
}

#[test]
fn the_offset_and_the_position_are_independent() {
    let mut t = rig();
    let d = t.make(1, 10, 6);
    t.clear();
    // the rows come from offset 13 on, one resource row apart, and land at (3, 2)
    let rect = rc(3, 2, 4, 3);
    t.ok(&transfer(rect, 13, 1));
    assert_eq!(t.host.take(), expect_puts(1, rect, 13, 10, &d));
    assert_eq!(&t.host.fb(1)[(2 * 10 + 3) * 4..][..16], &d[13..29]);
    assert_eq!(
        &t.host.fb(1)[(3 * 10 + 3) * 4..][..16],
        &d[53..69],
        "the next row is 40 bytes on"
    );
    assert_eq!(&t.host.fb(1)[(4 * 10 + 3) * 4..][..16], &d[93..109]);
}

#[test]
fn whole_rows_single_pixels_and_the_last_row_transfer_correctly() {
    let mut t = rig();
    let d = t.make(1, 10, 6);
    let mut fb = vec![0u8; 240];
    for (rect, offset) in [
        (rc(0, 0, 10, 6), 0),
        (rc(9, 5, 1, 1), 5 * 40 + 36),
        (rc(0, 5, 10, 1), 200),
        (rc(0, 0, 1, 6), 0),
        (rc(0, 0, 10, 1), 0),
    ] {
        t.clear();
        assert_eq!(t.code(&transfer(rect, offset as u64, 1)), OK, "{rect:?}");
        assert_eq!(
            t.host.take(),
            expect_puts(1, rect, offset, 10, &d),
            "{rect:?}"
        );
        fb = expect_fb(&fb, 10, rect, offset, &d);
        assert_eq!(t.host.fb(1), &fb[..], "{rect:?}");
    }
}

#[test]
fn rows_that_straddle_backing_entries_come_out_whole() {
    // 240 bytes in four pieces that are neither in address order nor row aligned
    let segs = [
        (PAGES + 0x3000, 37),
        (PAGES + 0x100, 100),
        (PAGES + 0x2000, 3),
        (PAGES + 0x1800, 100),
    ];
    let mut t = rig();
    let d = t.make_scattered(1, 10, 6, &segs, 5);
    t.clear();
    let rect = rc(1, 1, 8, 4);
    t.ok(&transfer(rect, 44, 1));
    assert_eq!(t.host.take(), expect_puts(1, rect, 44, 10, &d));
    assert_eq!(t.host.fb(1), &expect_fb(&[0; 240], 10, rect, 44, &d)[..]);
}

#[test]
fn a_wide_row_is_cut_into_runs_of_1024_pixels() {
    let segs = [
        (PAGES + 0x9000, 9999),
        (PAGES, 10001),
        (PAGES + 0x4000, 10000),
    ];
    let mut t = rig();
    let d = t.make_scattered(1, 2500, 3, &segs, 9);
    t.clear();
    let rect = rc(7, 1, 2300, 2);
    t.ok(&transfer(rect, 10_000 + 4 * 7, 1));
    let puts = t.host.take();
    let shape: Vec<(u32, u32, usize)> = puts
        .iter()
        .map(|e| match e {
            Ev::Put(1, x, y, row) => (*x, *y, row.len()),
            e => panic!("{e:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            (7, 1, 4096),
            (1031, 1, 4096),
            (2055, 1, 1008),
            (7, 2, 4096),
            (1031, 2, 4096),
            (2055, 2, 1008)
        ]
    );
    assert_eq!(puts, expect_puts(1, rect, 10_028, 2500, &d));
    assert_eq!(
        t.host.fb(1),
        &expect_fb(&vec![0; 30000], 2500, rect, 10_028, &d)[..]
    );
}

#[test]
fn a_row_of_exactly_1024_pixels_is_one_run_and_1025_are_two() {
    let mut t = rig();
    let d1 = t.make_scattered(1, 1024, 1, &[(PAGES, 4096)], 1);
    let d2 = t.make_scattered(2, 1025, 2, &[(PAGES + 0x4000, 8200)], 2);
    t.clear();
    t.ok(&transfer(rc(0, 0, 1024, 1), 0, 1));
    assert_eq!(t.host.take(), [Ev::Put(1, 0, 0, d1)]);
    t.ok(&transfer(rc(0, 0, 1025, 2), 0, 2));
    let puts = t.host.take();
    assert_eq!(puts, expect_puts(2, rc(0, 0, 1025, 2), 0, 1025, &d2));
    let sizes: Vec<usize> = puts
        .iter()
        .map(|e| match e {
            Ev::Put(_, _, _, row) => row.len(),
            e => panic!("{e:?}"),
        })
        .collect();
    assert_eq!(sizes, [4096, 4, 4096, 4]);
}

#[test]
fn a_transfer_changes_only_its_own_resource_and_presents_nothing() {
    let mut t = rig();
    t.make(1, 4, 4);
    let d2 = t.make_scattered(2, 4, 4, &[(PAGES + 0x1000, 64)], 2);
    t.ok(&scanout_cmd(rc(0, 0, 4, 4), 0, 2));
    t.clear();
    t.ok(&transfer(rc(0, 0, 4, 4), 0, 2));
    assert_eq!(t.host.fb(2), &d2[..]);
    assert_eq!(t.host.fb(1), &[0; 64][..]);
    assert!(
        t.host.take().iter().all(|e| matches!(e, Ev::Put(2, ..))),
        "no flush without a flush command"
    );
}

#[test]
fn a_transfer_is_checked_in_order_resource_backing_rectangle() {
    let mut t = rig();
    assert_eq!(t.code(&transfer(rc(0, 0, 1, 1), 0, 9)), BAD_RES);
    t.create_res(1, 10, 6);
    assert_eq!(
        t.code(&transfer(rc(0, 0, 1, 1), 0, 1)),
        UNSPEC,
        "no backing"
    );
    assert_eq!(
        t.code(&transfer(rc(0, 0, 99, 99), 0, 1)),
        UNSPEC,
        "the backing is looked at before the rectangle"
    );
    t.ok(&attach(1, &[(PAGES, 240)]));
    for rect in [
        rc(8, 0, 3, 1),
        rc(0, 5, 1, 2),
        rc(0, 0, 11, 1),
        rc(0, 0, 1, 7),
        rc(11, 0, 0, 1),
        rc(u32::MAX, 0, 2, 1),
        rc(0, u32::MAX, 1, 2),
    ] {
        assert_eq!(t.code(&transfer(rect, 0, 1)), BAD_PARAM, "{rect:?}");
    }
    assert!(
        t.host.take().iter().all(|e| matches!(e, Ev::Create(..))),
        "no pixels moved"
    );
}

#[test]
fn the_backing_must_cover_every_byte_the_rectangle_reads() {
    let mut t = rig();
    t.make(1, 10, 6);
    t.clear();
    // the last byte read is offset + 2 * 40 + 16 - 1 for a 4 x 3 rectangle
    assert_eq!(t.code(&transfer(rc(3, 2, 4, 3), 144, 1)), OK, "ends at 240");
    t.clear();
    assert_eq!(
        t.code(&transfer(rc(3, 2, 4, 3), 145, 1)),
        BAD_PARAM,
        "one byte beyond"
    );
    assert_eq!(t.code(&transfer(rc(0, 5, 10, 1), 200, 1)), OK);
    assert_eq!(t.code(&transfer(rc(0, 5, 10, 1), 201, 1)), BAD_PARAM);
    assert_eq!(
        t.code(&transfer(rc(0, 0, 1, 1), 239, 1)),
        BAD_PARAM,
        "a pixel starting in the last byte"
    );
    assert_eq!(t.code(&transfer(rc(0, 0, 1, 1), 236, 1)), OK);
    assert_eq!(t.code(&transfer(rc(0, 0, 1, 1), 240, 1)), BAD_PARAM);
    for offset in [u64::MAX, u64::MAX - 10, u64::MAX - 95, 1 << 63] {
        assert_eq!(
            t.code(&transfer(rc(3, 2, 4, 3), offset, 1)),
            BAD_PARAM,
            "offset {offset:#x}"
        );
    }
}

#[test]
fn the_rows_after_the_first_are_one_resource_row_apart_in_the_coverage_check() {
    let mut t = rig();
    t.create_res(1, 10, 6);
    t.ok(&attach(1, &[(PAGES, 100)])); // two and a half rows
    t.clear();
    assert_eq!(t.code(&transfer(rc(0, 0, 10, 1), 0, 1)), OK);
    assert_eq!(t.code(&transfer(rc(0, 0, 10, 2), 0, 1)), OK, "ends at 80");
    assert_eq!(
        t.code(&transfer(rc(0, 0, 10, 3), 0, 1)),
        BAD_PARAM,
        "would end at 120"
    );
    assert_eq!(
        t.code(&transfer(rc(0, 0, 1, 3), 16, 1)),
        OK,
        "ends at 16 + 80 + 4"
    );
    assert_eq!(
        t.code(&transfer(rc(0, 0, 1, 3), 17, 1)),
        BAD_PARAM,
        "ends at 101"
    );
}

#[test]
fn an_empty_rectangle_transfers_nothing_and_asks_nothing_of_the_backing() {
    let mut t = rig();
    t.make(1, 10, 6);
    t.clear();
    for (rect, offset) in [
        (rc(3, 2, 0, 3), 0),
        (rc(3, 2, 4, 0), 0),
        (rc(10, 6, 0, 0), u64::MAX),
        (rc(0, 0, 0, 0), 12345),
        (rc(10, 0, 0, 6), 0),
    ] {
        assert_eq!(t.code(&transfer(rect, offset, 1)), OK, "{rect:?}");
    }
    assert!(t.host.take().is_empty());
    assert_eq!(
        t.code(&transfer(rc(11, 0, 0, 0), 0, 1)),
        BAD_PARAM,
        "an empty one beyond the edge"
    );
    assert_eq!(t.code(&transfer(rc(0, 7, 4, 0), 0, 1)), BAD_PARAM);
}

#[test]
fn backing_the_guest_does_not_have_fails_the_transfer_and_not_the_device() {
    let mut t = rig();
    // one row of 40 bytes whose second half is beyond the end of RAM
    t.create_res(1, 10, 1);
    t.ok(&attach(1, &[(RAM as u64 - 20, 40)]));
    t.clear();
    assert_eq!(t.code(&transfer(rc(0, 0, 10, 1), 0, 1)), UNSPEC);
    assert!(t.host.take().is_empty());
    // two rows: the first is readable, the second is not
    t.create_res(2, 10, 2);
    let d = data(40, 4);
    t.scatter(&[(PAGES, 40)], &d);
    t.ok(&attach(2, &[(PAGES, 40), (RAM as u64 - 20, 40)]));
    t.clear();
    assert_eq!(t.code(&transfer(rc(0, 0, 10, 2), 0, 2)), UNSPEC);
    assert_eq!(
        t.host.take(),
        [Ev::Put(2, 0, 0, d)],
        "what was readable arrived, the rest did not"
    );
    assert!(!t.needs_reset(), "a driver's bad address is its own error");
}

#[test]
fn a_backing_address_that_overflows_is_an_error_not_a_panic() {
    let mut t = rig();
    t.create_res(1, 10, 1);
    t.ok(&attach(1, &[(u64::MAX - 1, 400)]));
    t.clear();
    assert_eq!(t.code(&transfer(rc(0, 0, 10, 1), 0, 1)), UNSPEC);
    assert_eq!(
        t.code(&transfer(rc(0, 0, 10, 1), 8, 1)),
        UNSPEC,
        "the address plus the offset into the entry wraps"
    );
    assert!(t.host.take().is_empty());
}

// ------------------------------------------------------------------- fences

#[test]
fn a_fenced_command_is_answered_with_the_flag_and_its_fence_id() {
    let mut t = rig();
    let sequence: Vec<(Vec<u8>, u32, u32)> = vec![
        (create(1, 2, 10, 6), 24, OK),
        (attach(1, &[(PAGES, 240)]), 24, OK),
        (scanout_cmd(rc(0, 0, 10, 6), 0, 1), 24, OK),
        (transfer(rc(0, 0, 10, 6), 0, 1), 24, OK),
        (flush(rc(0, 0, 10, 6), 1), 24, OK),
        (display_info(), 408, INFO),
        (detach(1), 24, OK),
        (unref(1), 24, OK),
        (unref(1), 24, BAD_RES),
        (hdr(0x99), 24, UNSPEC),
    ];
    for (i, (req, cap, code)) in sequence.into_iter().enumerate() {
        let id = 0x0102_0304_0506_0700 + i as u64;
        let o = t.submit(&[&fenced(req, id)], &[cap]);
        assert_eq!(o.code(), code, "command {i}");
        assert_eq!(o.flags(), 1, "command {i}");
        assert_eq!(o.fence(), id, "command {i}");
        assert_eq!(
            o.rest(),
            [0; 8],
            "the context and ring index are not echoed"
        );
    }
}

#[test]
fn a_command_without_the_flag_gets_a_plain_response() {
    let mut t = rig();
    let mut req = unref(1);
    req[8..16].copy_from_slice(&0xDEAD_BEEF_u64.to_le_bytes());
    let o = t.cmd(&req);
    assert_eq!(
        (o.code(), o.flags(), o.fence()),
        (BAD_RES, 0, 0),
        "a fence id alone means nothing"
    );
    // other flag bits (the ring index flag) are not copied, and do not stop the fence from being
    let mut req = unref(1);
    req[4..8].copy_from_slice(&2u32.to_le_bytes());
    req[8..16].copy_from_slice(&5u64.to_le_bytes());
    let o = t.cmd(&req);
    assert_eq!((o.flags(), o.fence()), (0, 0));
    req[4..8].copy_from_slice(&3u32.to_le_bytes());
    req[16..20].copy_from_slice(&7u32.to_le_bytes()); // context
    req[20] = 3; // ring index
    let o = t.cmd(&req);
    assert_eq!(
        (o.flags(), o.fence()),
        (1, 5),
        "only the fence flag is echoed"
    );
    assert_eq!(o.rest(), [0; 8]);
}

#[test]
fn a_command_too_short_for_its_fields_still_gets_its_fence_back() {
    let mut t = rig();
    let mut req = fenced(create(1, 2, 4, 4), 77);
    req.truncate(30);
    let o = t.cmd(&req);
    assert_eq!((o.code(), o.flags(), o.fence()), (BAD_PARAM, 1, 77));
}

// -------------------------------------------------------- malformed commands

#[test]
fn types_the_device_does_not_implement_get_unspec() {
    let mut t = rig();
    for kind in [
        0,
        0x99,
        0xFF,
        0x108,
        0x109,
        0x10A,
        0x10B,
        0x10C,
        0x10D,
        0x200,
        0x207,
        0x300,
        0x301,
        0x1100,
        0x1101,
        0x1200,
        0xFFFF_FFFF,
    ] {
        assert_eq!(t.code(&hdr(kind)), UNSPEC, "type {kind:#x}");
    }
    assert!(t.host.take().is_empty());
    assert_eq!((t.m.gpu.commands, t.m.gpu.failed), (17, 17));
}

#[test]
fn the_commands_need_their_fields_and_ignore_what_follows() {
    // (name, what the command needs in front of it, the fixed size of the command)
    const FIXED: [usize; 7] = [40, 32, 48, 48, 56, 32, 32];
    let prepared = |kind: usize, t: &mut Rig| -> Vec<u8> {
        match kind {
            0 => create(1, 2, 4, 4),
            1 => {
                t.make(1, 4, 4);
                unref(1)
            }
            2 => {
                t.make(1, 4, 4);
                scanout_cmd(rc(0, 0, 4, 4), 0, 1)
            }
            3 => {
                t.make(1, 4, 4);
                flush(rc(0, 0, 4, 4), 1)
            }
            4 => {
                t.make(1, 4, 4);
                transfer(rc(0, 0, 4, 4), 0, 1)
            }
            5 => {
                t.create_res(1, 4, 4);
                attach(1, &[(PAGES, 32), (PAGES + 0x100, 32)])
            }
            _ => {
                t.make(1, 4, 4);
                detach(1)
            }
        }
    };
    for (kind, fixed) in FIXED.into_iter().enumerate() {
        for extra in [0usize, 8, 40] {
            let mut t = rig();
            let full = prepared(kind, &mut t);
            assert!(full.len() >= fixed);
            t.clear();
            for cut in [full.len() - 1, fixed - 1, HEADER + 1, HEADER] {
                assert_eq!(
                    t.code(&full[..cut]),
                    BAD_PARAM,
                    "command {kind} cut to {cut}"
                );
            }
            assert!(
                t.host.take().is_empty(),
                "command {kind}: a short command changes nothing"
            );
            let mut longer = full.clone();
            longer.extend(vec![0xAA; extra]);
            assert_eq!(
                t.code(&longer),
                OK,
                "command {kind} with {extra} bytes more"
            );
        }
    }
}

#[test]
fn a_request_without_a_readable_header_is_unspec_and_never_fenced() {
    let mut t = rig();
    // one byte short of a header, carrying the fence flag
    let req = fenced(unref(1), 9);
    let o = t.cmd(&req[..HEADER - 1]);
    assert_eq!((o.code(), o.flags(), o.fence()), (UNSPEC, 0, 0));
    // no request buffer at all
    let o = t.submit_at(&[], &[24]);
    assert_eq!((o.code(), o.flags(), o.fence()), (UNSPEC, 0, 0));
    // a header that is not guest memory
    let o = t.submit_at(&[(RAM as u64, 24)], &[24]);
    assert_eq!((o.code(), o.flags(), o.fence()), (UNSPEC, 0, 0));
    let o = t.submit_at(&[(RAM as u64 - 10, 24)], &[24]);
    assert_eq!(o.code(), UNSPEC);
    // a header spread thinly, the way a driver can: it is still a header
    let o = t.submit(
        &[&fenced(unref(1), 9)[..5], &[], &fenced(unref(1), 9)[5..]],
        &[24],
    );
    assert_eq!((o.code(), o.fence()), (BAD_RES, 9));
    assert_eq!(t.m.gpu.commands, 5);
    assert_eq!(t.m.gpu.failed, 5);
    assert!(!t.needs_reset());
}

#[test]
fn a_command_may_be_split_over_two_buffers_at_every_byte() {
    let mut t = rig();
    let req = create(1, 2, 4, 4);
    for k in 0..=req.len() {
        let o = t.submit(&[&req[..k], &req[k..]], &[24]);
        assert_eq!(o.code(), OK, "split at {k}");
        t.ok(&unref(1));
    }
    t.make(2, 10, 6);
    t.clear();
    let d = data(240, 2);
    let rect = rc(3, 2, 4, 3);
    let req = transfer(rect, 92, 2);
    for k in 0..=req.len() {
        let o = t.submit(&[&req[..k], &req[k..]], &[24]);
        assert_eq!(o.code(), OK, "split at {k}");
        assert_eq!(
            t.host.take(),
            expect_puts(2, rect, 92, 10, &d),
            "split at {k}"
        );
    }
}

// --------------------------------------------------------- response buffers

#[test]
fn the_response_may_be_spread_over_buffers_and_the_used_length_is_what_was_written() {
    let mut t = rig();
    for caps in [
        vec![5, 19],
        vec![24, 100],
        vec![0, 24],
        vec![8, 8, 8],
        vec![1, 23, 0],
    ] {
        let o = t.submit(&[&unref(9)], &caps);
        assert_eq!(o.written(), 24, "{caps:?}");
        assert_eq!(le32(&o.bytes, 0), BAD_RES, "{caps:?}");
        assert!(
            o.bytes[4..HEADER].iter().all(|&b| b == 0),
            "{caps:?}: the rest of the header is zeros"
        );
        assert!(
            o.bytes[HEADER..].iter().all(|&b| b == 0xEE),
            "{caps:?}: nothing past it"
        );
    }
}

#[test]
fn a_chain_the_device_cannot_answer_needs_reset_and_stops_the_queue() {
    // no room for a header: 23 bytes, or no response buffer at all
    let mut t = rig();
    let o = t.submit(&[&unref(9)], &[23]);
    assert!(o.used.is_empty(), "not completed");
    assert_eq!(o.served, (0, 0));
    assert!(t.needs_reset());
    assert_eq!(t.m.gpu.commands, 0);
    let mut t = rig();
    assert!(t.ram.write(REQ, &unref(9)));
    assert_eq!(t.chain(&[(REQ, 32, false)]), (0, 0));
    assert!(t.needs_reset());
    // room in two buffers adds up: 12 + 12 is a header
    let mut t = rig();
    let o = t.submit(&[&unref(9)], &[12, 12]);
    assert_eq!(o.code(), BAD_RES);
    assert!(!t.needs_reset());
    // a response buffer first and a request buffer after it
    let mut t = rig();
    assert!(t.ram.write(REQ, &unref(9)));
    assert!(t.ram.write(RSP, &[0xEE; 24]));
    assert_eq!(
        t.chain(&[(REQ, 32, false), (RSP, 24, true), (REQ + 0x1000, 8, false)]),
        (0, 0)
    );
    assert!(t.needs_reset());
    // a response that is not guest memory, entirely or in part
    for bufs in [
        vec![(REQ, 32, false), (RAM as u64 - 10, 24, true)],
        vec![
            (REQ, 32, false),
            (RSP, 10, true),
            (RAM as u64 - 4, 14, true),
        ],
    ] {
        let mut t = rig();
        assert!(t.ram.write(REQ, &unref(9)));
        assert_eq!(t.chain(&bufs), (0, 0));
        assert!(t.needs_reset());
        assert!(t.ctl.take_used(&t.ram).is_empty());
        assert_eq!(
            t.m.gpu.commands, 0,
            "a command that was not answered is not counted"
        );
    }
    // the queue stays stopped: what was behind the broken chain is not served
    let mut t = rig();
    assert!(t.ram.write(REQ, &unref(9)));
    t.ctl.add(&mut t.ram, &[(REQ, 32, false), (RSP, 23, true)]);
    t.ctl
        .add(&mut t.ram, &[(REQ, 32, false), (RSP + 0x200, 24, true)]);
    t.cur.add(&mut t.ram, &[(REQ, 56, false)]);
    assert_eq!(t.kick(), (0, 0));
    assert!(t.ctl.take_used(&t.ram).is_empty());
    assert!(
        t.cur.take_used(&t.ram).is_empty(),
        "the cursor queue stops with the device"
    );
    assert_eq!(
        r(&mut t.m, GPU_BAR, 0x14, 1) & 0x40,
        0x40,
        "DEVICE_NEEDS_RESET"
    );
    assert!(!t.m.gpu.t.driver_ok());
}

#[test]
fn a_broken_ring_needs_reset_too() {
    // a descriptor that points to itself
    let mut t = rig();
    let mut d = [0u8; 16];
    d[8..12].copy_from_slice(&24u32.to_le_bytes());
    d[12..14].copy_from_slice(&1u16.to_le_bytes());
    assert!(t.ram.write(t.ctl.desc, &d));
    assert!(t.ram.write(t.ctl.avail + 2, &1u16.to_le_bytes()));
    assert_eq!(t.kick(), (0, 0));
    assert!(t.needs_reset());
    // and on the cursor queue
    let mut t = rig();
    assert!(t.ram.write(t.cur.desc, &d));
    assert!(t.ram.write(t.cur.avail + 2, &1u16.to_le_bytes()));
    assert_eq!(t.kick_cursor(), (0, 0));
    assert!(t.needs_reset());
    // a driver may have `size` chains outstanding and no more
    let mut t = rig_with(4);
    assert!(t.ram.write(REQ, &unref(9)));
    t.ctl.add(&mut t.ram, &[(REQ, 32, false), (RSP, 24, true)]);
    assert!(t.ram.write(t.ctl.avail + 2, &4u16.to_le_bytes())); // the other slots hold head 0 too
    assert_eq!(t.kick(), (4, 0), "four in a queue of four");
    assert_eq!(t.ctl.take_used(&t.ram), [(0, 24); 4]);
    assert!(!t.needs_reset());
    let mut t = rig_with(4);
    assert!(t.ram.write(REQ, &unref(9)));
    t.ctl.add(&mut t.ram, &[(REQ, 32, false), (RSP, 24, true)]);
    assert!(t.ram.write(t.ctl.avail + 2, &5u16.to_le_bytes()));
    assert_eq!(t.kick(), (0, 0), "five are too many");
    assert!(t.needs_reset());
}

// ------------------------------------------------------------------ queues

#[test]
fn several_commands_in_one_kick_complete_in_order_and_the_rings_wrap() {
    let mut t = rig_with(4);
    for round in 0..5 {
        let a = t.queue(&create(1, 2, 4, 4), 0);
        let b = t.queue(&unref(1), 1);
        assert_eq!(t.kick(), (2, 0), "round {round}");
        assert_eq!(
            t.ctl.take_used(&t.ram),
            [(u32::from(a), 24), (u32::from(b), 24)]
        );
        assert_eq!((t.queued_code(0), t.queued_code(1)), (OK, OK));
        assert_eq!(
            t.host.take(),
            [Ev::Create(1, 4, 4, 2), Ev::Destroy(1)],
            "round {round}"
        );
    }
    assert_eq!((t.m.gpu.commands, t.m.gpu.failed), (10, 0));
}

#[test]
fn nothing_is_served_without_bus_mastering_until_it_is_enabled() {
    let mut t = rig();
    cfg_write(&mut t.m, 5, 0x04, 2, 0x0002); // memory only
    let o = t.cmd(&create(1, 2, 4, 4));
    assert_eq!(o.served, (0, 0));
    assert!(o.used.is_empty());
    assert!(t.host.take().is_empty());
    cfg_write(&mut t.m, 5, 0x04, 2, 0x0006);
    assert_eq!(t.kick(), (1, 0), "the command was waiting");
    assert_eq!(t.host.take(), [Ev::Create(1, 4, 4, 2)]);
}

#[test]
fn the_queues_are_not_served_before_driver_ok() {
    let mut m = machine();
    let (ctl, cur) = (Q::new(CTL, 16), Q::new(CUR, 16));
    let s = handshake(&mut m, 5, GPU_BAR, &[&ctl, &cur], F_VERSION_1, false);
    assert_eq!(s, 0xB);
    let mut ram = Ram(vec![0; RAM]);
    let mut host = Host::default();
    let mut ctl = ctl;
    assert!(ram.write(REQ, &create(1, 2, 4, 4)));
    ctl.add(&mut ram, &[(REQ, 40, false), (RSP, 24, true)]);
    w(&mut m, GPU_BAR, 0x3000, 4, 0);
    assert_eq!(m.service_gpu(&mut ram, &mut host, 0), (0, 0));
    assert!(host.take().is_empty());
}

#[test]
fn service_consumes_the_notifications() {
    let mut t = rig();
    w(&mut t.m, GPU_BAR, 0x3000, 4, 0);
    w(&mut t.m, GPU_BAR, 0x3004, 4, 0);
    t.m.service_gpu(&mut t.ram, &mut t.host, 0);
    assert_eq!(t.m.gpu.t.take_kicks(), 0);
}

// ------------------------------------------------------------- the cursor queue

#[test]
fn cursor_commands_are_taken_completed_empty_and_ignored() {
    let mut t = rig();
    assert!(t.ram.write(REQ, &cursor_cmd(0x300)));
    let a = t.cur.add(&mut t.ram, &[(REQ, 56, false)]);
    assert_eq!(t.kick_cursor(), (0, 1));
    assert_eq!(
        t.cur.take_used(&t.ram),
        [(u32::from(a), 0)],
        "no response, so nothing written"
    );
    assert!(t.ctl.take_used(&t.ram).is_empty());
    assert!(t.host.take().is_empty(), "the host is not involved");
    assert_eq!((t.m.gpu.cursor_commands, t.m.gpu.commands), (1, 0));
    assert!(
        t.m.gpu.t.irq(),
        "the driver is interrupted when its buffer is returned"
    );
    // several in one kick, of both kinds and of no kind at all
    assert!(t.ram.write(REQ + 0x100, &cursor_cmd(0x301)));
    assert!(t.ram.write(REQ + 0x200, &hdr(0xFFFF)));
    let b = t.cur.add(&mut t.ram, &[(REQ + 0x100, 56, false)]);
    let c = t.cur.add(&mut t.ram, &[(REQ + 0x200, 24, false)]);
    let d = t.cur.add(&mut t.ram, &[(REQ + 0x100, 56, false)]);
    assert_eq!(
        t.kick(),
        (0, 3),
        "a kick of the control queue serves both queues"
    );
    assert_eq!(
        t.cur.take_used(&t.ram),
        [(u32::from(b), 0), (u32::from(c), 0), (u32::from(d), 0)]
    );
    assert_eq!(t.m.gpu.cursor_commands, 4);
    assert!(t.host.take().is_empty());
}

#[test]
fn a_cursor_chain_with_a_writable_buffer_is_completed_and_the_buffer_left_alone() {
    let mut t = rig();
    assert!(t.ram.write(REQ, &cursor_cmd(0x300)));
    assert!(t.ram.write(RSP, &[0xEE; 24]));
    t.cur.add(&mut t.ram, &[(REQ, 56, false), (RSP, 24, true)]);
    assert_eq!(t.kick_cursor(), (0, 1));
    assert_eq!(t.cur.take_used(&t.ram), [(0, 0)]);
    let mut b = [0u8; 24];
    assert!(t.ram.read(RSP, &mut b));
    assert_eq!(b, [0xEE; 24]);
}

#[test]
fn both_queues_are_served_in_one_call() {
    let mut t = rig();
    assert!(t.ram.write(REQ + 0x100, &cursor_cmd(0x300)));
    t.cur.add(&mut t.ram, &[(REQ + 0x100, 56, false)]);
    let o = t.submit(&[&create(1, 2, 4, 4)], &[24]);
    assert_eq!(o.served, (1, 1));
    assert_eq!(o.code(), OK);
    assert_eq!(t.cur.take_used(&t.ram), [(0, 0)]);
}

// ----------------------------------------------------------------- interrupts

#[test]
fn completing_a_command_raises_intx_that_reading_the_isr_clears() {
    let mut t = rig();
    assert!(!t.m.gpu.t.irq());
    assert_eq!(
        cfg_read(&mut t.m, 5, 0x06, 2) & 8,
        0,
        "status: no interrupt pending"
    );
    t.ok(&create(1, 2, 4, 4));
    assert!(t.m.gpu.t.irq());
    assert_eq!(
        cfg_read(&mut t.m, 5, 0x06, 2) & 8,
        8,
        "status: interrupt pending"
    );
    assert_eq!(
        r(&mut t.m, GPU_BAR, 0x1000, 1),
        1,
        "ISR bit 0: queue interrupt"
    );
    assert!(!t.m.gpu.t.irq());
    assert_eq!(cfg_read(&mut t.m, 5, 0x06, 2) & 8, 0);
    assert_eq!(r(&mut t.m, GPU_BAR, 0x1000, 1), 0, "reading cleared it");
    // an error response is a completion too
    assert_eq!(t.code(&unref(9)), BAD_RES);
    assert!(t.m.gpu.t.irq());
}

#[test]
fn the_driver_can_ask_not_to_be_interrupted_and_intx_can_be_disabled() {
    let mut t = rig();
    assert!(t.ram.write(t.ctl.avail, &1u16.to_le_bytes())); // VRING_AVAIL_F_NO_INTERRUPT
    t.ok(&create(1, 2, 4, 4));
    assert!(!t.m.gpu.t.irq(), "the control queue asked for silence");
    assert!(t.ram.write(REQ + 0x100, &cursor_cmd(0x300)));
    t.cur.add(&mut t.ram, &[(REQ + 0x100, 56, false)]);
    t.kick_cursor();
    assert!(t.m.gpu.t.irq(), "the cursor queue did not");
    assert_eq!(r(&mut t.m, GPU_BAR, 0x1000, 1), 1);
    assert!(t.ram.write(t.ctl.avail, &0u16.to_le_bytes()));
    cfg_write(&mut t.m, 5, 0x04, 2, 0x0406); // INTx disable
    t.ok(&unref(1));
    assert!(!t.m.gpu.t.irq(), "the command register masks the line");
    assert_eq!(
        r(&mut t.m, GPU_BAR, 0x1000, 1),
        1,
        "yet the ISR still says why"
    );
}

fn program_pic(m: &mut Machine) {
    for (cmd, data, icw3, base) in [(0x20u16, 0x21u16, 4u8, 0x20u8), (0xA0, 0xA1, 2, 0x28)] {
        m.io_out(cmd, 1, 0x11, 0);
        m.io_out(data, 1, u32::from(base), 0);
        m.io_out(data, 1, u32::from(icw3), 0);
        m.io_out(data, 1, 1, 0);
        m.io_out(data, 1, 0xFF, 0);
    }
    // IRQ 5 is level (ELCR); unmask it and the cascade
    m.io_out(0x4D0, 1, 0x20, 0);
    m.io_out(0x21, 1, 0xDB, 0);
}

#[test]
fn the_interrupt_reaches_the_8259_on_irq_5_as_soon_as_the_service_returns() {
    let mut t = rig();
    program_pic(&mut t.m);
    assert_eq!(t.m.pending(0), None);
    t.ok(&create(1, 2, 4, 4));
    assert!(t.m.pic.int_pending(), "no further call was needed");
    assert_eq!(t.m.pending(0), Some(0x25), "IRQ 5 on the master");
    assert_eq!(t.m.acknowledge(0), Some(0x25));
    assert_eq!(
        r(&mut t.m, GPU_BAR, 0x1000, 1),
        1,
        "the driver reads the ISR"
    );
    assert!(!t.m.gpu.t.irq());
    t.m.io_out(0x20, 1, 0x20, 0);
    assert_eq!(t.m.pending(0), None, "level dropped with the ISR read");
    assert_eq!(cfg_read(&mut t.m, 5, 0x06, 2) & 8, 0);
}

#[test]
fn irq_5_is_wired_or_with_another_function_moved_onto_it() {
    // the card sits on the display's line; the display's own interrupt must not be dropped by the quiet card
    let mut t = rig();
    program_pic(&mut t.m);
    cfg_write(&mut t.m, 4, 0x3C, 1, 5);
    t.ok(&create(1, 2, 4, 4));
    assert_eq!(t.m.pending(0), Some(0x25));
    assert_eq!(t.m.pending(0), Some(0x25), "still the display's interrupt");
    r(&mut t.m, GPU_BAR, 0x1000, 1);
    t.m.acknowledge(0);
    t.m.io_out(0x20, 1, 0x20, 0);
    assert_eq!(t.m.pending(0), None);
    // and the other way round: the card interrupts, the quiet display is the later one on the line
    t.m.net.t.interrupt();
    t.m.sync(0);
    assert_eq!(
        t.m.pending(0),
        Some(0x25),
        "the card's interrupt on the shared line"
    );
}

#[test]
fn intx_reaches_pin_17_of_the_io_apic_active_low_and_level() {
    let mut t = rig();
    let ioapic_base = ioapic::DEFAULT_BASE;
    let lapic_base = lapic::DEFAULT_BASE;
    t.m.mmio_write(lapic_base + u64::from(reg::SVR), 4, 0x1FF, 0);
    t.m.mmio_write(lapic_base + u64::from(reg::LVT_LINT0), 4, 1 << 16, 0);
    let route = |m: &mut Machine, pin: u8, low: u32| {
        m.mmio_write(ioapic_base, 4, u64::from(0x10 + 2 * pin + 1), 0);
        m.mmio_write(ioapic_base + 0x10, 4, 0, 0);
        m.mmio_write(ioapic_base, 4, u64::from(0x10 + 2 * pin), 0);
        m.mmio_write(ioapic_base + 0x10, 4, u64::from(low), 0);
    };
    let level_low = (1 << 15) | (1 << 13);
    assert_eq!(pci_pin(slot::GPU), 17);
    route(&mut t.m, 16, level_low | 0x52);
    route(&mut t.m, 17, level_low | 0x53);
    route(&mut t.m, 19, level_low | 0x51);
    assert_eq!(t.m.pending(0), None, "idle: the active-low lines are high");
    t.ok(&create(1, 2, 4, 4));
    assert_eq!(t.m.pending(0), Some(0x53), "the display is on pin 17");
    assert_eq!(t.m.acknowledge(0), Some(0x53));
    t.m.mmio_write(lapic_base + u64::from(reg::EOI), 4, 0, 0);
    assert_eq!(
        t.m.pending(0),
        Some(0x53),
        "level: fires again until the device is quiet"
    );
    assert_eq!(t.m.acknowledge(0), Some(0x53));
    r(&mut t.m, GPU_BAR, 0x1000, 1);
    t.m.mmio_write(lapic_base + u64::from(reg::EOI), 4, 0, 0);
    assert_eq!(t.m.pending(0), None);
}

// ------------------------------------------------------------------------ bus

#[test]
fn the_three_bars_are_separate_windows_onto_separate_functions() {
    let mut t = rig();
    for (dev, bar) in [(3, BLK_BAR), (4, NET_BAR)] {
        cfg_write(&mut t.m, dev, 0x10, 4, bar as u32);
        cfg_write(&mut t.m, dev, 0x04, 2, 0x0006);
    }
    assert_eq!(r(&mut t.m, GPU_BAR, 0x2008, 4), 1, "num_scanouts");
    assert_eq!(r(&mut t.m, BLK_BAR, 0x2008, 4), 0);
    assert_eq!(r(&mut t.m, NET_BAR, 0x2008, 4), 0);
    assert_eq!(r(&mut t.m, BLK_BAR, 0x12, 2), 1, "the disk has one queue");
    assert_eq!(r(&mut t.m, GPU_BAR, 0x12, 2), 2);
    // a reset of one is not a reset of the others
    w(&mut t.m, BLK_BAR, 0x14, 1, 1);
    w(&mut t.m, NET_BAR, 0x14, 1, 1);
    assert_eq!(r(&mut t.m, GPU_BAR, 0x14, 1), 0xF);
    w(&mut t.m, GPU_BAR, 0x14, 1, 0);
    assert_eq!(r(&mut t.m, BLK_BAR, 0x14, 1), 1);
    assert_eq!(r(&mut t.m, NET_BAR, 0x14, 1), 1);
    assert_eq!(r(&mut t.m, GPU_BAR, 0x14, 1), 0);
    // notifications go to their own function
    w(&mut t.m, BLK_BAR, 0x3000, 4, 0);
    w(&mut t.m, NET_BAR, 0x3004, 4, 0);
    assert_eq!(t.m.gpu.t.take_kicks(), 0);
    w(&mut t.m, GPU_BAR, 0x3004, 4, 0);
    assert_eq!((t.m.blk.t.take_kicks(), t.m.net.t.take_kicks()), (1, 2));
    assert_eq!(t.m.gpu.t.take_kicks(), 2, "queue 1 of the display");
    assert_eq!(t.m.unclaimed_mmio, 0);
}

// ---------------------------------------------------------------------- reset

#[test]
fn a_reset_drops_every_resource_and_blanks_the_scanout() {
    let mut t = rig();
    t.make(3, 4, 4);
    t.make_scattered(1, 4, 4, &[(PAGES + 0x1000, 64)], 1);
    t.make_scattered(2, 4, 4, &[(PAGES + 0x2000, 64)], 2);
    t.ok(&scanout_cmd(rc(0, 0, 4, 4), 0, 1));
    t.clear();
    w(&mut t.m, GPU_BAR, 0x14, 1, 0); // the driver resets the device
                                      // even without bus mastering the device notices when it is next called
    cfg_write(&mut t.m, 5, 0x04, 2, 0x0002);
    assert_eq!(t.m.service_gpu(&mut t.ram, &mut t.host, 0), (0, 0));
    assert_eq!(
        t.host.take(),
        [
            Ev::Show(0, 0, OFF),
            Ev::Destroy(3),
            Ev::Destroy(1),
            Ev::Destroy(2)
        ],
        "the scanout first, then the resources in the order of the pool"
    );
    // a second look finds nothing left to drop
    assert_eq!(t.m.service_gpu(&mut t.ram, &mut t.host, 0), (0, 0));
    assert!(t.host.take().is_empty());
    // the new driver finds an empty device with every slot and every id free
    t.restart();
    assert_eq!(t.kick(), (0, 0));
    assert!(t.host.take().is_empty(), "nothing to drop twice");
    for id in 1..=MAX_RESOURCES as u32 {
        assert_eq!(t.code(&create(id, 2, 2, 2)), OK, "resource {id}");
    }
    t.clear();
    assert_eq!(
        t.code(&flush(rc(0, 0, 1, 1), 1)),
        OK,
        "and nothing is on display"
    );
    assert!(t.host.take().is_empty());
    // resets are noticed every time
    t.restart();
    t.kick();
    let want: Vec<Ev> = (1..=MAX_RESOURCES as u32).map(Ev::Destroy).collect();
    assert_eq!(t.host.take(), want);
}

#[test]
fn a_reset_of_an_idle_device_says_nothing_to_the_host() {
    let mut t = rig();
    t.restart();
    assert_eq!(t.kick(), (0, 0));
    t.restart();
    assert_eq!(t.kick(), (0, 0));
    assert!(t.host.take().is_empty());
    // without a scanout only the resources are dropped
    t.create_res(1, 2, 2);
    t.clear();
    t.restart();
    t.kick();
    assert_eq!(t.host.take(), [Ev::Destroy(1)]);
}
