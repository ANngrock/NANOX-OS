//! Pixel sinks.
//!
//! Why a trait and not only `&mut [u32]`: the real framebuffer is MMIO
//! mapped write-combining. Plain stores through a slice let the compiler
//! elide, merge or reorder them and would force `unsafe` slice construction
//! over device memory into this crate. With [`Surface`] the kernel owns the
//! mapping and writes with `write_volatile` (and may batch whole rows), while
//! this crate stays `forbid(unsafe_code)`. [`SliceSurface`] covers ordinary
//! memory: tests, shadow buffers, and framebuffers that are known to be safe
//! to treat as RAM.
//!
//! The console only ever passes coordinates inside the validated geometry,
//! but implementations must still drop anything outside it, so a logic error
//! can never become an out-of-bounds write.

use crate::format::{Error, FramebufferInfo};

/// Destination for raw, already-encoded pixel values.
pub trait Surface {
    /// Stores one pixel. Out-of-range coordinates are ignored.
    fn write_pixel(&mut self, x: usize, y: usize, raw: u32);

    /// Stores `pixels` left to right starting at `(x, y)`. Pixels past the
    /// right edge are ignored. Override to issue one burst per row.
    fn write_row(&mut self, x: usize, y: usize, pixels: &[u32]) {
        for (i, &raw) in pixels.iter().enumerate() {
            match x.checked_add(i) {
                Some(px) => self.write_pixel(px, y, raw),
                None => break,
            }
        }
    }

    /// Fills a `w` by `h` rectangle. Parts outside the surface are ignored.
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, raw: u32) {
        for dy in 0..h {
            let Some(py) = y.checked_add(dy) else { break };
            for dx in 0..w {
                let Some(px) = x.checked_add(dx) else { break };
                self.write_pixel(px, py, raw);
            }
        }
    }

    /// Optionally moves `rows` full-width pixel rows from `src_y` to `dst_y`
    /// (overlapping ranges allowed). Returns `false` when unsupported or when
    /// the range is invalid; the console then repaints from its text buffer
    /// instead, so an MMIO framebuffer never has to be read. Must not modify
    /// anything when it returns `false`.
    fn copy_rows(&mut self, dst_y: usize, src_y: usize, rows: usize) -> bool {
        let _ = (dst_y, src_y, rows);
        false
    }
}

impl<S: Surface + ?Sized> Surface for &mut S {
    fn write_pixel(&mut self, x: usize, y: usize, raw: u32) {
        (**self).write_pixel(x, y, raw);
    }

    fn write_row(&mut self, x: usize, y: usize, pixels: &[u32]) {
        (**self).write_row(x, y, pixels);
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, raw: u32) {
        (**self).fill(x, y, w, h, raw);
    }

    fn copy_rows(&mut self, dst_y: usize, src_y: usize, rows: usize) -> bool {
        (**self).copy_rows(dst_y, src_y, rows)
    }
}

/// A [`Surface`] over ordinary memory laid out like the framebuffer
/// (`stride` pixels per row). Only the visible `width` of each row is ever
/// written; stride padding and anything past `stride * height` stay intact.
pub struct SliceSurface<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
    stride: usize,
}

impl<'a> SliceSurface<'a> {
    /// Wraps `pixels`, which must hold at least `info.pixel_len()` values.
    /// A longer slice is allowed; the tail is never touched.
    pub fn new(pixels: &'a mut [u32], info: &FramebufferInfo) -> Result<Self, Error> {
        if pixels.len() < info.pixel_len() {
            return Err(Error::SurfaceTooSmall {
                required: info.pixel_len(),
                actual: pixels.len(),
            });
        }
        Ok(Self {
            pixels,
            width: info.width(),
            height: info.height(),
            stride: info.stride(),
        })
    }

    /// Index of `(x, y)` if it is inside the visible area.
    fn index(&self, x: usize, y: usize) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        y.checked_mul(self.stride)?.checked_add(x)
    }

    /// Read access for tests and for copying a shadow buffer elsewhere.
    pub fn pixels(&self) -> &[u32] {
        self.pixels
    }

    /// The raw value at `(x, y)`, or `None` outside the visible area.
    pub fn pixel(&self, x: usize, y: usize) -> Option<u32> {
        self.pixels.get(self.index(x, y)?).copied()
    }
}

impl Surface for SliceSurface<'_> {
    fn write_pixel(&mut self, x: usize, y: usize, raw: u32) {
        if let Some(i) = self.index(x, y) {
            if let Some(p) = self.pixels.get_mut(i) {
                *p = raw;
            }
        }
    }

    fn write_row(&mut self, x: usize, y: usize, pixels: &[u32]) {
        let Some(start) = self.index(x, y) else {
            return;
        };
        let n = pixels.len().min(self.width - x);
        let Some(end) = start.checked_add(n) else {
            return;
        };
        if let (Some(dst), Some(src)) = (self.pixels.get_mut(start..end), pixels.get(..n)) {
            dst.copy_from_slice(src);
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, raw: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let w = w.min(self.width - x);
        let h = h.min(self.height - y);
        for py in y..y + h {
            let Some(start) = self.index(x, py) else {
                return;
            };
            let Some(end) = start.checked_add(w) else {
                return;
            };
            if let Some(dst) = self.pixels.get_mut(start..end) {
                dst.fill(raw);
            }
        }
    }

    fn copy_rows(&mut self, dst_y: usize, src_y: usize, rows: usize) -> bool {
        let fits = |y: usize| y.checked_add(rows).is_some_and(|end| end <= self.height);
        if !fits(dst_y) || !fits(src_y) {
            return false;
        }
        let mut copy_one = |i: usize| {
            let (Some(src), Some(dst)) = (self.index(0, src_y + i), self.index(0, dst_y + i))
            else {
                return;
            };
            if src
                .checked_add(self.width)
                .is_some_and(|end| end <= self.pixels.len())
                && dst
                    .checked_add(self.width)
                    .is_some_and(|end| end <= self.pixels.len())
            {
                self.pixels.copy_within(src..src + self.width, dst);
            }
        };
        // Copy in the direction that never overwrites a row before reading it.
        if dst_y <= src_y {
            (0..rows).for_each(&mut copy_one);
        } else {
            (0..rows).rev().for_each(&mut copy_one);
        }
        true
    }
}
