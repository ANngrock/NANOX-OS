//! Original 8x16 bitmap font for printable ASCII (0x20..=0x7E) plus a
//! replacement glyph for everything else.
//!
//! The drawings live in `font/gen_font.py`, which generates `font_data.rs`.
//! Each glyph is 16 bytes, one per pixel row from top to bottom; bit 7 is the
//! leftmost pixel. Column 7 is empty in every printable glyph, so adjacent
//! characters never touch.

use crate::font_data::{PRINTABLE, REPLACEMENT};

pub const GLYPH_WIDTH: usize = 8;
pub const GLYPH_HEIGHT: usize = 16;

/// First and last byte with a printable glyph.
pub const FIRST_PRINTABLE: u8 = 0x20;
pub const LAST_PRINTABLE: u8 = 0x7E;

/// Glyph bitmap type: one byte per row.
pub type Glyph = [u8; GLYPH_HEIGHT];

/// Glyph drawn for bytes and code points outside printable ASCII.
pub const REPLACEMENT_GLYPH: &Glyph = &REPLACEMENT;

/// Whether `byte` has its own glyph.
pub const fn is_printable(byte: u8) -> bool {
    byte >= FIRST_PRINTABLE && byte <= LAST_PRINTABLE
}

/// Glyph for a byte; non-printable bytes get [`REPLACEMENT_GLYPH`].
pub fn glyph(byte: u8) -> &'static Glyph {
    if !is_printable(byte) {
        return REPLACEMENT_GLYPH;
    }
    PRINTABLE
        .get(usize::from(byte - FIRST_PRINTABLE))
        .unwrap_or(REPLACEMENT_GLYPH)
}

/// Glyph for a Unicode scalar; anything but printable ASCII gets
/// [`REPLACEMENT_GLYPH`].
pub fn glyph_for_char(c: char) -> &'static Glyph {
    match u8::try_from(c) {
        Ok(byte) if is_printable(byte) => glyph(byte),
        _ => REPLACEMENT_GLYPH,
    }
}
