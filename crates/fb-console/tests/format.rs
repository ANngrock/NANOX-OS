//! Framebuffer descriptions: exact pixel values per format and every
//! rejection rule.

mod common;

use common::*;
use fb_console::font::{self, GLYPH_HEIGHT, GLYPH_WIDTH};
use fb_console::{
    Attr, Channel, Color, Console, Error, FramebufferDesc, PixelFormat, PixelMasks, Scroll,
    SliceSurface,
};

const FG: Color = Color::new(0x12, 0x34, 0x56);
const BG: Color = Color::new(0xAB, 0xCD, 0xEF);

/// Draws 'A' with palette entries set to FG/BG and checks every pixel of
/// the cell against hand-computed raw values.
fn assert_cell_pixels(format: PixelFormat, fg_raw: u32, bg_raw: u32) {
    let info = info(16, 16, 16, format);
    let mut pixels = buffer(&info);
    let mut con: Con<'_, 2, 1> = console(&mut pixels, info, Scroll::Redraw);
    con.set_palette(1, FG);
    con.set_palette(2, BG);
    con.set_attr(Attr::new(1, 2));
    con.write_bytes(b"A");
    let glyph = font::glyph(b'A');
    for y in 0..GLYPH_HEIGHT {
        for x in 0..GLYPH_WIDTH {
            let lit = glyph[y] & (0x80 >> x) != 0;
            let want = if lit { fg_raw } else { bg_raw };
            assert_eq!(pixels[y * 16 + x], want, "{format:?} pixel ({x}, {y})");
        }
    }
    // The untouched second cell is the normal background (black).
    assert_eq!(pixels[8], 0);
    assert_canaries(&pixels, &info);
}

#[test]
fn rgbx8_stores_red_in_the_lowest_byte() {
    assert_cell_pixels(PixelFormat::Rgbx8, 0x0056_3412, 0x00EF_CDAB);
}

#[test]
fn bgrx8_stores_blue_in_the_lowest_byte() {
    assert_cell_pixels(PixelFormat::Bgrx8, 0x0012_3456, 0x00AB_CDEF);
}

#[test]
fn bitmask_equivalent_to_bgrx8() {
    let masks = PixelMasks {
        red: 0x00FF_0000,
        green: 0x0000_FF00,
        blue: 0x0000_00FF,
        reserved: 0xFF00_0000,
    };
    assert_cell_pixels(PixelFormat::Bitmask(masks), 0x0012_3456, 0x00AB_CDEF);
}

#[test]
fn bitmask_with_reserved_in_the_low_byte() {
    let masks = PixelMasks {
        red: 0xFF00_0000,
        green: 0x00FF_0000,
        blue: 0x0000_FF00,
        reserved: 0x0000_00FF,
    };
    assert_cell_pixels(PixelFormat::Bitmask(masks), 0x1234_5600, 0xABCD_EF00);
}

#[test]
fn bitmask_ten_bit_channels_scale_with_rounding() {
    // 2:10:10:10 layout, red in the low bits.
    let masks = PixelMasks {
        red: 0x0000_03FF,
        green: 0x000F_FC00,
        blue: 0x3FF0_0000,
        reserved: 0xC000_0000,
    };
    let info = info(8, 16, 8, PixelFormat::Bitmask(masks));
    assert_eq!(info.encode(Color::new(255, 0, 0)), 0x0000_03FF);
    assert_eq!(info.encode(Color::new(0, 255, 0)), 0x000F_FC00);
    assert_eq!(info.encode(Color::new(0, 0, 255)), 0x3FF0_0000);
    assert_eq!(info.encode(Color::WHITE), 0x3FFF_FFFF);
    assert_eq!(info.encode(Color::BLACK), 0);
    // round(128 * 1023 / 255) = 514
    assert_eq!(info.encode(Color::new(128, 0, 0)), 514);
    // FG = (0x12, 0x34, 0x56): round(v * 1023 / 255) = 72, 209, 345.
    assert_cell_pixels(
        PixelFormat::Bitmask(masks),
        72 | (209 << 10) | (345 << 20),
        // BG = (0xAB, 0xCD, 0xEF) -> 686, 822, 959.
        686 | (822 << 10) | (959 << 20),
    );
}

#[test]
fn from_gop_maps_every_defined_value() {
    let masks = PixelMasks {
        red: 1,
        green: 2,
        blue: 4,
        reserved: 0,
    };
    assert_eq!(PixelFormat::from_gop(0, masks), Ok(PixelFormat::Rgbx8));
    assert_eq!(PixelFormat::from_gop(1, masks), Ok(PixelFormat::Bgrx8));
    assert_eq!(
        PixelFormat::from_gop(2, masks),
        Ok(PixelFormat::Bitmask(masks))
    );
    assert_eq!(PixelFormat::from_gop(3, masks), Ok(PixelFormat::BltOnly));
    assert_eq!(
        PixelFormat::from_gop(4, masks),
        Err(Error::UnknownFormat(4))
    );
    assert_eq!(
        PixelFormat::from_gop(u32::MAX, masks),
        Err(Error::UnknownFormat(u32::MAX))
    );
}

