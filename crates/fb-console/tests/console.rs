//! Console behaviour read back from pixels: cursor control, wrapping,
//! scrolling in both modes, colours and `fmt::Write`.

mod common;

use core::fmt::Write as _;

use common::*;
use fb_console::{
    Attr, Cell, Color, Console, FramebufferInfo, PixelFormat, Scroll, SliceSurface, Surface,
};

const R: char = '\u{FFFD}';

/// 10 x 3 cells, stride padding of 6 pixels.
fn small() -> FramebufferInfo {
    info(80, 48, 86, PixelFormat::Bgrx8)
}

fn run(scroll: Scroll, input: &[u8]) -> (Vec<String>, (usize, usize)) {
    let info = small();
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 10, 3> = console(&mut pixels, info, scroll);
    con.write_bytes(input);
    assert_matches_buffer(&con);
    let cursor = con.cursor();
    assert_canaries(&pixels, &info);
    (read_screen(&pixels, &info, 10, 3), cursor)
}

fn both(input: &[u8]) -> (Vec<String>, (usize, usize)) {
    let copy = run(Scroll::CopyRows, input);
    let redraw = run(Scroll::Redraw, input);
    assert_eq!(copy, redraw, "scroll modes disagree");
    copy
}

#[test]
fn text_and_line_feed() {
    let (screen, cursor) = both(b"hello\nworld");
    assert_eq!(screen, ["hello", "world", ""]);
    assert_eq!(cursor, (5, 1));
}

#[test]
fn carriage_return_overwrites_the_line() {
    let (screen, cursor) = both(b"abcdef\rXY");
    assert_eq!(screen, ["XYcdef", "", ""]);
    assert_eq!(cursor, (2, 0));
}

#[test]
fn tab_advances_to_the_next_multiple_of_eight() {
    // From column 1 and from column 7 the next stop is 8.
    let (screen, _) = both(b"a\tb\n1234567\tc");
    assert_eq!(screen, ["a       b", "1234567 c", ""]);
    // From column 8 the next stop is 16, past the 10-column grid, so the
    // cursor waits at the right edge and the next character wraps.
    let (screen, cursor) = both(b"\n\n12345678\td");
    assert_eq!(screen, ["", "12345678", "d"]);
    assert_eq!(cursor, (1, 2));
}

#[test]
fn tab_does_not_erase() {
    let (screen, _) = both(b"abcdefgh\r\tX");
    assert_eq!(screen[0], "abcdefghX");
}

#[test]
fn backspace_moves_left_and_erases() {
    let (screen, cursor) = both(b"abc\x08\x08X");
    assert_eq!(screen[0], "aX");
    assert_eq!(cursor, (2, 0));
}

#[test]
fn backspace_stops_at_column_zero() {
    let (screen, cursor) = both(b"ab\ncd\x08\x08\x08\x08e");
    assert_eq!(screen, ["ab", "e", ""]);
    assert_eq!(cursor, (1, 1));
}

#[test]
fn backspace_after_a_full_line_erases_the_last_column() {
    let (screen, cursor) = both(b"0123456789\x08");
    assert_eq!(screen[0], "012345678");
    assert_eq!(cursor, (9, 0));
}

#[test]
fn full_line_waits_before_wrapping() {
    let (screen, cursor) = both(b"0123456789");
    assert_eq!(screen, ["0123456789", "", ""]);
    assert_eq!(cursor, (10, 0));
    // A line feed right after a full line does not leave an empty line.
    let (screen, _) = both(b"0123456789\nx");
    assert_eq!(screen, ["0123456789", "x", ""]);
    // More text wraps.
    let (screen, cursor) = both(b"0123456789abc");
    assert_eq!(screen, ["0123456789", "abc", ""]);
    assert_eq!(cursor, (3, 1));
}

#[test]
fn line_feed_on_the_last_row_scrolls() {
    let (screen, cursor) = both(b"1\n2\n3\n4");
    assert_eq!(screen, ["2", "3", "4"]);
    assert_eq!(cursor, (1, 2));
    let (screen, _) = both(b"1\n2\n3\n4\n5\n6\n7\n");
    assert_eq!(screen, ["6", "7", ""]);
}

#[test]
fn wrapping_on_the_last_row_scrolls() {
    let (screen, _) = both(b"aaaaaaaaaabbbbbbbbbbccccccccccdd");
    assert_eq!(screen, ["bbbbbbbbbb", "cccccccccc", "dd"]);
}

