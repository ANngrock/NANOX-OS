//! The NANOX screen of the `linux` case: what the probe draws on the
//! firmware's display (UEFI GOP) while the guest runs. It is made of the
//! pieces a NANOX desktop is to use — the server window's place from
//! `serverwin`, `fb-console`'s font and the drawing of `canvas` — so a run
//! shows the server window with a real guest in it (docs/specs/M11-WINDOW.md,
//! "Экран NANOX"): a top bar, the window with its title bar and buttons and
//! the guest's screen scaled into it, and a status line.
//!
//! The screen is composed in a shadow buffer and then copied to the display:
//! the dump at the end of the run reads the shadow, never the display's memory.

use crate::hw::Gop;
use canvas::{blit_scaled, frame, text, text_width, View};
use core::fmt::{self, Write};
use fb_console::{Color, FramebufferDesc, FramebufferInfo, PixelFormat, SliceSurface, Surface};
use serverwin::{Event, GuestState, Mode, Screen, ServerWindow};

const BAR_H: usize = 28;
const STATUS_H: usize = 24;
const TITLE_H: usize = 26;
const BUTTON: usize = 18;
const DESK_TOP: Color = Color::new(26, 35, 48);
const DESK_BOTTOM: Color = Color::new(10, 14, 20);
const BAR: Color = Color::new(9, 12, 17);
const ACCENT: Color = Color::new(79, 195, 247);
const TEXT: Color = Color::new(176, 190, 197);
const DIM: Color = Color::new(110, 125, 135);
const TITLE: Color = Color::new(36, 48, 62);
const BODY: Color = Color::new(0, 0, 0);
const OFF: Color = Color::new(239, 154, 154);

/// What the screen says about the run.
pub struct Status<'a> {
    /// The guest kernel's release (from its "Linux version" line), if seen yet.
    pub kernel: Option<&'a [u8]>,
    pub running: bool,
    pub virtual_ms: u64,
    pub exits: u64,
    pub irqs: u64,
    pub disk_requests: u64,
    pub net: (u64, u64),
    pub agent: (u64, u64),
}

pub struct Display {
    gop: Gop,
    shadow_info: FramebufferInfo,
    shadow: &'static mut [u32],
    win: ServerWindow,
}

impl Display {
    /// A display for `gop` with `shadow` (at least width × height pixels) to
    /// compose in; None unless the mode has blue-green-red pixels (the
    /// guest's own format, copied without conversion) and a framebuffer that
    /// holds its rows.
    pub fn open(gop: Gop, shadow: &'static mut [u32]) -> Option<Self> {
        if gop.format != PixelFormat::GOP_BGRX8 {
            return None;
        }
        let desc = |stride: u32, size_bytes: u64| FramebufferDesc {
            width: gop.width,
            height: gop.height,
            stride,
            format: PixelFormat::Bgrx8,
            size_bytes,
        };
        desc(gop.stride, gop.size).validate().ok()?;
        let pixels = u64::from(gop.width) * u64::from(gop.height);
        let shadow_info = desc(gop.width, pixels * 4).validate().ok()?;
        if (shadow.len() as u64) < pixels {
            return None;
        }
        // The server window as the interface switch opens it and the user
        // expands it, with the guest starting.
        let screen = Screen {
            w: gop.width,
            h: gop.height,
        };
        let mut win = ServerWindow::new(screen, Mode::Desktop);
        for ev in [
            Event::Switch,
            Event::Expand,
            Event::Guest(GuestState::Starting),
        ] {
            win.step(ev);
        }
        Some(Self {
            gop,
            shadow_info,
            shadow,
            win,
        })
    }

    /// The composed screen.
    pub fn view(&self) -> View<'_> {
        let (w, h) = (self.gop.width as usize, self.gop.height as usize);
        View::new(self.shadow, w, h, w).expect("the shadow holds the screen")
    }

    /// Composes the screen with `guest` in the window and shows it.
    pub fn draw(&mut self, guest: &View<'_>, st: &Status<'_>) {
        if st.running && self.win.guest() == GuestState::Starting && st.kernel.is_some() {
            self.win.step(Event::Guest(GuestState::Running));
        }
        let info = self.shadow_info;
        let rect = self.win.rect();
        let Ok(mut s) = SliceSurface::new(self.shadow, &info) else {
            return;
        };
        compose(&mut s, &info, rect, guest, st);
        self.present();
    }

    /// Copies the shadow to the display, row by row (the display's rows may be longer).
    fn present(&mut self) {
        let (w, h) = (self.gop.width as usize, self.gop.height as usize);
        let stride = self.gop.stride as usize;
        for y in 0..h {
            let row = &self.shadow[y * w..(y + 1) * w];
            // SAFETY: the firmware's framebuffer, identity-mapped, holds
            // `stride * height` pixels (validated in `open`); row `y` starts at
            // `y * stride` and is `w <= stride` pixels long; the shadow is
            // probe memory and does not overlap it.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    row.as_ptr(),
                    (self.gop.base as *mut u32).add(y * stride),
                    w,
                );
            }
        }
    }
}

