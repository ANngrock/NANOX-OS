//! Emergency band: fixed place and colours, independent of console state,
//! truncation, and nothing drawn outside the band.

mod common;

use core::fmt::Write as _;

use common::*;
use fb_console::font::GLYPH_HEIGHT;
use fb_console::{
    emergency_message, Attr, Color, EmergencyWriter, PixelFormat, Scroll, SliceSurface, Surface,
    EMERGENCY_ROWS,
};

// Rgbx8 (0x00BBGGRR): white text on (170, 0, 0).
const WHITE: u32 = 0x00FF_FFFF;
const RED: u32 = 0x0000_00AA;

#[test]
fn message_is_white_on_red_in_the_top_band() {
    let info = info(1280, 800, 1280, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 160, 50> = console(&mut pixels, info, Scroll::CopyRows);
    for i in 0..200 {
        writeln!(con, "boot log line {i}").unwrap();
    }
    let before = con.surface().pixels().to_vec();
    con.emergency(b"KERNEL PANIC: page fault\nat 0xdeadbeef");

    let band = read_screen(&pixels, &info, 160, EMERGENCY_ROWS);
    assert_eq!(band, ["KERNEL PANIC: page fault", "at 0xdeadbeef", "", ""]);
    assert_eq!(cell_colors(&pixels, &info, 0, 0), (Some(WHITE), RED));
    // The whole band, full width, is red or white.
    let band_px = EMERGENCY_ROWS * GLYPH_HEIGHT;
    for y in 0..band_px {
        for x in 0..1280 {
            let p = px(&pixels, &info, x, y);
            assert!(p == RED || p == WHITE, "({x}, {y}) = {p:#x}");
        }
    }
    // Below the band nothing changed.
    let start = band_px * info.stride();
    assert_eq!(
        pixels[start..info.pixel_len()],
        before[start..info.pixel_len()]
    );
    assert_canaries(&pixels, &info);
}

/// Band pixels after writing `msg` to a fresh, never-used framebuffer.
fn reference_band(msg: &[u8]) -> Vec<u32> {
    let info = info(640, 480, 672, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
    emergency_message(&mut surface, &info, msg);
    pixels[..EMERGENCY_ROWS * GLYPH_HEIGHT * info.stride()].to_vec()
}

#[test]
fn output_does_not_depend_on_console_state() {
    let msg = b"panic: assertion failed in scheduler";
    let info = info(640, 480, 672, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 80, 30> = console(&mut pixels, info, Scroll::Redraw);
    // Scramble everything the console owns: palette, attribute, cursor in
    // the pending-wrap state on the last row, text everywhere.
    for i in 0..16 {
        con.set_palette(i, Color::new(i * 13, 255 - i, 7));
    }
    con.set_attr(Attr::new(3, 9));
    con.write_bytes(&[b'#'; 80 * 45]);
    assert_eq!(con.cursor(), (80, 29));
    con.emergency(msg);
    let band = EMERGENCY_ROWS * GLYPH_HEIGHT * info.stride();
    assert_eq!(pixels[..band], reference_band(msg)[..]);
}

#[test]
fn long_messages_wrap_and_truncate_with_a_marker() {
    let info = info(80, 160, 80, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
    let mut w = EmergencyWriter::new(&mut surface, &info);
    w.write_bytes(b"0123456789abcdefghijKLMNOPQRSTuvwxyz!@#$%^&*()-overflow");
    assert!(w.truncated());
    assert_eq!(
        read_screen(&pixels, &info, 10, 10),
        [
            "0123456789",
            "abcdefghij",
            "KLMNOPQRST",
            "uvwxyz!@#>",
            "",
            "",
            "",
            "",
            "",
            ""
        ]
    );
    // Rows below the band were never written.
    let band_end = EMERGENCY_ROWS * GLYPH_HEIGHT * info.stride();
    assert!(pixels[band_end..].iter().all(|&p| p == CANARY));
}

#[test]
fn a_message_that_fits_is_not_marked() {
    let info = info(80, 160, 80, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
    let mut w = EmergencyWriter::new(&mut surface, &info);
    w.write_bytes(&[b'z'; 40]);
    assert!(!w.truncated());
    assert_eq!(read_screen(&pixels, &info, 10, 4), ["zzzzzzzzzz"; 4]);
}

#[test]
fn control_bytes_in_the_band() {
    let info = info(160, 64, 160, PixelFormat::Bgrx8);
    let mut pixels = buffer(&info);
    let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
    emergency_message(&mut surface, &info, b"a\tb\r\nabc\rX\n\x00\xff");
    let r = '\u{FFFD}';
    assert_eq!(
        read_screen(&pixels, &info, 20, 4),
        [
            "a       b".to_string(),
            "Xbc".to_string(),
            [r, r].iter().collect(),
            String::new()
        ]
    );
}

#[test]
fn fmt_writer_for_panic_handlers() {
    let info = info(480, 64, 480, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
    let dynamic: &mut dyn Surface = &mut surface;
    let mut w = EmergencyWriter::new(dynamic, &info);
    let (file, line, msg) = ("kernel/src/main.rs", 42, "oops →");
    write!(w, "panicked at {file}:{line}: {msg}").unwrap();
    let line = read_screen(&pixels, &info, 60, 4).remove(0);
    assert_eq!(line, "panicked at kernel/src/main.rs:42: oops \u{FFFD}");
}

#[test]
fn short_screens_get_a_shorter_band() {
    // Two text rows high: the band is two rows, and the bottom margin of
    // 5 pixels is not part of it.
    let info = info(64, 37, 64, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
    emergency_message(&mut surface, &info, &[b'm'; 100]);
    assert_eq!(read_screen(&pixels, &info, 8, 2), ["mmmmmmmm", "mmmmmmm>"]);
    assert!(pixels[32 * 64..].iter().all(|&p| p == CANARY));
}

#[test]
fn screens_smaller_than_a_cell_are_left_alone() {
    for (w, h) in [(7, 16), (8, 15), (1, 1)] {
        let info = info(w, h, w + 1, PixelFormat::Rgbx8);
        let mut pixels = buffer(&info);
        let mut surface = SliceSurface::new(&mut pixels, &info).unwrap();
        let mut writer = EmergencyWriter::new(&mut surface, &info);
        writer.write_bytes(b"panic");
        write!(writer, "{}", 1).unwrap();
        assert!(writer.truncated());
        assert!(pixels.iter().all(|&p| p == CANARY), "{w}x{h}");
    }
}