#[test]
fn other_control_and_high_bytes_use_the_replacement_glyph() {
    let (screen, _) = both(b"a\x00b\x07c\x7f\xff\x1b");
    let want: String = ['a', R, 'b', R, 'c', R, R, R].iter().collect();
    assert_eq!(screen[0], want);
}

#[test]
fn colours_follow_the_attribute() {
    let info = info(64, 16, 64, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 8, 1> = console(&mut pixels, info, Scroll::Redraw);
    con.write_bytes(b"N");
    con.set_attr(Attr::WARNING);
    con.write_bytes(b"W");
    con.set_attr(Attr::ERROR);
    con.write_bytes(b"E");
    con.set_attr(Attr::NORMAL);
    con.write_bytes(b"n");
    assert_eq!(con.cell(1, 0).unwrap().attr, Attr::WARNING);
    // Rgbx8: 0x00BBGGRR.
    const GREY: u32 = 0x00AA_AAAA;
    const YELLOW: u32 = 0x0055_FFFF;
    const WHITE: u32 = 0x00FF_FFFF;
    const RED: u32 = 0x0000_00AA;
    assert_eq!(cell_colors(&pixels, &info, 0, 0), (Some(GREY), 0));
    assert_eq!(cell_colors(&pixels, &info, 1, 0), (Some(YELLOW), 0));
    assert_eq!(cell_colors(&pixels, &info, 2, 0), (Some(WHITE), RED));
    assert_eq!(cell_colors(&pixels, &info, 3, 0), (Some(GREY), 0));
    assert_eq!(read_screen(&pixels, &info, 8, 1), ["NWEn"]);
}

#[test]
fn attribute_indices_are_four_bits() {
    let a = Attr::new(0xF3, 0xA5);
    assert_eq!((a.fg(), a.bg()), (3, 5));
    assert_eq!((Attr::ERROR.fg(), Attr::ERROR.bg()), (15, 4));
}

#[test]
fn palette_change_applies_after_redraw_all() {
    let info = info(32, 16, 32, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 4, 1> = console(&mut pixels, info, Scroll::Redraw);
    con.write_bytes(b"#");
    con.set_palette(7, Color::new(1, 2, 3));
    // Existing pixels keep the old colour until a redraw.
    assert_eq!(
        cell_colors(con.surface().pixels(), &info, 0, 0).0,
        Some(0x00AA_AAAA)
    );
    con.redraw_all();
    assert_eq!(
        cell_colors(con.surface().pixels(), &info, 0, 0).0,
        Some(0x0003_0201)
    );
    con.set_palette(0x10, Color::new(9, 9, 9)); // index 0x10 is entry 0
    con.redraw_all();
    assert_eq!(
        cell_colors(con.surface().pixels(), &info, 0, 0).1,
        0x0009_0909
    );
    assert_matches_buffer(&con);
}

#[test]
fn fmt_write_formats_and_replaces_non_ascii() {
    let info = info(160, 32, 160, PixelFormat::Bgrx8);
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 20, 2> = console(&mut pixels, info, Scroll::Redraw);
    writeln!(con, "x={} y={:#x}", 42, 255).unwrap();
    write!(con, "é→ok").unwrap();
    assert_eq!(con.cursor(), (4, 1));
    let want: String = [R, R, 'o', 'k'].iter().collect();
    assert_eq!(read_screen(&pixels, &info, 20, 2), ["x=42 y=0xff", &want]);
}

#[test]
fn write_bytes_accepts_invalid_utf8_byte_by_byte() {
    // 'é' as UTF-8 is two bytes, so two replacement cells; a lone
    // continuation byte and an overlong prefix are one cell each.
    let (screen, _) = both("é".as_bytes());
    assert_eq!(screen[0], [R, R].iter().collect::<String>());
    let (screen, _) = both(b"\x80\xc0z");
    assert_eq!(screen[0], [R, R, 'z'].iter().collect::<String>());
}

#[test]
fn set_cursor_is_clamped_to_the_grid() {
    let info = small();
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 10, 3> = console(&mut pixels, info, Scroll::Redraw);
    con.set_cursor(usize::MAX, usize::MAX);
    assert_eq!(con.cursor(), (9, 2));
    // 'X' fills the last cell; 'Y' wraps, which scrolls 'X' up one row.
    con.write_bytes(b"XY");
    assert_eq!(con.cell(9, 1).unwrap().byte, b'X');
    assert_eq!(con.cell(0, 2).unwrap().byte, b'Y');
    assert_eq!(con.cursor(), (1, 2));
    assert_eq!(con.cell(10, 0), None);
    assert_eq!(con.cell(0, 3), None);
    con.set_cursor(3, 0);
    con.write_bytes(b"Z");
    assert_matches_buffer(&con);
    assert_eq!(
        read_screen(&pixels, &info, 10, 3),
        ["   Z", "         X", "Y"]
    );
}

#[test]
fn clear_blanks_everything_and_homes_the_cursor() {
    let info = small();
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 10, 3> = console(&mut pixels, info, Scroll::CopyRows);
    con.set_attr(Attr::ERROR);
    con.write_bytes(b"lots of text here\nand more");
    con.clear();
    assert_eq!(con.cursor(), (0, 0));
    assert!((0..3).all(|r| (0..10).all(|c| con.cell(c, r) == Some(Cell::BLANK))));
    assert_matches_buffer(&con);
}

#[test]
fn stride_padding_and_margins_are_never_touched() {
    // 100x40 visible (12x2 cells, 4 px right margin, 8 px bottom margin),
    // stride 128.
    for scroll in [Scroll::CopyRows, Scroll::Redraw] {
        let info = info(100, 40, 128, PixelFormat::Rgbx8);
        let mut pixels = buffer(&info);
        let mut con: Con<'_, 64, 64> = console(&mut pixels, info, scroll);
        assert_eq!((con.cols(), con.rows()), (12, 2));
        con.set_attr(Attr::ERROR);
        for i in 0..50 {
            writeln!(con, "line {i} with wrapping text\t|").unwrap();
        }
        assert_matches_buffer(&con);
        assert_canaries(&pixels, &info);
    }
}

#[test]
fn text_capacity_smaller_than_the_screen_limits_the_grid() {
    let info = info(1280, 800, 1280, PixelFormat::Bgrx8);
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 10, 3> = console(&mut pixels, info, Scroll::CopyRows);
    assert_eq!((con.cols(), con.rows()), (10, 3));
    con.write_bytes(b"0123456789ABCDEFGHIJ\nKLM\nNOP");
    assert_matches_buffer(&con);
    assert_eq!(
        read_screen(&pixels, &info, 10, 3),
        ["ABCDEFGHIJ", "KLM", "NOP"]
    );
    // Everything right of and below the 80x48 grid is background.
    assert!((0..800).all(|y| (80..1280).all(|x| px(&pixels, &info, x, y) == 0)));
    assert!((48..800).all(|y| (0..80).all(|x| px(&pixels, &info, x, y) == 0)));
}

/// Fills every row, then scrolls once more; returns the final screen.
fn fill_and_scroll(info: FramebufferInfo, scroll: Scroll) -> Vec<String> {
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 240, 67> = console(&mut pixels, info, scroll);
    let (cols, rows) = (con.cols(), con.rows());
    for row in 0..=rows {
        // Distinct content per row, exactly `cols` wide on even rows.
        let text: String = (0..cols)
            .map(|c| char::from(b'!' + ((row * 7 + c) % 94) as u8))
            .take(if row % 2 == 0 { cols } else { cols / 3 })
            .collect();
        con.write_str(&text).unwrap();
        if row < rows {
            con.write_bytes(b"\n");
        }
    }
    con.set_attr(Attr::WARNING);
    con.write_bytes(b"\nlast line");
    assert_matches_buffer(&con);
    assert_canaries(&pixels, &info);
    read_screen(&pixels, &info, cols, rows)
}

#[test]
fn laptop_1920x1080_fill_and_scroll() {
    let info = info(1920, 1080, 1920, PixelFormat::Bgrx8);
    let copy = fill_and_scroll(info, Scroll::CopyRows);
    let redraw = fill_and_scroll(info, Scroll::Redraw);
    assert_eq!(copy, redraw);
    assert_eq!(copy.len(), 67);
    assert_eq!(copy[66], "last line");
    // Row 0 now shows what was written as row 2.
    let row2: String = (0..240)
        .map(|c| char::from(b'!' + ((2 * 7 + c) % 94) as u8))
        .collect();
    assert_eq!(copy[0], row2);
}

#[test]
fn ovmf_1280x800_with_stride_padding_fill_and_scroll() {
    let info = info(1280, 800, 1344, PixelFormat::Rgbx8);
    let copy = fill_and_scroll(info, Scroll::CopyRows);
    let redraw = fill_and_scroll(info, Scroll::Redraw);
    assert_eq!(copy, redraw);
    assert_eq!(copy.len(), 50);
    assert_eq!(copy[49], "last line");
}

/// Forwards pixel writes only: no `copy_rows`, counts every call.
struct Counting<'a> {
    inner: SliceSurface<'a>,
    rows_written: usize,
}

impl Surface for Counting<'_> {
    fn write_pixel(&mut self, x: usize, y: usize, raw: u32) {
        self.inner.write_pixel(x, y, raw);
    }

    fn write_row(&mut self, x: usize, y: usize, pixels: &[u32]) {
        self.rows_written += 1;
        self.inner.write_row(x, y, pixels);
    }
}