#[test]
fn blt_only_is_rejected() {
    assert_eq!(
        desc(1280, 800, 1280, PixelFormat::BltOnly).validate(),
        Err(Error::BltOnly)
    );
}

#[test]
fn zero_dimensions_are_rejected() {
    for (w, h, s) in [(0, 800, 1280), (1280, 0, 1280), (0, 0, 0)] {
        assert_eq!(
            desc(w, h, s, PixelFormat::Rgbx8).validate(),
            Err(Error::ZeroDimension),
            "{w}x{h} stride {s}"
        );
    }
    let zero_stride = FramebufferDesc {
        stride: 0,
        ..desc(1280, 800, 1280, PixelFormat::Rgbx8)
    };
    assert_eq!(zero_stride.validate(), Err(Error::ZeroDimension));
}

#[test]
fn stride_below_width_is_rejected() {
    let d = FramebufferDesc {
        stride: 1279,
        ..desc(1280, 800, 1280, PixelFormat::Bgrx8)
    };
    assert_eq!(
        d.validate(),
        Err(Error::StrideTooSmall {
            width: 1280,
            stride: 1279
        })
    );
    // Equal is fine, larger is fine.
    assert!(desc(1280, 800, 1280, PixelFormat::Bgrx8).validate().is_ok());
    assert!(desc(1280, 800, 1344, PixelFormat::Bgrx8).validate().is_ok());
}

#[test]
fn size_overflow_is_rejected() {
    let d = FramebufferDesc {
        width: u32::MAX,
        height: u32::MAX,
        stride: u32::MAX,
        format: PixelFormat::Rgbx8,
        size_bytes: u64::MAX,
    };
    assert_eq!(d.validate(), Err(Error::SizeOverflow));
    // stride * height fits in u64 but not after * 4.
    let d = FramebufferDesc {
        width: 1,
        height: u32::MAX,
        stride: u32::MAX,
        format: PixelFormat::Rgbx8,
        size_bytes: u64::MAX,
    };
    assert_eq!(d.validate(), Err(Error::SizeOverflow));
}

#[test]
fn buffer_smaller_than_stride_times_height_is_rejected() {
    let exact = desc(1920, 1080, 1920, PixelFormat::Bgrx8);
    assert_eq!(exact.size_bytes, 1920 * 1080 * 4);
    assert!(exact.validate().is_ok());
    let short = FramebufferDesc {
        size_bytes: exact.size_bytes - 1,
        ..exact
    };
    assert_eq!(
        short.validate(),
        Err(Error::BufferTooSmall {
            required: exact.size_bytes,
            actual: exact.size_bytes - 1
        })
    );
    // Stride padding counts: 1920 wide with stride 2048 needs 2048 * 1080 * 4.
    let padded = FramebufferDesc {
        stride: 2048,
        ..exact
    };
    assert_eq!(
        padded.validate(),
        Err(Error::BufferTooSmall {
            required: 2048 * 1080 * 4,
            actual: exact.size_bytes
        })
    );
}

fn masks_error(red: u32, green: u32, blue: u32, reserved: u32) -> Error {
    let masks = PixelMasks {
        red,
        green,
        blue,
        reserved,
    };
    desc(64, 64, 64, PixelFormat::Bitmask(masks))
        .validate()
        .expect_err("masks must be rejected")
}

#[test]
fn empty_colour_masks_are_rejected() {
    assert_eq!(
        masks_error(0, 0xFF00, 0xFF_0000, 0xFF00_0000),
        Error::EmptyMask(Channel::Red)
    );
    assert_eq!(
        masks_error(0xFF, 0, 0xFF_0000, 0xFF00_0000),
        Error::EmptyMask(Channel::Green)
    );
    assert_eq!(
        masks_error(0xFF, 0xFF00, 0, 0xFF00_0000),
        Error::EmptyMask(Channel::Blue)
    );
    // An empty reserved mask is legal when the colours reach bit 24+.
    let masks = PixelMasks {
        red: 0x0000_03FF,
        green: 0x000F_FC00,
        blue: 0x3FF0_0000,
        reserved: 0,
    };
    assert!(desc(64, 64, 64, PixelFormat::Bitmask(masks))
        .validate()
        .is_ok());
}

#[test]
fn non_contiguous_masks_are_rejected() {
    assert_eq!(
        masks_error(0b1010, 0xFF00, 0xFF_0000, 0xFF00_0000),
        Error::NonContiguousMask(Channel::Red)
    );
    assert_eq!(
        masks_error(0xFF, 0xF0F0_0000 >> 8, 0xF, 0xFF00_0000),
        Error::NonContiguousMask(Channel::Green)
    );
    assert_eq!(
        masks_error(0xFF, 0xFF00, 0xFF_0000, 0x8100_0000),
        Error::NonContiguousMask(Channel::Reserved)
    );
}

