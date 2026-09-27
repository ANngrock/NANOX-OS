//! Shared helpers: framebuffers with canaries, a glyph recogniser that reads
//! text back from pixels, and a deterministic PRNG.
#![allow(dead_code)]

use fb_console::font::{self, GLYPH_HEIGHT, GLYPH_WIDTH};
use fb_console::{
    Cell, Console, FramebufferDesc, FramebufferInfo, PixelFormat, Scroll, SliceSurface,
};

/// Value of every pixel the console must never write: stride padding and the
/// tail past `stride * height`.
pub const CANARY: u32 = 0xDEAD_BEEF;
/// Extra pixels after the framebuffer proper.
pub const TAIL: usize = 257;

pub fn desc(width: u32, height: u32, stride: u32, format: PixelFormat) -> FramebufferDesc {
    FramebufferDesc {
        width,
        height,
        stride,
        format,
        size_bytes: u64::from(stride) * u64::from(height) * 4,
    }
}

pub fn info(width: u32, height: u32, stride: u32, format: PixelFormat) -> FramebufferInfo {
    desc(width, height, stride, format)
        .validate()
        .expect("valid test framebuffer")
}

/// A pixel buffer of `pixel_len + TAIL` values, all [`CANARY`].
pub fn buffer(info: &FramebufferInfo) -> Vec<u32> {
    vec![CANARY; info.pixel_len() + TAIL]
}

/// Panics if anything outside the visible area was written.
pub fn assert_canaries(pixels: &[u32], info: &FramebufferInfo) {
    for y in 0..info.height() {
        for x in info.width()..info.stride() {
            assert_eq!(
                pixels[y * info.stride() + x],
                CANARY,
                "stride padding written at ({x}, {y})"
            );
        }
    }
    assert!(
        pixels[info.pixel_len()..].iter().all(|&p| p == CANARY),
        "pixels past the framebuffer were written"
    );
}

pub type Con<'a, const C: usize, const R: usize> = Console<SliceSurface<'a>, C, R>;

pub fn console<'a, const C: usize, const R: usize>(
    pixels: &'a mut [u32],
    info: FramebufferInfo,
    scroll: Scroll,
) -> Con<'a, C, R> {
    let surface = SliceSurface::new(pixels, &info).expect("surface");
    Console::new(surface, info, scroll).expect("console")
}

pub fn px(pixels: &[u32], info: &FramebufferInfo, x: usize, y: usize) -> u32 {
    assert!(x < info.width() && y < info.height());
    pixels[y * info.stride() + x]
}

/// Glyph bitmap of the cell at `(col, row)`: a bit is set where the pixel
/// differs from the cell's top-left pixel, which is background for every
/// glyph (row 0 is always empty).
pub fn cell_bits(pixels: &[u32], info: &FramebufferInfo, col: usize, row: usize) -> [u8; 16] {
    let (x0, y0) = (col * GLYPH_WIDTH, row * GLYPH_HEIGHT);
    let bg = px(pixels, info, x0, y0);
    let mut bits = [0u8; 16];
    for (dy, b) in bits.iter_mut().enumerate() {
        for dx in 0..GLYPH_WIDTH {
            if px(pixels, info, x0 + dx, y0 + dy) != bg {
                *b |= 0x80 >> dx;
            }
        }
    }
    bits
}

/// `(foreground, background)` raw values of a cell; the foreground is `None`
/// for a cell without lit pixels.
pub fn cell_colors(
    pixels: &[u32],
    info: &FramebufferInfo,
    col: usize,
    row: usize,
) -> (Option<u32>, u32) {
    let (x0, y0) = (col * GLYPH_WIDTH, row * GLYPH_HEIGHT);
    let bg = px(pixels, info, x0, y0);
    for dy in 0..GLYPH_HEIGHT {
        for dx in 0..GLYPH_WIDTH {
            let p = px(pixels, info, x0 + dx, y0 + dy);
            if p != bg {
                return (Some(p), bg);
            }
        }
    }
    (None, bg)
}

/// Character shown in a cell: printable ASCII, or `'\u{FFFD}'` for the
/// replacement glyph. Panics on pixels that are not a glyph.
pub fn read_cell(pixels: &[u32], info: &FramebufferInfo, col: usize, row: usize) -> char {
    let bits = cell_bits(pixels, info, col, row);
    if &bits == font::REPLACEMENT_GLYPH {
        return '\u{FFFD}';
    }
    (font::FIRST_PRINTABLE..=font::LAST_PRINTABLE)
        .find(|&b| font::glyph(b) == &bits)
        .map(char::from)
        .unwrap_or_else(|| panic!("cell ({col}, {row}) is not a glyph: {bits:02X?}"))
}

/// Text read back from pixels, one string per text row, trailing spaces
/// removed.
pub fn read_screen(
    pixels: &[u32],
    info: &FramebufferInfo,
    cols: usize,
    rows: usize,
) -> Vec<String> {
    (0..rows)
        .map(|row| {
            let line: String = (0..cols)
                .map(|col| read_cell(pixels, info, col, row))
                .collect();
            line.trim_end_matches(' ').to_string()
        })
        .collect()
}

/// Asserts that the framebuffer shows exactly the console's text buffer:
/// every cell has the glyph of its stored byte in its attribute's colours,
/// and everything outside the text grid is the normal background.
pub fn assert_matches_buffer<const C: usize, const R: usize>(con: &Con<'_, C, R>) {
    let info = *con.info();
    let pixels = con.surface().pixels();
    let raw: Vec<u32> = con.palette().iter().map(|&c| info.encode(c)).collect();
    for row in 0..con.rows() {
        for col in 0..con.cols() {
            let cell = con.cell(col, row).expect("cell in grid");
            let glyph = font::glyph(cell.byte);
            let fg = raw[usize::from(cell.attr.fg())];
            let bg = raw[usize::from(cell.attr.bg())];
            for (dy, &bits) in glyph.iter().enumerate() {
                for dx in 0..GLYPH_WIDTH {
                    let lit = bits & (0x80 >> dx) != 0;
                    let want = if lit { fg } else { bg };
                    let got = px(
                        pixels,
                        &info,
                        col * GLYPH_WIDTH + dx,
                        row * GLYPH_HEIGHT + dy,
                    );
                    assert_eq!(got, want, "cell ({col}, {row}) pixel ({dx}, {dy})");
                }
            }
        }
    }
    let bg = raw[usize::from(Cell::BLANK.attr.bg())];
    let (text_w, text_h) = (con.cols() * GLYPH_WIDTH, con.rows() * GLYPH_HEIGHT);
    for y in 0..info.height() {
        for x in 0..info.width() {
            if x >= text_w || y >= text_h {
                assert_eq!(px(pixels, &info, x, y), bg, "margin pixel ({x}, {y})");
            }
        }
    }
}

/// xorshift64*: deterministic, dependency-free test randomness.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}
