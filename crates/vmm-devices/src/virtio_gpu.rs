//! virtio-gpu (device type 16), 2D only: a display behind a [`Scanout`].
//! Queue 0 carries the control commands, queue 1 the cursor commands; the
//! only feature offered is VERSION_1 (no 3D, no EDID, no resource UUIDs, no
//! blobs, so no capability sets either) and the device configuration is
//! `events_read`, `events_clear`, `num_scanouts` (1) and `num_capsets` (0).
//!
//! A control request is a chain of device-readable buffers (a 24-byte header:
//! type, flags, fence id, context id, ring index, then the command's fields)
//! followed by device-writable ones that take the response. The buffers are
//! one byte stream: a command may be split anywhere, and so may the
//! response. Commands: GET_DISPLAY_INFO, RESOURCE_CREATE_2D, RESOURCE_UNREF,
//! SET_SCANOUT, RESOURCE_FLUSH, TRANSFER_TO_HOST_2D, RESOURCE_ATTACH_BACKING
//! and RESOURCE_DETACH_BACKING; any other type is answered ERR_UNSPEC. A
//! fenced command (flags bit 0) is answered with the flag and its fence id,
//! also when it fails. The cursor queue is drained and its commands ignored:
//! they have no response, and the host draws its own pointer.
//!
//! The device keeps no pixels (there is no allocation here). A resource is a
//! slot of the pool with its size and the guest pages that back it; its pixels
//! live in the host, which is told about every resource by [`Scanout::create`]
//! and receives the pixels a transfer copies, one run of at most [`CHUNK`]
//! bytes of one row at a time ([`Scanout::put`]). The bytes are as the guest
//! wrote them, in the resource's format: nothing is converted. What the
//! scanout shows and what a flush changes reach the host as
//! [`Scanout::show`] and [`Scanout::present`].
//!
//! Everything the guest sends is checked: a rectangle must lie inside its
//! resource, the backing must cover what a transfer reads, every sum is a
//! checked one. A command that is wrong gets an error response and changes
//! nothing. A chain the device cannot answer at all (no room for a response
//! header, device-readable buffers after device-writable ones, a response
//! buffer that is not guest memory) sets DEVICE_NEEDS_RESET. A reset of the
//! device drops every resource and blanks the scanout; the device notices it
//! in [`VirtioGpu::service`].

use core::ops::Range;

use crate::virtio::{Chain, Desc, GuestMemory, VirtioPci};

pub const DEVICE_TYPE: u16 = 16;
/// Display controller, other (not VGA-compatible).
pub const CLASS: u32 = 0x03_8000;
/// The scanouts the device has (`num_scanouts`); GET_DISPLAY_INFO describes sixteen.
pub const SCANOUTS: u32 = 1;
pub const DEFAULT_WIDTH: u32 = 1024;
pub const DEFAULT_HEIGHT: u32 = 768;
/// Resources that exist at once.
pub const MAX_RESOURCES: usize = 8;
/// Backing entries one resource may have (the Linux driver sends one per run of contiguous pages).
pub const MAX_BACKING: usize = 128;
/// The largest width or height of a resource (and of the display).
pub const MAX_DIMENSION: u32 = 8192;
/// The most bytes of one row a single [`Scanout::put`] gets.
pub const CHUNK: usize = 4096;
/// Bytes of the header every command and response starts with.
pub const HEADER: usize = 24;
/// The pixel formats (B8G8R8A8, B8G8R8X8, A8R8G8B8, X8R8G8B8, R8G8B8A8, X8B8G8R8, A8B8G8R8, R8G8B8X8): all four bytes per pixel.
pub const FORMATS: [u32; 8] = [1, 2, 3, 4, 67, 68, 121, 134];

const CONTROL: usize = 0;
const CURSOR: usize = 1;
const FLAG_FENCE: u32 = 1;
/// Command sizes: the header and the fields of each command.
const LEN_CREATE: usize = 40;
const LEN_RESOURCE_ID: usize = 32;
const LEN_SCANOUT: usize = 48;
const LEN_FLUSH: usize = 48;
const LEN_TRANSFER: usize = 56;
const LEN_ATTACH: usize = 32;
/// The largest fixed-size command: all the device reads of a request up front.
const CMD_MAX: usize = LEN_TRANSFER;
const ENTRY_LEN: usize = 16;
/// The display-info response: the header and sixteen scanouts of 24 bytes.
const INFO_LEN: usize = HEADER + 16 * 24;

