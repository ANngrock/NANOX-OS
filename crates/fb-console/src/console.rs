//! Text console with a fixed-size text buffer.

use core::fmt;

use crate::emergency::{emergency_message, EmergencyWriter};
use crate::font::{self, GLYPH_HEIGHT};
use crate::format::{Color, Error, FramebufferInfo};
use crate::render::{cell_origin, draw_glyph};
use crate::surface::Surface;

/// Default 16-entry palette in the familiar CGA/VGA order: black, blue,
/// green, cyan, red, magenta, brown, light grey, then the bright variants.
pub const DEFAULT_PALETTE: [Color; 16] = [
    Color::new(0, 0, 0),
    Color::new(0, 0, 170),
    Color::new(0, 170, 0),
    Color::new(0, 170, 170),
    Color::new(170, 0, 0),
    Color::new(170, 0, 170),
    Color::new(170, 85, 0),
    Color::new(170, 170, 170),
    Color::new(85, 85, 85),
    Color::new(85, 85, 255),
    Color::new(85, 255, 85),
    Color::new(85, 255, 255),
    Color::new(255, 85, 85),
    Color::new(255, 85, 255),
    Color::new(255, 255, 85),
    Color::new(255, 255, 255),
];

/// Foreground and background palette indices (4 bits each).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Attr(u8);

impl Attr {
    /// Light grey on black.
    pub const NORMAL: Self = Self::new(7, 0);
    /// Yellow on black.
    pub const WARNING: Self = Self::new(14, 0);
    /// White on red.
    pub const ERROR: Self = Self::new(15, 4);

    /// Palette indices are taken modulo 16.
    pub const fn new(fg: u8, bg: u8) -> Self {
        Self((fg & 0x0F) | ((bg & 0x0F) << 4))
    }

    pub const fn fg(self) -> u8 {
        self.0 & 0x0F
    }

    pub const fn bg(self) -> u8 {
        self.0 >> 4
    }
}

/// One character cell of the text buffer. `byte` is the stored byte; bytes
/// outside printable ASCII are drawn with the replacement glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub byte: u8,
    pub attr: Attr,
}

impl Cell {
    /// Content of cleared and scrolled-in cells.
    pub const BLANK: Self = Self {
        byte: b' ',
        attr: Attr::NORMAL,
    };
}

/// How the console scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scroll {
    /// Move pixel rows with [`Surface::copy_rows`]; falls back to
    /// [`Scroll::Redraw`] when the surface does not support it. Best for
    /// surfaces in ordinary RAM.
    CopyRows,
    /// Repaint from the text buffer, only the cells whose content changes.
    /// Never reads the framebuffer; use it for MMIO.
    Redraw,
}

/// Byte stored for a non-ASCII `char` written through [`fmt::Write`].
const NON_ASCII: u8 = 0x7F;
const BACKSPACE: u8 = 0x08;
const TAB_WIDTH: usize = 8;

/// A text console of at most `COLS` x `ROWS` cells on a [`Surface`].
///
/// The visible grid is the smaller of the framebuffer's whole 8x16 cells and
/// the const-generic capacity, anchored at the top-left; the rest of the
/// screen stays background. For 1920x1080 the full grid is 240x67, for
/// 1280x800 it is 160x50.
///
/// Control bytes: `\n` moves to the start of the next line (it implies `\r`),
/// `\r` returns to column 0, `\t` advances to the next multiple of 8,
/// backspace (0x08) moves one cell left and erases it (not past column 0).
/// Every other byte outside 0x20..=0x7E occupies one cell drawn with the
/// replacement glyph. Writing past the last column wraps to the next line;
/// a line feed on the last row scrolls.
pub struct Console<S: Surface, const COLS: usize, const ROWS: usize> {
    surface: S,
    info: FramebufferInfo,
    cols: usize,
    rows: usize,
    /// Cursor column; equals `cols` after the last column was written
    /// (pending wrap), so a full line does not scroll until more text comes.
    col: usize,
    row: usize,
    attr: Attr,
    scroll: Scroll,
    palette: [Color; 16],
    raw: [u32; 16],
    cells: [[Cell; COLS]; ROWS],
}

