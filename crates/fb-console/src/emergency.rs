//! Emergency output for panic and fatal-error paths.
//!
//! The writer depends only on the validated framebuffer geometry and a
//! surface, never on a [`crate::Console`]'s cursor, text buffer or palette,
//! so it still works when that state is corrupt or its lock is held by the
//! code that panicked. It draws white on red into a fixed band of
//! [`EMERGENCY_ROWS`] text rows at the top of the screen, never scrolls and
//! truncates what does not fit (marking the last cell with `>`).

use core::fmt;

use crate::font::{self, GLYPH_HEIGHT};
use crate::format::{Color, FramebufferInfo};
use crate::render::{cell_origin, draw_glyph};
use crate::surface::Surface;

/// Text rows in the emergency band (fewer if the screen is shorter).
pub const EMERGENCY_ROWS: usize = 4;

const EMERGENCY_FG: Color = Color::WHITE;
const EMERGENCY_BG: Color = Color::new(170, 0, 0);
const TAB_WIDTH: usize = 8;

/// Clears the emergency band and writes `msg` into it. Accepts any bytes.
/// Does nothing if the framebuffer is smaller than one character cell.
pub fn emergency_message<S: Surface + ?Sized>(surface: &mut S, info: &FramebufferInfo, msg: &[u8]) {
    EmergencyWriter::new(surface, info).write_bytes(msg);
}

/// Cursor over the emergency band; implements [`fmt::Write`] so a panic
/// handler can format its message without allocating.
pub struct EmergencyWriter<'a, S: Surface + ?Sized> {
    surface: &'a mut S,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: u32,
    bg: u32,
    truncated: bool,
}

impl<'a, S: Surface + ?Sized> EmergencyWriter<'a, S> {
    /// Clears the band to the emergency background and starts at its
    /// top-left cell.
    pub fn new(surface: &'a mut S, info: &FramebufferInfo) -> Self {
        let cols = info.cell_cols();
        let rows = info.cell_rows().min(EMERGENCY_ROWS);
        let bg = info.encode(EMERGENCY_BG);
        if cols > 0 && rows > 0 {
            surface.fill(0, 0, info.width(), rows * GLYPH_HEIGHT, bg);
        }
        Self {
            surface,
            cols,
            rows,
            col: 0,
            row: 0,
            fg: info.encode(EMERGENCY_FG),
            bg,
            truncated: cols == 0 || rows == 0,
        }
    }

    /// Whether some output did not fit (always true for a framebuffer
    /// smaller than one cell).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.truncated {
                return;
            }
            self.write_byte(byte);
        }
    }

    pub fn write_byte(&mut self, byte: u8) {
        if self.truncated {
            return;
        }
        match byte {
            b'\n' => {
                self.col = 0;
                self.row = self.row.saturating_add(1);
            }
            b'\r' => self.col = 0,
            b'\t' => {
                let next = (self.col / TAB_WIDTH + 1).saturating_mul(TAB_WIDTH);
                self.col = next.min(self.cols);
            }
            _ => self.put(byte),
        }
    }

    fn put(&mut self, byte: u8) {
        if self.col >= self.cols {
            self.col = 0;
            self.row = self.row.saturating_add(1);
        }
        if self.row >= self.rows {
            self.truncated = true;
            self.draw(self.cols - 1, self.rows - 1, b'>');
            return;
        }
        self.draw(self.col, self.row, byte);
        self.col += 1;
    }

    fn draw(&mut self, col: usize, row: usize, byte: u8) {
        if let Some((x, y)) = cell_origin(col, row) {
            draw_glyph(
                &mut *self.surface,
                x,
                y,
                font::glyph(byte),
                self.fg,
                self.bg,
            );
        }
    }
}

impl<S: Surface + ?Sized> fmt::Write for EmergencyWriter<'_, S> {
    /// Never fails, so a panic handler cannot fail again while reporting.
    /// Each non-ASCII `char` takes one replacement cell.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            if self.truncated {
                break;
            }
            match u8::try_from(c) {
                Ok(byte) if byte.is_ascii() => self.write_byte(byte),
                _ => self.put(0x7F),
            }
        }
        Ok(())
    }
}
