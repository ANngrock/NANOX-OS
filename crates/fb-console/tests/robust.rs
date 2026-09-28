//! Hostile and oversized input: no panic, no write outside the visible
//! framebuffer (canaries in stride padding and past the end of the slice),
//! and the screen always equals the text buffer.

mod common;

use core::fmt::Write as _;

use common::*;
use fb_console::{
    emergency_message, Attr, Color, FramebufferInfo, PixelFormat, PixelMasks, Scroll,
};

const MODES: [Scroll; 2] = [Scroll::CopyRows, Scroll::Redraw];

/// Screen sizes from one cell up, with odd margins and stride padding.
fn odd_sizes() -> Vec<FramebufferInfo> {
    [
        (8, 16, 8),
        (9, 17, 12),
        (15, 31, 15),
        (64, 48, 70),
        (100, 70, 131),
    ]
    .into_iter()
    .map(|(w, h, s)| info(w, h, s, PixelFormat::Rgbx8))
    .collect()
}

#[test]
fn random_bytes_on_odd_sizes() {
    for (i, info) in odd_sizes().into_iter().enumerate() {
        let mut results = Vec::new();
        for scroll in MODES {
            let mut rng = Rng::new(0x5EED + i as u64);
            let mut pixels = buffer(&info);
            let mut con: Con<'_, 16, 8> = console(&mut pixels, info, scroll);
            let mut left = 40_000;
            while left > 0 {
                let n = (rng.below(700) as usize + 1).min(left);
                con.write_bytes(&rng.bytes(n));
                left -= n;
                if rng.below(4) == 0 {
                    con.set_attr(Attr::new(rng.next_u64() as u8, rng.next_u64() as u8));
                }
            }
            assert_matches_buffer(&con);
            assert_canaries(&pixels, &info);
            results.push(pixels);
        }
        assert_eq!(results[0], results[1], "scroll modes disagree on {info:?}");
    }
}

#[test]
fn random_operation_sequences() {
    let masks = PixelMasks {
        red: 0x3FF0_0000,
        green: 0x000F_FC00,
        blue: 0x0000_03FF,
        reserved: 0xC000_0000,
    };
    let info = info(203, 101, 211, PixelFormat::Bitmask(masks));
    for scroll in MODES {
        let mut rng = Rng::new(0xC0FFEE);
        let mut pixels = buffer(&info);
        let mut con: Con<'_, 32, 8> = console(&mut pixels, info, scroll);
        assert_eq!((con.cols(), con.rows()), (25, 6));
        for step in 0..3000 {
            match rng.below(9) {
                0..=2 => {
                    let n = rng.below(300) as usize;
                    con.write_bytes(&rng.bytes(n));
                }
                3 => {
                    let s: String = (0..rng.below(80))
                        .filter_map(|_| char::from_u32(rng.next_u64() as u32 % 0x11_0000))
                        .collect();
                    con.write_str(&s).unwrap();
                }
                4 => con.set_attr(Attr::new(rng.next_u64() as u8, rng.next_u64() as u8)),
                5 => con.set_cursor(rng.next_u64() as usize, rng.next_u64() as usize),
                6 => {
                    let c = rng.next_u64();
                    con.set_palette(
                        c as u8,
                        Color::new((c >> 8) as u8, (c >> 16) as u8, (c >> 24) as u8),
                    );
                    con.redraw_all();
                }
                7 => {
                    let n = rng.below(2000) as usize;
                    con.emergency(&rng.bytes(n));
                    // The emergency band deliberately overwrites the screen.
                    con.redraw_all();
                }
                _ => {
                    if rng.below(10) == 0 {
                        con.clear();
                    }
                }
            }
            let (col, row) = con.cursor();
            assert!(
                col <= con.cols() && row < con.rows(),
                "cursor ({col}, {row})"
            );
            if step % 250 == 0 {
                assert_matches_buffer(&con);
            }
        }
        assert_matches_buffer(&con);
        assert_canaries(&pixels, &info);
    }
}

#[test]
fn every_byte_value_is_accepted() {
    let info = info(128, 64, 130, PixelFormat::Bgrx8);
    for scroll in MODES {
        let mut pixels = buffer(&info);
        let mut con: Con<'_, 16, 4> = console(&mut pixels, info, scroll);
        let all: Vec<u8> = (0..=255).collect();
        con.write_bytes(&all);
        assert_matches_buffer(&con);
        assert_canaries(&pixels, &info);
    }
}