#[test]
fn copy_mode_falls_back_to_redraw_without_copy_rows() {
    let info = small();
    let script = b"one\ntwo\nthree\nfour\nfive and wrap around\n\tsix";
    let mut plain = buffer(&info);
    let mut con: Con<'_, 10, 3> = console(&mut plain, info, Scroll::Redraw);
    con.write_bytes(script);

    let mut fallback = buffer(&info);
    let surface = Counting {
        inner: SliceSurface::new(&mut fallback, &info).unwrap(),
        rows_written: 0,
    };
    let mut con = Console::<_, 10, 3>::new(surface, info, Scroll::CopyRows, leaked_text()).unwrap();
    con.write_bytes(script);
    assert_eq!(plain, fallback);
}

#[test]
fn redraw_scroll_repaints_only_changed_cells() {
    let info = small();
    let mut pixels = buffer(&info);
    let surface = Counting {
        inner: SliceSurface::new(&mut pixels, &info).unwrap(),
        rows_written: 0,
    };
    let mut con = Console::<_, 10, 3>::new(surface, info, Scroll::Redraw, leaked_text()).unwrap();
    con.write_bytes(b"xx\nxx\nxx");
    let before = con.surface().rows_written;
    // Scrolling identical rows changes only the last row: two 'x' cells
    // become blank, 16 row writes each.
    con.write_bytes(b"\n");
    assert_eq!(con.surface().rows_written - before, 2 * 16);
    // Writing a character equal to what is already there costs nothing;
    // a different one costs one cell.
    con.set_cursor(0, 0);
    let before = con.surface().rows_written;
    con.write_bytes(b"xy");
    assert_eq!(con.surface().rows_written - before, 16);
}

