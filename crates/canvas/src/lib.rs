//! Drawing on a framebuffer beyond a text console: what a screen with windows
//! needs first (docs/specs/M11-WINDOW.md, "Экран NANOX"). Text at any pixel
//! position and integer scale, frames, and a scaled copy of another
//! framebuffer — a guest's screen in a window — all on
//! [`fb_console::Surface`] with `fb-console`'s 8x16 font.
//!
//! Pixels are raw 32-bit values in the destination's format (encode colours
//! with `FramebufferInfo::encode`). [`blit_scaled`] averages every byte of a
//! pixel on its own, so it works for any format whose channels are whole
//! bytes (RGBX, BGRX) as long as source and destination agree.
//!
//! Nothing here panics or writes outside the surface: the surface clips,
//! and every size is checked or saturated.

#![no_std]
#![forbid(unsafe_code)]

use fb_console::font::{glyph_for_char, GLYPH_HEIGHT, GLYPH_WIDTH};
use fb_console::Surface;

/// Pixels to read: `height` rows of `width` 32-bit pixels, `stride` apart.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    pixels: &'a [u32],
    width: usize,
    height: usize,
    stride: usize,
}

impl<'a> View<'a> {
    /// None if a row is longer than the stride or the pixels end before the last row does.
    pub fn new(pixels: &'a [u32], width: usize, height: usize, stride: usize) -> Option<Self> {
        let needed = match height {
            0 => 0,
            h => stride.checked_mul(h - 1)?.checked_add(width)?,
        };
        (width <= stride && needed <= pixels.len()).then_some(Self {
            pixels,
            width,
            height,
            stride,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Row `y` (`y < height`).
    pub fn row(&self, y: usize) -> &'a [u32] {
        &self.pixels[y * self.stride..y * self.stride + self.width]
    }
}

/// Width in pixels of `text` drawn at `scale`.
pub fn text_width(text: &str, scale: usize) -> usize {
    text.chars()
        .count()
        .saturating_mul(GLYPH_WIDTH)
        .saturating_mul(scale)
}

/// Draws `text` with its top left corner at (`x`, `y`), every font pixel a
/// `scale` × `scale` square of `fg`; with `bg`, the glyph cells are filled
/// with it first. Characters outside printable ASCII get the font's
/// replacement glyph. Returns the width drawn ([`text_width`]).
pub fn text<S: Surface + ?Sized>(
    s: &mut S,
    x: usize,
    y: usize,
    text: &str,
    fg: u32,
    bg: Option<u32>,
    scale: usize,
) -> usize {
    let scale = scale.max(1);
    let cell_w = GLYPH_WIDTH.saturating_mul(scale);
    let mut cx = x;
    for c in text.chars() {
        if let Some(bg) = bg {
            s.fill(cx, y, cell_w, GLYPH_HEIGHT.saturating_mul(scale), bg);
        }
        for (gy, &bits) in glyph_for_char(c).iter().enumerate() {
            for gx in 0..GLYPH_WIDTH {
                if bits & (0x80 >> gx) != 0 {
                    s.fill(
                        cx.saturating_add(gx.saturating_mul(scale)),
                        y.saturating_add(gy.saturating_mul(scale)),
                        scale,
                        scale,
                        fg,
                    );
                }
            }
        }
        cx = cx.saturating_add(cell_w);
    }
    cx - x
}

/// A frame `t` pixels thick along the inside of the `w` × `h` rectangle at
/// (`x`, `y`); a frame thicker than half the rectangle fills it.
pub fn frame<S: Surface + ?Sized>(
    s: &mut S,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    t: usize,
    raw: u32,
) {
    let (tw, th) = (t.min(w), t.min(h));
    s.fill(x, y, w, th, raw);
    s.fill(x, y.saturating_add(h - th), w, th, raw);
    s.fill(x, y, tw, h, raw);
    s.fill(x.saturating_add(w - tw), y, tw, h, raw);
}

/// Pixels written per call to the surface.
const CHUNK: usize = 256;

/// `src` scaled into the `w` × `h` rectangle at (`x`, `y`). A destination
/// pixel is the average of the source pixels its area covers (each byte on
/// its own, rounded to nearest), so shrinking keeps thin lines and text
/// visible; enlarging repeats source pixels.
pub fn blit_scaled<S: Surface + ?Sized>(
    s: &mut S,
    src: &View<'_>,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) {
    let (sw, sh) = (src.width, src.height);
    if sw == 0 || sh == 0 {
        return;
    }
    // Source span [from, to) of destination index `d` of `n` over `len` source pixels.
    let span = |d: usize, n: usize, len: usize| {
        let from = d * len / n;
        (from, ((d + 1) * len / n).max(from + 1))
    };
    let mut out = [0u32; CHUNK];
    for dy in 0..h {
        let (y0, y1) = span(dy, h, sh);
        let mut dx = 0;
        while dx < w {
            let n = (w - dx).min(CHUNK);
            for (i, px) in out[..n].iter_mut().enumerate() {
                let (x0, x1) = span(dx + i, w, sw);
                let mut sum = [0u64; 4];
                for sy in y0..y1 {
                    for &p in &src.row(sy)[x0..x1] {
                        for (k, acc) in sum.iter_mut().enumerate() {
                            *acc += u64::from((p >> (8 * k)) & 0xFF);
                        }
                    }
                }
                let count = ((y1 - y0) * (x1 - x0)) as u64;
                *px = sum.iter().enumerate().fold(0, |v, (k, &acc)| {
                    v | (((acc + count / 2) / count) as u32) << (8 * k)
                });
            }
            s.write_row(x.saturating_add(dx), y.saturating_add(dy), &out[..n]);
            dx += n;
        }
    }
}