impl<S: Surface, const COLS: usize, const ROWS: usize> Console<S, COLS, ROWS> {
    /// Creates the console and clears the whole framebuffer. `surface` must
    /// draw onto the framebuffer described by `info`.
    pub fn new(surface: S, info: FramebufferInfo, scroll: Scroll) -> Result<Self, Error> {
        if COLS == 0 || ROWS == 0 {
            return Err(Error::ZeroTextCapacity);
        }
        if info.cell_cols() == 0 || info.cell_rows() == 0 {
            return Err(Error::TooSmallForCell);
        }
        let mut console = Self {
            surface,
            info,
            cols: info.cell_cols().min(COLS),
            rows: info.cell_rows().min(ROWS),
            col: 0,
            row: 0,
            attr: Attr::NORMAL,
            scroll,
            palette: DEFAULT_PALETTE,
            raw: DEFAULT_PALETTE.map(|color| info.encode(color)),
            cells: [[Cell::BLANK; COLS]; ROWS],
        };
        console.clear();
        Ok(console)
    }

    /// Visible text columns.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Visible text rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Cursor as `(column, row)`. The column equals [`Self::cols`] while a
    /// wrap is pending after the last column was written.
    pub fn cursor(&self) -> (usize, usize) {
        (self.col, self.row)
    }

    /// Moves the cursor, clamped to the visible grid.
    pub fn set_cursor(&mut self, col: usize, row: usize) {
        self.col = col.min(self.cols - 1);
        self.row = row.min(self.rows - 1);
    }

    pub fn attr(&self) -> Attr {
        self.attr
    }

    /// Attribute for subsequent characters, e.g. [`Attr::WARNING`].
    pub fn set_attr(&mut self, attr: Attr) {
        self.attr = attr;
    }

    pub fn palette(&self) -> &[Color; 16] {
        &self.palette
    }

    /// Changes a palette entry (index modulo 16). Cells already on screen
    /// keep their pixels until [`Self::redraw_all`].
    pub fn set_palette(&mut self, index: u8, color: Color) {
        let i = usize::from(index & 0x0F);
        if let (Some(entry), Some(raw)) = (self.palette.get_mut(i), self.raw.get_mut(i)) {
            *entry = color;
            *raw = self.info.encode(color);
        }
    }

    pub fn info(&self) -> &FramebufferInfo {
        &self.info
    }

    pub fn surface(&self) -> &S {
        &self.surface
    }

    pub fn surface_mut(&mut self) -> &mut S {
        &mut self.surface
    }

    pub fn into_surface(self) -> S {
        self.surface
    }