#[test]
fn console_works_through_a_trait_object() {
    let info = small();
    let mut pixels = buffer(&info);
    let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
    let dynamic: &mut dyn Surface = &mut surface;
    let mut con = Console::<_, 10, 3>::new(dynamic, info, Scroll::CopyRows, leaked_text()).unwrap();
    con.write_bytes(b"1\n2\n3\n4");
    assert_eq!(read_screen(&pixels, &info, 10, 3), ["2", "3", "4"]);
}

#[test]
fn text_buffer_is_static_and_console_is_small() {
    use std::sync::Mutex;
    // The kernel keeps the text in a static behind its lock; `new` is const.
    static TEXT: Mutex<fb_console::TextBuffer<240, 67>> = Mutex::new(fb_console::TextBuffer::new());
    let info = info(1920, 1080, 1920, fb_console::PixelFormat::Bgrx8);
    let mut pixels = buffer(&info);
    let mut text = TEXT.lock().unwrap();
    let surface = fb_console::SliceSurface::new(&mut pixels, &info).unwrap();
    let mut con = Console::new(surface, info, Scroll::Redraw, &mut text).unwrap();
    con.write_bytes(b"static text");
    assert_eq!(con.cell(0, 0).unwrap().byte, b's');
    assert!(
        std::mem::size_of::<Con<'_, 240, 67>>() < 512,
        "console must stay small: {}",
        std::mem::size_of::<Con<'_, 240, 67>>()
    );
    assert!(std::mem::size_of::<fb_console::TextBuffer<240, 67>>() >= 240 * 67 * 2);
}