/// Command types.
pub mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x100;
    pub const RESOURCE_CREATE_2D: u32 = 0x101;
    pub const RESOURCE_UNREF: u32 = 0x102;
    pub const SET_SCANOUT: u32 = 0x103;
    pub const RESOURCE_FLUSH: u32 = 0x104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x107;
}

/// Response types.
pub mod resp {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;
}

/// A rectangle of pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// The host side of the display: where the pixels of the resources are kept and shown.
/// Resource ids are the guest's; a resource is `width` x `height` pixels of four bytes.
pub trait Scanout {
    /// A resource was created; the host makes room for its pixels. False if it cannot (the guest is told ERR_OUT_OF_MEMORY).
    fn create(&mut self, resource: u32, width: u32, height: u32, format: u32) -> bool;
    /// The resource is gone.
    fn destroy(&mut self, resource: u32);
    /// `row` (a whole number of pixels within one row) are the new pixels of the resource from (`x`, `y`) on.
    fn put(&mut self, resource: u32, x: u32, y: u32, row: &[u8]);
    /// `scanout` now shows `rect` of `resource`; resource 0 turns it off.
    fn show(&mut self, scanout: u32, resource: u32, rect: Rect);
    /// `rect` (in the resource's coordinates, inside what `scanout` shows) of the resource on display changed.
    fn present(&mut self, scanout: u32, resource: u32, rect: Rect);
}

#[derive(Clone, Debug)]
struct Resource {
    /// 0: the slot is free.
    id: u32,
    width: u32,
    height: u32,
    /// Entries of `backing` in use; 0: no backing attached.
    entries: usize,
    /// The sum of the entries' lengths.
    backing_len: u64,
    backing: [Desc; MAX_BACKING],
}

impl Resource {
    const FREE: Self = Self {
        id: 0,
        width: 0,
        height: 0,
        entries: 0,
        backing_len: 0,
        backing: [Desc {
            addr: 0,
            len: 0,
            write: false,
        }; MAX_BACKING],
    };

    fn new(id: u32, width: u32, height: u32) -> Self {
        Self {
            id,
            width,
            height,
            ..Self::FREE
        }
    }
}