    /// Text buffer content at `(col, row)`, `None` outside the visible grid.
    pub fn cell(&self, col: usize, row: usize) -> Option<Cell> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells.get(row)?.get(col).copied()
    }

    /// Blanks the text buffer and the whole framebuffer; cursor to (0, 0).
    pub fn clear(&mut self) {
        for row in &mut self.cells {
            row.fill(Cell::BLANK);
        }
        let bg = self.raw_color(Cell::BLANK.attr.bg());
        self.surface
            .fill(0, 0, self.info.width(), self.info.height(), bg);
        self.col = 0;
        self.row = 0;
    }

    /// Repaints the whole screen from the text buffer, e.g. after a palette
    /// change or after something else drew over the framebuffer.
    pub fn redraw_all(&mut self) {
        let bg = self.raw_color(Cell::BLANK.attr.bg());
        let (text_w, text_h) = cell_origin(self.cols, self.rows).unwrap_or((0, 0));
        let (w, h) = (self.info.width(), self.info.height());
        self.surface
            .fill(text_w, 0, w.saturating_sub(text_w), h, bg);
        self.surface
            .fill(0, text_h, text_w, h.saturating_sub(text_h), bg);
        for row in 0..self.rows {
            for col in 0..self.cols {
                self.draw_cell(col, row);
            }
        }
    }

    /// Writes raw bytes; see the type documentation for control bytes. Any
    /// byte sequence is accepted, including invalid UTF-8.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                let next = (self.col / TAB_WIDTH + 1).saturating_mul(TAB_WIDTH);
                self.col = next.min(self.cols);
            }
            BACKSPACE => {
                if self.col > 0 {
                    self.col -= 1;
                    let attr = self.attr;
                    self.store(self.col, self.row, Cell { byte: b' ', attr });
                }
            }
            _ => self.put(byte),
        }
    }

    /// Draws `msg` in the emergency band; see [`emergency_message`]. Uses
    /// only the framebuffer geometry, not the cursor or text buffer.
    pub fn emergency(&mut self, msg: &[u8]) {
        emergency_message(&mut self.surface, &self.info, msg);
    }

    /// A [`fmt::Write`] sink for the emergency band; see [`EmergencyWriter`].
    pub fn emergency_writer(&mut self) -> EmergencyWriter<'_, S> {
        EmergencyWriter::new(&mut self.surface, &self.info)
    }

    fn put(&mut self, byte: u8) {
        if self.col >= self.cols {
            self.newline();
        }
        let attr = self.attr;
        self.store(self.col, self.row, Cell { byte, attr });
        self.col += 1;
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.row = self.rows - 1;
            self.scroll_up();
        }
    }

    fn scroll_up(&mut self) {
        let last = self.rows - 1;
        if self.scroll == Scroll::CopyRows && self.copy_scroll(last) {
            return;
        }
        // Repaint only what changes: the framebuffer already shows the
        // current buffer, so a cell whose new content equals the old one
        // needs no pixel writes.
        for row in 0..=last {
            for col in 0..self.cols {
                let next = if row < last {
                    self.cells.get(row + 1).and_then(|r| r.get(col)).copied()
                } else {
                    Some(Cell::BLANK)
                };
                if let Some(next) = next {
                    self.store(col, row, next);
                }
            }
        }
    }

    /// Scrolls by moving pixels; `false` if the surface cannot, with nothing
    /// changed.
    fn copy_scroll(&mut self, last: usize) -> bool {
        let moved = last * GLYPH_HEIGHT;
        if !self.surface.copy_rows(0, GLYPH_HEIGHT, moved) {
            return false;
        }
        self.cells.copy_within(1..=last, 0);
        if let Some(row) = self.cells.get_mut(last) {
            row.fill(Cell::BLANK);
        }
        let bg = self.raw_color(Cell::BLANK.attr.bg());
        self.surface
            .fill(0, moved, self.info.width(), GLYPH_HEIGHT, bg);
        true
    }

    /// Updates one cell and draws it if its content changed.
    fn store(&mut self, col: usize, row: usize, cell: Cell) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        let Some(slot) = self.cells.get_mut(row).and_then(|r| r.get_mut(col)) else {
            return;
        };
        if *slot != cell {
            *slot = cell;
            self.draw_cell(col, row);
        }
    }

    fn draw_cell(&mut self, col: usize, row: usize) {
        let Some(cell) = self.cell(col, row) else {
            return;
        };
        let Some((x, y)) = cell_origin(col, row) else {
            return;
        };
        let fg = self.raw_color(cell.attr.fg());
        let bg = self.raw_color(cell.attr.bg());
        draw_glyph(&mut self.surface, x, y, font::glyph(cell.byte), fg, bg);
    }

    fn raw_color(&self, index: u8) -> u32 {
        self.raw
            .get(usize::from(index & 0x0F))
            .copied()
            .unwrap_or(0)
    }
}

impl<S: Surface, const COLS: usize, const ROWS: usize> fmt::Write for Console<S, COLS, ROWS> {
    /// Never fails. Each non-ASCII `char` takes one replacement cell.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            match u8::try_from(c) {
                Ok(byte) if byte.is_ascii() => self.write_byte(byte),
                _ => self.put(NON_ASCII),
            }
        }
        Ok(())
    }
}