#[test]
fn one_mebibyte_line_on_a_tiny_screen() {
    let info = info(64, 48, 64, PixelFormat::Rgbx8);
    let data: Vec<u8> = (0..1usize << 20).map(|i| b'!' + (i % 94) as u8).collect();
    for scroll in MODES {
        let mut pixels = buffer(&info);
        let mut con: Con<'_, 8, 3> = console(&mut pixels, info, scroll);
        con.write_bytes(&data);
        // 2^20 / 8 lines exactly: the cursor waits at the end of the last.
        assert_eq!(con.cursor(), (8, 2));
        assert_matches_buffer(&con);
        let expect: Vec<String> = (0..3)
            .map(|r| {
                let start = data.len() - (3 - r) * 8;
                String::from_utf8(data[start..start + 8].to_vec()).unwrap()
            })
            .collect();
        assert_eq!(read_screen(&pixels, &info, 8, 3), expect);
        assert_canaries(&pixels, &info);
    }
}

#[test]
fn huge_formatted_padding_on_a_tiny_screen() {
    let info = info(64, 48, 64, PixelFormat::Rgbx8);
    for scroll in MODES {
        let mut pixels = buffer(&info);
        let mut con: Con<'_, 8, 3> = console(&mut pixels, info, scroll);
        write!(con, "{:>60000}", "x").unwrap();
        assert_eq!(read_screen(&pixels, &info, 8, 3), ["", "", "       x"]);
        assert_canaries(&pixels, &info);
    }
}

#[test]
fn large_random_text_on_the_laptop_screen() {
    let info = info(1920, 1080, 1920, PixelFormat::Bgrx8);
    let mut rng = Rng::new(1080);
    let text: Vec<u8> = (0..48 * 1024)
        .map(|_| match rng.below(40) {
            0 => b'\n',
            1 => b'\t',
            _ => b' ' + rng.below(95) as u8,
        })
        .collect();
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 240, 67> = console(&mut pixels, info, Scroll::CopyRows);
    con.write_bytes(&text);
    assert_matches_buffer(&con);
    assert_canaries(&pixels, &info);
}

#[test]
fn emergency_on_any_size_and_input_stays_inside() {
    let mut rng = Rng::new(911);
    let mut sizes = odd_sizes();
    sizes.push(info(1, 1, 3, PixelFormat::Rgbx8));
    sizes.push(info(7, 15, 7, PixelFormat::Bgrx8));
    sizes.push(info(1280, 800, 1344, PixelFormat::Bgrx8));
    for info in sizes {
        for len in [0, 1, 100, 10_000, 1 << 20] {
            let mut pixels = buffer(&info);
            let mut surface = fb_console::SliceSurface::new(&mut pixels, &info).unwrap();
            emergency_message(&mut surface, &info, &rng.bytes(len));
            assert_canaries(&pixels, &info);
        }
    }
}

#[test]
fn slice_surface_clips_every_operation_to_the_visible_area() {
    use fb_console::{SliceSurface, Surface};
    let info = info(10, 6, 13, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let mut s = SliceSurface::new(&mut pixels, &info).unwrap();
    let row = [7u32; 40];
    for &(x, y) in &[
        (0, 0),
        (5, 2),
        (9, 5),
        (10, 0),
        (0, 6),
        (usize::MAX, 0),
        (0, usize::MAX),
    ] {
        s.write_pixel(x, y, 1);
        s.write_row(x, y, &row);
        s.fill(x, y, usize::MAX, usize::MAX, 2);
        s.fill(x, y, 3, 3, 3);
    }
    // Invalid copies are refused without effect; valid ones move only the
    // visible width.
    let snapshot = s.pixels().to_vec();
    assert!(!s.copy_rows(0, 1, 6));
    assert!(!s.copy_rows(usize::MAX, 0, 1));
    assert!(!s.copy_rows(0, 0, usize::MAX));
    assert_eq!(s.pixels(), &snapshot[..]);
    s.fill(0, 5, 10, 1, 9);
    assert!(s.copy_rows(0, 5, 1));
    assert!(s.copy_rows(2, 0, 4));
    assert_eq!(s.pixel(9, 2), Some(9));
    assert_eq!(s.pixel(10, 0), None);
    assert_canaries(&pixels, &info);
    assert_eq!(px(&pixels, &info, 0, 0), 9);
    assert_eq!(px(&pixels, &info, 9, 2), 9);
}