#[derive(Clone, Debug)]
pub struct VirtioGpu {
    pub t: VirtioPci,
    width: u32,
    height: u32,
    res: [Resource; MAX_RESOURCES],
    /// The resource scanout 0 shows (0: none) and the part of it that is shown.
    shown: u32,
    window: Rect,
    /// The transport's reset count at the last look.
    seen_resets: u32,
    /// Control commands answered, and those of them answered with an error.
    pub commands: u64,
    pub failed: u64,
    /// Cursor commands taken and ignored.
    pub cursor_commands: u64,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

fn rect_at(b: &[u8], at: usize) -> Rect {
    Rect {
        x: le32(b, at),
        y: le32(b, at + 4),
        w: le32(b, at + 8),
        h: le32(b, at + 12),
    }
}

/// Does `r` lie inside a `width` x `height` area (an empty one on its edge does)?
fn inside(r: Rect, width: u32, height: u32) -> bool {
    r.x.checked_add(r.w).is_some_and(|e| e <= width)
        && r.y.checked_add(r.h).is_some_and(|e| e <= height)
}

/// The part two rectangles share, if it has any pixels. Both are within a resource, so no sum overflows.
fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    (x0 < x1 && y0 < y1).then(|| Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

fn total(segs: &[Desc]) -> u64 {
    segs.iter().map(|s| u64::from(s.len)).sum()
}

/// Visits the pieces of the byte stream `segs` (the buffers back to back) that are stream bytes
/// `at..at + len`: `f(guest address, the part of the caller's buffer it is)`. False if the stream
/// ends first, an address overflows or `f` says no.
fn walk(segs: &[Desc], at: u64, len: usize, mut f: impl FnMut(u64, Range<usize>) -> bool) -> bool {
    let mut skip = at;
    let mut done = 0;
    for s in segs {
        if done == len {
            break;
        }
        let seg = u64::from(s.len);
        if skip >= seg {
            skip -= seg;
            continue;
        }
        let n = (seg - skip).min((len - done) as u64) as usize;
        let Some(gpa) = s.addr.checked_add(skip) else {
            return false;
        };
        if !f(gpa, done..done + n) {
            return false;
        }
        done += n;
        skip = 0;
    }
    done == len
}

fn read_stream(mem: &dyn GuestMemory, segs: &[Desc], at: u64, buf: &mut [u8]) -> bool {
    let len = buf.len();
    walk(segs, at, len, |gpa, r| mem.read(gpa, &mut buf[r]))
}

fn write_stream(mem: &mut dyn GuestMemory, segs: &[Desc], data: &[u8]) -> bool {
    walk(segs, 0, data.len(), |gpa, r| mem.write(gpa, &data[r]))
}

impl VirtioGpu {
    pub fn new(line: u8) -> Self {
        let mut t = VirtioPci::new(DEVICE_TYPE, CLASS, 2, 0, line);
        t.set_device_config(8, &SCANOUTS.to_le_bytes());
        Self {
            t,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            res: [Resource::FREE; MAX_RESOURCES],
            shown: 0,
            window: Rect::default(),
            seen_resets: 0,
            commands: 0,
            failed: 0,
            cursor_commands: 0,
        }
    }

    /// Sets the size GET_DISPLAY_INFO reports for the scanout; false (nothing changed) if either side is 0 or above [`MAX_DIMENSION`].
    /// The guest is not told of a change (there are no display events): it sees it when it asks again.
    pub fn set_display(&mut self, width: u32, height: u32) -> bool {
        let ok = |v: u32| (1..=MAX_DIMENSION).contains(&v);
        if !ok(width) || !ok(height) {
            return false;
        }
        self.width = width;
        self.height = height;
        true
    }

    /// Serves what the driver made available (the guest's memory is `mem`, the display is `scan`);
    /// returns how many control and how many cursor commands it completed.
    pub fn service(&mut self, mem: &mut dyn GuestMemory, scan: &mut dyn Scanout) -> (u32, u32) {
        self.t.take_kicks();
        if self.seen_resets != self.t.resets {
            self.seen_resets = self.t.resets;
            self.forget(scan);
        }
        if !self.t.dma_allowed() {
            return (0, 0);
        }
        (self.serve_control(mem, scan), self.serve_cursor(mem))
    }

    /// The driver reset the device: its resources are gone and the scanout is dark.
    fn forget(&mut self, scan: &mut dyn Scanout) {
        if self.shown != 0 {
            self.hide(scan);
        }
        for r in self.res.iter_mut().filter(|r| r.id != 0) {
            scan.destroy(r.id);
            r.id = 0;
        }
    }

    fn hide(&mut self, scan: &mut dyn Scanout) {
        self.shown = 0;
        scan.show(0, 0, Rect::default());
    }

    fn serve_control(&mut self, mem: &mut dyn GuestMemory, scan: &mut dyn Scanout) -> u32 {
        let mut done = 0;
        while let Some(chain) = self.t.pop(mem, CONTROL) {
            match self.command(mem, scan, &chain) {
                Some(written) => {
                    self.t.push_used(mem, CONTROL, chain.head, written);
                    done += 1;
                }
                // Nothing can be answered: the driver is broken, and the queue stops until it resets the device.
                None => self.t.needs_reset(),
            }
        }
        done
    }

    fn serve_cursor(&mut self, mem: &mut dyn GuestMemory) -> u32 {
        let mut done = 0;
        while let Some(chain) = self.t.pop(mem, CURSOR) {
            self.cursor_commands += 1;
            self.t.push_used(mem, CURSOR, chain.head, 0);
            done += 1;
        }
        done
    }

    /// Executes one control request and writes its response; returns the response's length,
    /// or `None` if there was nowhere to answer.
    fn command(
        &mut self,
        mem: &mut dyn GuestMemory,
        scan: &mut dyn Scanout,
        chain: &Chain,
    ) -> Option<u32> {
        let (rd, wr) = chain
            .descs()
            .split_at(chain.descs().iter().take_while(|d| !d.write).count());
        let room = total(wr);
        if room < HEADER as u64 || wr.iter().any(|d| !d.write) {
            return None;
        }
        let mut c = [0u8; CMD_MAX];
        let have = total(rd).min(CMD_MAX as u64) as usize;
        let parsed = have >= HEADER && read_stream(mem, rd, 0, &mut c[..have]);
        let cmd = &c[..have];
        let mut reply = [0u8; INFO_LEN];
        let mut len = HEADER;
        let code = if !parsed {
            resp::ERR_UNSPEC
        } else {
            match le32(cmd, 0) {
                cmd::GET_DISPLAY_INFO if room < INFO_LEN as u64 => resp::ERR_INVALID_PARAMETER,
                cmd::GET_DISPLAY_INFO => {
                    self.display_info(&mut reply[HEADER..]);
                    len = INFO_LEN;
                    resp::OK_DISPLAY_INFO
                }
                cmd::RESOURCE_CREATE_2D => self.create_2d(scan, cmd),
                cmd::RESOURCE_UNREF => self.unref(scan, cmd),
                cmd::SET_SCANOUT => self.set_scanout(scan, cmd),
                cmd::RESOURCE_FLUSH => self.flush(scan, cmd),
                cmd::TRANSFER_TO_HOST_2D => self.transfer(mem, scan, cmd),
                cmd::RESOURCE_ATTACH_BACKING => self.attach_backing(mem, rd, cmd),
                cmd::RESOURCE_DETACH_BACKING => self.detach_backing(cmd),
                _ => resp::ERR_UNSPEC,
            }
        };
        reply[..4].copy_from_slice(&code.to_le_bytes());
        if parsed && le32(cmd, 4) & FLAG_FENCE != 0 {
            reply[4..8].copy_from_slice(&FLAG_FENCE.to_le_bytes());
            reply[8..16].copy_from_slice(&le64(cmd, 8).to_le_bytes());
        }
        if !write_stream(mem, wr, &reply[..len]) {
            return None;
        }
        self.commands += 1;
        if code >= resp::ERR_UNSPEC {
            self.failed += 1;
        }
        Some(len as u32)
    }

    fn find(&mut self, id: u32) -> Option<&mut Resource> {
        if id == 0 {
            return None;
        }
        self.res.iter_mut().find(|r| r.id == id)
    }

    /// The sixteen scanouts' modes (`out` is zero): the first one is on and has the display's size.
    fn display_info(&self, out: &mut [u8]) {
        out[8..12].copy_from_slice(&self.width.to_le_bytes());
        out[12..16].copy_from_slice(&self.height.to_le_bytes());
        out[16..20].copy_from_slice(&1u32.to_le_bytes());
    }

    fn create_2d(&mut self, scan: &mut dyn Scanout, c: &[u8]) -> u32 {
        if c.len() < LEN_CREATE {
            return resp::ERR_INVALID_PARAMETER;
        }
        let (id, format, w, h) = (le32(c, 24), le32(c, 28), le32(c, 32), le32(c, 36));
        if id == 0 || self.find(id).is_some() {
            return resp::ERR_INVALID_RESOURCE_ID;
        }
        let side = |v: u32| (1..=MAX_DIMENSION).contains(&v);
        if !FORMATS.contains(&format) || !side(w) || !side(h) {
            return resp::ERR_INVALID_PARAMETER;
        }
        let Some(slot) = self.res.iter_mut().find(|r| r.id == 0) else {
            return resp::ERR_OUT_OF_MEMORY;
        };
        if !scan.create(id, w, h, format) {
            return resp::ERR_OUT_OF_MEMORY;
        }
        *slot = Resource::new(id, w, h);
        resp::OK_NODATA
    }

    fn unref(&mut self, scan: &mut dyn Scanout, c: &[u8]) -> u32 {
        if c.len() < LEN_RESOURCE_ID {
            return resp::ERR_INVALID_PARAMETER;
        }
        let id = le32(c, 24);
        let Some(r) = self.find(id) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        r.id = 0;
        if self.shown == id {
            self.hide(scan);
        }
        scan.destroy(id);
        resp::OK_NODATA
    }

    fn set_scanout(&mut self, scan: &mut dyn Scanout, c: &[u8]) -> u32 {
        if c.len() < LEN_SCANOUT {
            return resp::ERR_INVALID_PARAMETER;
        }
        let (rect, scanout, id) = (rect_at(c, 24), le32(c, 40), le32(c, 44));
        if scanout >= SCANOUTS {
            return resp::ERR_INVALID_SCANOUT_ID;
        }
        if id == 0 {
            self.hide(scan);
            return resp::OK_NODATA;
        }
        let Some(r) = self.find(id) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        if rect.w == 0 || rect.h == 0 || !inside(rect, r.width, r.height) {
            return resp::ERR_INVALID_PARAMETER;
        }
        self.shown = id;
        self.window = rect;
        scan.show(scanout, id, rect);
        resp::OK_NODATA
    }

    fn flush(&mut self, scan: &mut dyn Scanout, c: &[u8]) -> u32 {
        if c.len() < LEN_FLUSH {
            return resp::ERR_INVALID_PARAMETER;
        }
        let (rect, id) = (rect_at(c, 24), le32(c, 40));
        let Some(r) = self.find(id) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        if !inside(rect, r.width, r.height) {
            return resp::ERR_INVALID_PARAMETER;
        }
        // Only what is on display is of interest to the host, and only the part that is shown.
        if self.shown == id {
            if let Some(part) = intersect(rect, self.window) {
                scan.present(0, id, part);
            }
        }
        resp::OK_NODATA
    }

    /// Copies a rectangle from the resource's backing to the host. The backing holds the resource's
    /// rows one after another (a row is `4 * width` bytes); `offset` is where the rectangle's first row
    /// starts in it, and the next rows follow one resource row apart.
    fn transfer(&mut self, mem: &dyn GuestMemory, scan: &mut dyn Scanout, c: &[u8]) -> u32 {
        if c.len() < LEN_TRANSFER {
            return resp::ERR_INVALID_PARAMETER;
        }
        let (rect, offset, id) = (rect_at(c, 24), le64(c, 40), le32(c, 48));
        let Some(r) = self.find(id) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        if r.entries == 0 {
            return resp::ERR_UNSPEC;
        }
        if !inside(rect, r.width, r.height) {
            return resp::ERR_INVALID_PARAMETER;
        }
        if rect.w == 0 || rect.h == 0 {
            return resp::OK_NODATA;
        }
        let stride = 4 * u64::from(r.width);
        let row = 4 * u64::from(rect.w);
        let covered = offset
            .checked_add(u64::from(rect.h - 1) * stride + row)
            .is_some_and(|end| end <= r.backing_len);
        if !covered {
            return resp::ERR_INVALID_PARAMETER;
        }
        let backing = &r.backing[..r.entries];
        let mut buf = [0u8; CHUNK];
        for line in 0..rect.h {
            let start = offset + u64::from(line) * stride;
            let mut x = 0;
            while x < rect.w {
                let pixels = (rect.w - x).min((CHUNK / 4) as u32);
                let bytes = &mut buf[..4 * pixels as usize];
                if !read_stream(mem, backing, start + 4 * u64::from(x), bytes) {
                    return resp::ERR_UNSPEC;
                }
                scan.put(id, rect.x + x, rect.y + line, bytes);
                x += pixels;
            }
        }
        resp::OK_NODATA
    }

    /// The entries follow the fixed part of the command, in this buffer or the next ones.
    fn attach_backing(&mut self, mem: &dyn GuestMemory, rd: &[Desc], c: &[u8]) -> u32 {
        if c.len() < LEN_ATTACH {
            return resp::ERR_INVALID_PARAMETER;
        }
        let Some(r) = self.find(le32(c, 24)) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        let n = le32(c, 28) as usize;
        if r.entries != 0 {
            return resp::ERR_UNSPEC;
        }
        if n == 0 {
            return resp::ERR_INVALID_PARAMETER;
        }
        if n > MAX_BACKING {
            return resp::ERR_OUT_OF_MEMORY;
        }
        if total(rd) < (LEN_ATTACH + ENTRY_LEN * n) as u64 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let mut sum = 0u64;
        for k in 0..n {
            let mut e = [0u8; ENTRY_LEN];
            if !read_stream(mem, rd, (LEN_ATTACH + ENTRY_LEN * k) as u64, &mut e) {
                return resp::ERR_UNSPEC;
            }
            let len = le32(&e, 8);
            r.backing[k] = Desc {
                addr: le64(&e, 0),
                len,
                write: false,
            };
            sum += u64::from(len);
        }
        r.entries = n;
        r.backing_len = sum;
        resp::OK_NODATA
    }

    fn detach_backing(&mut self, c: &[u8]) -> u32 {
        if c.len() < LEN_RESOURCE_ID {
            return resp::ERR_INVALID_PARAMETER;
        }
        let Some(r) = self.find(le32(c, 24)) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        r.entries = 0;
        resp::OK_NODATA
    }
}
