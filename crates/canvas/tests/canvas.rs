use canvas::*;
use fb_console::font::{glyph_for_char, GLYPH_HEIGHT, GLYPH_WIDTH};
use fb_console::{FramebufferDesc, FramebufferInfo, PixelFormat, SliceSurface};

const BG: u32 = 0x0011_2233;
const FG: u32 = 0x00AA_BBCC;

fn info(w: u32, h: u32, stride: u32) -> FramebufferInfo {
    FramebufferDesc {
        width: w,
        height: h,
        stride,
        format: PixelFormat::Bgrx8,
        size_bytes: u64::from(stride) * u64::from(h) * 4,
    }
    .validate()
    .unwrap()
}

/// A `w` × `h` screen (stride `w + 3`) filled with `BG`, drawn on by `f`; returns its pixels by row.
fn screen(w: u32, h: u32, f: impl FnOnce(&mut SliceSurface<'_>)) -> Vec<Vec<u32>> {
    let stride = w + 3;
    let mut px = vec![BG; (stride * h) as usize];
    let i = info(w, h, stride);
    let mut s = SliceSurface::new(&mut px, &i).unwrap();
    f(&mut s);
    (0..h as usize)
        .map(|y| px[y * stride as usize..y * stride as usize + w as usize].to_vec())
        .collect()
}

#[test]
fn a_view_needs_its_rows_inside_the_pixels() {
    let px = [0u32; 11];
    assert!(View::new(&px, 3, 3, 4).is_some(), "4 + 4 + 3 = 11");
    assert!(View::new(&px[..10], 3, 3, 4).is_none());
    assert!(View::new(&px[..9], 3, 3, 3).is_some());
    assert!(View::new(&px[..8], 3, 3, 3).is_none());
    assert!(
        View::new(&px, 5, 2, 4).is_none(),
        "row longer than the stride"
    );
    assert!(View::new(&[], 0, 0, 0).is_some());
    assert!(
        View::new(&px, 1, usize::MAX, usize::MAX).is_none(),
        "overflow"
    );
    let v = View::new(&px, 2, 3, 4).unwrap();
    assert_eq!((v.width(), v.height(), v.row(1).len()), (2, 3, 2));
}

#[test]
fn text_draws_the_font_at_any_position_and_scale() {
    for scale in [1usize, 2, 3] {
        let (x, y) = (5usize, 3usize);
        let rows = screen(60, 60, |s| {
            assert_eq!(
                text(s, x, y, "Az", FG, None, scale),
                2 * GLYPH_WIDTH * scale
            );
        });
        for (i, c) in "Az".chars().enumerate() {
            let g = glyph_for_char(c);
            for py in 0..GLYPH_HEIGHT * scale {
                for px in 0..GLYPH_WIDTH * scale {
                    let on = g[py / scale] & (0x80 >> (px / scale)) != 0;
                    let got = rows
                        .get(y + py)
                        .and_then(|r| r.get(x + i * GLYPH_WIDTH * scale + px));
                    if let Some(&got) = got {
                        assert_eq!(
                            got,
                            if on { FG } else { BG },
                            "scale {scale} {c} ({px},{py})"
                        );
                    }
                }
            }
        }
        // nothing outside the cells
        for (py, r) in rows.iter().enumerate() {
            for (px, &p) in r.iter().enumerate() {
                let inside = px >= x
                    && px < x + 2 * GLYPH_WIDTH * scale
                    && py >= y
                    && py < y + GLYPH_HEIGHT * scale;
                if !inside {
                    assert_eq!(p, BG, "scale {scale} ({px},{py})");
                }
            }
        }
    }
    assert_eq!(text_width("Az", 3), 48);
    assert_eq!(text_width("", 3), 0);
}

#[test]
fn every_printable_character_is_its_glyph() {
    let all: String = (0x20u8..=0x7E).map(char::from).collect();
    let n = all.len();
    let rows = screen((n * GLYPH_WIDTH) as u32, GLYPH_HEIGHT as u32, |s| {
        text(s, 0, 0, &all, FG, None, 1);
    });
    for (i, c) in all.chars().enumerate() {
        let g = glyph_for_char(c);
        for (py, row) in rows.iter().enumerate() {
            for px in 0..GLYPH_WIDTH {
                let on = g[py] & (0x80 >> px) != 0;
                let want = if on { FG } else { BG };
                assert_eq!(row[i * GLYPH_WIDTH + px], want, "{c:?} ({px},{py})");
            }
        }
    }
}

#[test]
fn text_with_a_background_fills_its_cells_and_scale_0_is_1() {
    let rows = screen(30, 20, |s| {
        text(s, 1, 2, " ", FG, Some(0x0000_0001), 0);
    });
    for (py, r) in rows.iter().enumerate() {
        for (px, &p) in r.iter().enumerate() {
            let cell = (1..1 + GLYPH_WIDTH).contains(&px) && (2..2 + GLYPH_HEIGHT).contains(&py);
            assert_eq!(p, if cell { 1 } else { BG }, "({px},{py})");
        }
    }
    // a character outside ASCII is the replacement glyph
    let a = screen(20, 20, |s| {
        text(s, 0, 0, "Ж", FG, None, 1);
    });
    let b = screen(20, 20, |s| {
        text(s, 0, 0, "\u{1}", FG, None, 1);
    });
    assert_eq!(a, b);
    assert_ne!(a, screen(20, 20, |_| {}));
}

#[test]
fn text_and_frames_are_clipped_at_the_edges() {
    let rows = screen(10, 10, |s| {
        text(s, 6, 4, "WW", FG, Some(1), 2);
        text(s, usize::MAX - 3, usize::MAX - 3, "x", FG, None, usize::MAX);
        frame(s, 8, 8, 100, 100, 5, 2);
    });
    assert_eq!(rows.len(), 10);
    assert!(rows.iter().all(|r| r.len() == 10));
}

#[test]
fn a_frame_lines_the_inside_of_its_rectangle() {
    let rows = screen(12, 10, |s| frame(s, 2, 1, 7, 6, 2, FG));
    for (py, r) in rows.iter().enumerate() {
        for (px, &p) in r.iter().enumerate() {
            let inside = (2..9).contains(&px) && (1..7).contains(&py);
            let border = inside && !((4..7).contains(&px) && (3..5).contains(&py));
            assert_eq!(p, if border { FG } else { BG }, "({px},{py})");
        }
    }
    // thicker than half: filled
    let rows = screen(12, 10, |s| frame(s, 2, 1, 4, 3, 9, FG));
    let filled = rows.iter().flatten().filter(|&&p| p == FG).count();
    assert_eq!(filled, 12);
}

#[test]
fn the_same_size_is_a_copy() {
    let src: Vec<u32> = (0..6 * 4).map(|i| 0x0101_0101 * i).collect();
    let v = View::new(&src, 5, 4, 6).unwrap();
    let rows = screen(9, 7, |s| blit_scaled(s, &v, 2, 1, 5, 4));
    for y in 0..4 {
        assert_eq!(&rows[y + 1][2..7], v.row(y));
    }
    assert_eq!(rows[0], vec![BG; 9]);
    assert_eq!(rows[5], vec![BG; 9]);
    assert_eq!(rows[1][..2], [BG, BG]);
    assert_eq!(rows[1][7..], [BG, BG]);
}

#[test]
fn shrinking_averages_every_byte_and_rounds_to_nearest() {
    // 4 x 2 source to 2 x 1: each destination pixel averages a 2 x 2 block.
    let src = [
        0x00_00_00_00,
        0x04_02_01_FF,
        0x10_00_00_00,
        0x10_00_00_00,
        0x00_00_00_02,
        0x00_00_00_FF,
        0x10_00_00_01,
        0x10_00_00_00,
    ];
    let v = View::new(&src, 4, 2, 4).unwrap();
    let rows = screen(2, 1, |s| blit_scaled(s, &v, 0, 0, 2, 1));
    // byte 0: (0 + 255 + 2 + 255) / 4 = 128; byte 1: 1/4 rounds to 0; byte 2: 2/4 rounds to 1 (half up)
    assert_eq!(rows[0][0], 0x01_01_00_80);
    assert_eq!(rows[0][1], 0x10_00_00_00, "1/4 rounds down");
    // a ratio that is not whole: 3 to 2 covers [0, 1) and [1, 3)
    let src = [0x0000_0030, 0x0000_0010, 0x0000_0020];
    let v = View::new(&src, 3, 1, 3).unwrap();
    let rows = screen(2, 1, |s| blit_scaled(s, &v, 0, 0, 2, 1));
    assert_eq!(rows[0], [0x30, 0x18]);
}

#[test]
fn enlarging_repeats_pixels() {
    let src = [1u32, 2, 3, 4];
    let v = View::new(&src, 2, 2, 2).unwrap();
    let rows = screen(4, 4, |s| blit_scaled(s, &v, 0, 0, 4, 4));
    assert_eq!(
        rows,
        [[1, 1, 2, 2], [1, 1, 2, 2], [3, 3, 4, 4], [3, 3, 4, 4]]
    );
}

#[test]
fn a_wide_destination_is_written_in_chunks_and_clipped() {
    // wider than one chunk of 256 pixels, and partly off the surface
    let src: Vec<u32> = (0..300u32).collect();
    let v = View::new(&src, 300, 1, 300).unwrap();
    let rows = screen(400, 3, |s| blit_scaled(s, &v, 50, 1, 600, 2));
    for row in &rows[1..3] {
        let want: Vec<u32> = (0..350).map(|i| i / 2).collect();
        assert_eq!(&row[50..], &want[..]);
    }
    assert_eq!(rows[0], vec![BG; 400]);
    // a source one pixel wide is drawn like any other
    let one = View::new(&[7], 1, 1, 1).unwrap();
    let rows = screen(4, 3, |s| blit_scaled(s, &one, 1, 1, 3, 2));
    assert_eq!(rows, [vec![BG; 4], vec![BG, 7, 7, 7], vec![BG, 7, 7, 7]]);
    // empty source (no rows, or rows of no pixels) or destination: nothing
    let empty = View::new(&[], 0, 0, 0).unwrap();
    let narrow = View::new(&[], 0, 3, 0).unwrap();
    let rows = screen(4, 4, |s| {
        blit_scaled(s, &empty, 0, 0, 4, 4);
        blit_scaled(s, &narrow, 0, 0, 4, 4);
        blit_scaled(s, &v, 0, 0, 0, 4);
        blit_scaled(s, &v, 0, 0, 4, 0);
    });
    assert!(rows.iter().flatten().all(|&p| p == BG));
}
