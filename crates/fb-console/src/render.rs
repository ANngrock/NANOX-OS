//! Glyph rasterisation shared by the console and the emergency writer.

use crate::font::{Glyph, GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::surface::Surface;

/// Draws `glyph` with its top-left corner at pixel `(x, y)`, writing every
/// pixel of the cell (foreground or background) one row at a time.
pub(crate) fn draw_glyph<S: Surface + ?Sized>(
    surface: &mut S,
    x: usize,
    y: usize,
    glyph: &Glyph,
    fg: u32,
    bg: u32,
) {
    for (dy, &bits) in glyph.iter().enumerate() {
        let Some(py) = y.checked_add(dy) else { return };
        let mut row = [bg; GLYPH_WIDTH];
        for (dx, px) in row.iter_mut().enumerate() {
            if bits & (0x80 >> dx) != 0 {
                *px = fg;
            }
        }
        surface.write_row(x, py, &row);
    }
}

/// Pixel origin of text cell `(col, row)`; `None` only on arithmetic
/// overflow, which validated geometry never produces.
pub(crate) fn cell_origin(col: usize, row: usize) -> Option<(usize, usize)> {
    Some((
        col.checked_mul(GLYPH_WIDTH)?,
        row.checked_mul(GLYPH_HEIGHT)?,
    ))
}