/// A line of text built with `write!`, cut at its capacity.
struct Line {
    buf: [u8; 192],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            buf: [0; 192],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

fn kernel_name<'a>(st: &Status<'a>) -> &'a str {
    st.kernel
        .and_then(|k| core::str::from_utf8(k).ok())
        .unwrap_or("guest")
}

fn compose(
    s: &mut SliceSurface<'_>,
    info: &FramebufferInfo,
    rect: Option<serverwin::Rect>,
    guest: &View<'_>,
    st: &Status<'_>,
) {
    let enc = |c: Color| info.encode(c);
    let (w, h) = (info.width(), info.height());
    // The desktop: a vertical gradient.
    for y in 0..h {
        let mix = |a: u8, b: u8| {
            ((u32::from(a) * (h - y) as u32 + u32::from(b) * y as u32) / h as u32) as u8
        };
        let c = Color::new(
            mix(DESK_TOP.r, DESK_BOTTOM.r),
            mix(DESK_TOP.g, DESK_BOTTOM.g),
            mix(DESK_TOP.b, DESK_BOTTOM.b),
        );
        s.fill(0, y, w, 1, enc(c));
    }

    // The top bar: the system, what is open, the virtual time.
    s.fill(0, 0, w, BAR_H, enc(BAR));
    let x = 14 + text(s, 14, 6, "NANOX", enc(ACCENT), None, 1);
    text(s, x + 8, 6, "desktop  |  server window", enc(DIM), None, 1);
    let mut t = Line::new();
    let _ = write!(
        t,
        "virtual time {}.{:03} s",
        st.virtual_ms / 1000,
        st.virtual_ms % 1000
    );
    text(
        s,
        w.saturating_sub(text_width(t.as_str(), 1) + 14),
        6,
        t.as_str(),
        enc(TEXT),
        None,
        1,
    );

    // The server window where serverwin puts it, or all of the desktop.
    let (rx, ry, rw, rh) = match rect {
        Some(r) => (
            r.x.max(0) as usize,
            r.y.max(0) as usize,
            r.w as usize,
            r.h as usize,
        ),
        None => (0, BAR_H, w, h - BAR_H - STATUS_H),
    };
    if rw < 240 || rh < TITLE_H + 40 {
        return;
    }
    frame(s, rx, ry, rw, rh, 1, enc(ACCENT));
    s.fill(rx + 1, ry + 1, rw - 2, TITLE_H, enc(TITLE));
    let mut title = Line::new();
    let _ = write!(title, "Linux {}  -  NANOX VMM", kernel_name(st));
    text(s, rx + 12, ry + 6, title.as_str(), enc(TEXT), None, 1);
    // Collapse, full screen, close.
    let mut bx = rx + rw - 1 - 6 - BUTTON;
    for label in ["x", "+", "-"] {
        frame(s, bx, ry + 5, BUTTON, BUTTON, 1, enc(DIM));
        text(s, bx + 5, ry + 6, label, enc(TEXT), None, 1);
        bx -= BUTTON + 6;
    }
    let (state, color) = if st.running {
        ("running", ACCENT)
    } else {
        ("powered off", OFF)
    };
    text(
        s,
        bx - text_width(state, 1) - 6,
        ry + 6,
        state,
        enc(color),
        None,
        1,
    );

    // The guest's screen, as large as fits with its proportions, centred.
    let (bx, by) = (rx + 1, ry + 1 + TITLE_H);
    let (bw, bh) = (rw - 2, rh - 2 - TITLE_H);
    s.fill(bx, by, bw, bh, enc(BODY));
    let (gw, gh) = (guest.width(), guest.height());
    if gw > 0 && gh > 0 {
        let (dw, dh) = if bw * gh <= bh * gw {
            (bw, bw * gh / gw)
        } else {
            (bh * gw / gh, bh)
        };
        blit_scaled(s, guest, bx + (bw - dw) / 2, by + (bh - dh) / 2, dw, dh);
    }

    // The status line: what the guest did with its devices.
    s.fill(0, h - STATUS_H, w, STATUS_H, enc(BAR));
    let mut line = Line::new();
    let _ = write!(
        line,
        "exits {}   interrupts {}   disk requests {}   network {} out / {} in   agent {} B out / {} B in",
        st.exits, st.irqs, st.disk_requests, st.net.0, st.net.1, st.agent.0, st.agent.1
    );
    text(s, 14, h - STATUS_H + 4, line.as_str(), enc(DIM), None, 1);
}
