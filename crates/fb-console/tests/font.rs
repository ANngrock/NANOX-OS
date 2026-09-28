//! Font data: golden rendering, pinned hash, design invariants, and
//! agreement with the drawings in `font/gen_font.py`.

mod common;

use common::*;
use fb_console::font::{self, GLYPH_HEIGHT, GLYPH_WIDTH, REPLACEMENT_GLYPH};
use fb_console::{PixelFormat, Scroll};

/// Renders `text` on one console row and returns it as ASCII art, one
/// string per pixel row ('#' lit, '.' background).
fn render_art(text: &str) -> Vec<String> {
    let cols = text.chars().count();
    let info = info(
        (cols * GLYPH_WIDTH) as u32,
        GLYPH_HEIGHT as u32,
        (cols * GLYPH_WIDTH) as u32,
        PixelFormat::Rgbx8,
    );
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 64, 1> = console(&mut pixels, info, Scroll::Redraw);
    core::fmt::Write::write_str(&mut con, text).unwrap();
    (0..GLYPH_HEIGHT)
        .map(|y| {
            (0..cols * GLYPH_WIDTH)
                .map(|x| {
                    if px(&pixels, &info, x, y) == 0 {
                        '.'
                    } else {
                        '#'
                    }
                })
                .collect()
        })
        .collect()
}

#[test]
fn golden_rendering_of_a_short_string() {
    let art = render_art("Nx0?");
    let expected = [
        "................................",
        "................................",
        "................................",
        "#.....#...........###....#####..",
        "##....#..........#...#..#.....#.",
        "##....#.........#.....#.......#.",
        "#.#...#.#.....#.#....##.......#.",
        "#..#..#..#...#..#...#.#......#..",
        "#...#.#...#.#...#..#..#.....#...",
        "#....##....#....#.#...#....#....",
        "#....##...#.#...##....#....#....",
        "#.....#..#...#...#...#..........",
        "#.....#.#.....#...###......#....",
        "................................",
        "................................",
        "................................",
    ];
    assert_eq!(art, expected, "\n{}", art.join("\n"));
}

/// FNV-1a over every glyph, printable ones in order then the replacement.
fn font_hash() -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    let glyphs = (0x20..=0x7E).map(font::glyph).chain([REPLACEMENT_GLYPH]);
    for glyph in glyphs {
        for &b in glyph {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    h
}

#[test]
fn font_data_hash_is_pinned() {
    // Update deliberately when the drawings change.
    assert_eq!(
        font_hash(),
        0xBA42_F83C_A15A_2BBB,
        "font hash {:#018x}",
        font_hash()
    );
}

#[test]
fn glyph_design_invariants() {
    let printable: Vec<&[u8; 16]> = (0x20..=0x7E).map(font::glyph).collect();
    for (i, g) in printable.iter().enumerate() {
        let byte = 0x20 + i;
        // Row 0 empty (text recognition relies on it), column 7 empty so
        // neighbours never touch.
        assert_eq!(g[0], 0, "0x{byte:02X} row 0");
        assert!(g.iter().all(|row| row & 1 == 0), "0x{byte:02X} column 7");
        assert_eq!(
            g.iter().all(|&row| row == 0),
            byte == 0x20,
            "0x{byte:02X} emptiness"
        );
        assert_ne!(*g, REPLACEMENT_GLYPH, "0x{byte:02X} equals the replacement");
        for (j, other) in printable.iter().enumerate().skip(i + 1) {
            assert_ne!(
                g,
                other,
                "0x{byte:02X} and 0x{:02X} are identical",
                0x20 + j
            );
        }
    }
    assert_eq!(REPLACEMENT_GLYPH[0], 0);
}

#[test]
fn lookup_maps_everything_outside_printable_ascii_to_the_replacement() {
    for b in (0x00..0x20).chain(0x7F..=0xFF) {
        assert_eq!(font::glyph(b), REPLACEMENT_GLYPH, "byte {b:#04x}");
        assert!(!font::is_printable(b));
    }
    assert_eq!(font::glyph_for_char('A'), font::glyph(b'A'));
    assert_eq!(font::glyph_for_char('~'), font::glyph(b'~'));
    for c in ['\0', '\u{7F}', 'é', '→', '\u{10FFFF}'] {
        assert_eq!(font::glyph_for_char(c), REPLACEMENT_GLYPH, "{c:?}");
    }
}

/// Parses the glyph drawings from the generator script.
fn drawings() -> Vec<(Option<u8>, [u8; 16])> {
    let src = include_str!("../font/gen_font.py");
    let mut out = Vec::new();
    let mut current: Option<(Option<u8>, [u8; 16])> = None;
    for line in src.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("0x") {
            let code = u8::from_str_radix(&rest[..2], 16).unwrap();
            if rest.ends_with("\"\",") {
                out.push((Some(code), [0; 16]));
            } else {
                current = Some((Some(code), [0; 16]));
            }
            continue;
        }
        if t == "REPLACEMENT = \"\"\"" {
            current = Some((None, [0; 16]));
            continue;
        }
        if t.starts_with("\"\"\"") {
            if let Some(done) = current.take() {
                out.push(done);
            }
            continue;
        }
        if let Some((_, rows)) = current.as_mut() {
            let mut parts = t.split_whitespace();
            let (Some(range), Some(pattern)) = (parts.next(), parts.next()) else {
                continue;
            };
            let (lo, hi) = match range.split_once('-') {
                Some((a, b)) => (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()),
                None => {
                    let r = range.parse::<usize>().unwrap();
                    (r, r)
                }
            };
            let value = pattern
                .bytes()
                .fold(0u8, |acc, c| (acc << 1) | u8::from(c == b'#'));
            for row in &mut rows[lo..=hi] {
                *row = value;
            }
        }
    }
    out
}

#[test]
fn generated_data_matches_the_drawings() {
    let drawn = drawings();
    assert_eq!(drawn.len(), 96, "95 printable glyphs plus the replacement");
    for (code, rows) in drawn {
        match code {
            Some(b) => assert_eq!(font::glyph(b), &rows, "glyph 0x{b:02X}"),
            None => assert_eq!(REPLACEMENT_GLYPH, &rows, "replacement glyph"),
        }
    }
}

/// Run with `--nocapture` to eyeball the font.
#[test]
fn print_sample() {
    for text in ["Hello, NANOX!", "0123456789 {}[]<>", "Quick jpg: @#$%&*"] {
        let art = render_art(text);
        assert_eq!(art.len(), GLYPH_HEIGHT);
        println!("{text}");
        for row in art {
            println!("{}", row.replace('.', " "));
        }
    }
}