#[test]
fn overlapping_masks_are_rejected() {
    assert_eq!(
        masks_error(0x1FF, 0xFF00, 0xFF_0000, 0xFF00_0000),
        Error::OverlappingMasks(Channel::Red, Channel::Green)
    );
    assert_eq!(
        masks_error(0xFF, 0xFF00, 0x1FF_0000, 0xFF00_0000),
        Error::OverlappingMasks(Channel::Blue, Channel::Reserved)
    );
    assert_eq!(
        masks_error(0xFF, 0xFF00, 0xFF, 0xFF00_0000),
        Error::OverlappingMasks(Channel::Red, Channel::Blue)
    );
}

#[test]
fn pixels_other_than_four_bytes_are_rejected() {
    // RGB565 in the low 16 bits.
    assert_eq!(
        masks_error(0xF800, 0x07E0, 0x001F, 0),
        Error::UnsupportedPixelSize { bits: 16 }
    );
    // Packed 24-bit RGB.
    assert_eq!(
        masks_error(0xFF_0000, 0xFF00, 0xFF, 0),
        Error::UnsupportedPixelSize { bits: 24 }
    );
}

#[test]
fn slice_shorter_than_the_framebuffer_is_rejected() {
    let info = info(64, 32, 80, PixelFormat::Rgbx8);
    let mut short = vec![0u32; 80 * 32 - 1];
    assert_eq!(
        SliceSurface::new(&mut short, &info).err(),
        Some(Error::SurfaceTooSmall {
            required: 80 * 32,
            actual: 80 * 32 - 1
        })
    );
    // The last row does not need to be followed by padding in memory terms,
    // but the slice model requires stride * height pixels.
    let mut exact = vec![0u32; 80 * 32];
    assert!(SliceSurface::new(&mut exact, &info).is_ok());
}

#[test]
fn framebuffer_smaller_than_one_cell_is_rejected() {
    for (w, h) in [(7, 16), (8, 15), (1, 1)] {
        let info = info(w, h, w, PixelFormat::Rgbx8);
        let mut pixels = buffer(&info);
        let surface = SliceSurface::new(&mut pixels, &info).unwrap();
        assert_eq!(
            Console::<_, 80, 25>::new(surface, info, Scroll::Redraw, leaked_text()).err(),
            Some(Error::TooSmallForCell),
            "{w}x{h}"
        );
        // Nothing was drawn.
        assert!(pixels.iter().all(|&p| p == CANARY));
    }
    // Exactly one cell works.
    let info = info(8, 16, 8, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let con: Con<'_, 80, 25> = console(&mut pixels, info, Scroll::Redraw);
    assert_eq!((con.cols(), con.rows()), (1, 1));
}

#[test]
fn zero_text_capacity_is_rejected() {
    let info = info(64, 64, 64, PixelFormat::Rgbx8);
    let mut pixels = buffer(&info);
    let surface = SliceSurface::new(&mut pixels, &info).unwrap();
    assert_eq!(
        Console::<_, 0, 4>::new(surface, info, Scroll::Redraw, leaked_text()).err(),
        Some(Error::ZeroTextCapacity)
    );
    let surface = SliceSurface::new(&mut pixels, &info).unwrap();
    assert_eq!(
        Console::<_, 4, 0>::new(surface, info, Scroll::Redraw, leaked_text()).err(),
        Some(Error::ZeroTextCapacity)
    );
}

#[test]
fn reference_resolutions_give_expected_grids() {
    // Laptop panel: 1080 = 67 * 16 + 8, so 8 pixel rows of margin.
    let laptop = info(1920, 1080, 1920, PixelFormat::Bgrx8);
    assert_eq!((laptop.cell_cols(), laptop.cell_rows()), (240, 67));
    assert_eq!(laptop.pixel_len(), 1920 * 1080);
    // Typical OVMF mode.
    let ovmf = info(1280, 800, 1280, PixelFormat::Bgrx8);
    assert_eq!((ovmf.cell_cols(), ovmf.cell_rows()), (160, 50));
}

#[test]
fn errors_have_messages() {
    let all = [
        Error::ZeroDimension,
        Error::StrideTooSmall {
            width: 2,
            stride: 1,
        },
        Error::SizeOverflow,
        Error::BufferTooSmall {
            required: 2,
            actual: 1,
        },
        Error::BltOnly,
        Error::UnknownFormat(9),
        Error::EmptyMask(Channel::Red),
        Error::NonContiguousMask(Channel::Blue),
        Error::OverlappingMasks(Channel::Red, Channel::Green),
        Error::UnsupportedPixelSize { bits: 16 },
        Error::TooSmallForCell,
        Error::ZeroTextCapacity,
        Error::SurfaceTooSmall {
            required: 2,
            actual: 1,
        },
    ];
    for e in all {
        assert!(!e.to_string().is_empty());
    }
}
