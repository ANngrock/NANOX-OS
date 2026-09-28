//! Framebuffer description, validation and pixel encoding.
//!
//! The model follows the UEFI GOP mode information (UEFI 2.11, Graphics
//! Output Protocol): `HorizontalResolution`, `VerticalResolution`,
//! `PixelsPerScanLine`, `PixelFormat` with an optional `PixelBitmask`, and
//! `FrameBufferSize`.
//! Only 32-bit pixels are supported, which covers every GOP format except
//! `PixelBltOnly` (no linear framebuffer at all) and exotic bitmask layouts.

use core::fmt;

/// An 8-bit-per-channel colour; converted to the framebuffer layout by
/// [`FramebufferInfo::encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const BLACK: Self = Self::new(0, 0, 0);
    pub const WHITE: Self = Self::new(255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// `EFI_PIXEL_BITMASK`: which bits of a 32-bit pixel hold each channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PixelMasks {
    pub red: u32,
    pub green: u32,
    pub blue: u32,
    pub reserved: u32,
}

/// Pixel layout of the framebuffer. Byte order is memory order: `Rgbx8`
/// stores red in the lowest-addressed byte, i.e. `0x00BBGGRR` as a
/// little-endian `u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// `PixelRedGreenBlueReserved8BitPerColor`.
    Rgbx8,
    /// `PixelBlueGreenRedReserved8BitPerColor`.
    Bgrx8,
    /// `PixelBitMask` with its masks.
    Bitmask(PixelMasks),
    /// `PixelBltOnly`: no linear framebuffer. Always rejected.
    BltOnly,
}

impl PixelFormat {
    /// GOP `EFI_GRAPHICS_PIXEL_FORMAT` values.
    pub const GOP_RGBX8: u32 = 0;
    pub const GOP_BGRX8: u32 = 1;
    pub const GOP_BITMASK: u32 = 2;
    pub const GOP_BLT_ONLY: u32 = 3;

    /// Maps the raw GOP enumeration value. `masks` is used only for
    /// `PixelBitMask`. `PixelBltOnly` maps to [`PixelFormat::BltOnly`] so the
    /// caller gets the specific [`Error::BltOnly`] from validation.
    pub fn from_gop(code: u32, masks: PixelMasks) -> Result<Self, Error> {
        match code {
            Self::GOP_RGBX8 => Ok(Self::Rgbx8),
            Self::GOP_BGRX8 => Ok(Self::Bgrx8),
            Self::GOP_BITMASK => Ok(Self::Bitmask(masks)),
            Self::GOP_BLT_ONLY => Ok(Self::BltOnly),
            other => Err(Error::UnknownFormat(other)),
        }
    }
}

/// A colour channel of a pixel bitmask, for error reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Red,
    Green,
    Blue,
    Reserved,
}

/// Every way a framebuffer description or console setup can be rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Error {
    /// Width, height or stride is zero.
    ZeroDimension,
    /// Pixels per scan line is smaller than the visible width.
    StrideTooSmall { width: u32, stride: u32 },
    /// `stride * height * 4` does not fit in `u64` or `usize`.
    SizeOverflow,
    /// The reported buffer is smaller than `stride * height * 4` bytes.
    BufferTooSmall { required: u64, actual: u64 },
    /// GOP reported `PixelBltOnly`: there is no linear framebuffer.
    BltOnly,
    /// GOP pixel format value outside the specification.
    UnknownFormat(u32),
    /// A colour channel has an empty bitmask.
    EmptyMask(Channel),
    /// A bitmask has gaps between its set bits.
    NonContiguousMask(Channel),
    /// Two channel bitmasks share bits.
    OverlappingMasks(Channel, Channel),
    /// The bitmasks do not describe a 4-byte pixel (`bits` is the highest
    /// used bit position plus one).
    UnsupportedPixelSize { bits: u32 },
    /// The framebuffer cannot hold even one 8x16 character cell.
    TooSmallForCell,
    /// The console's const-generic text capacity has zero columns or rows.
    ZeroTextCapacity,
    /// A pixel slice is shorter than `stride * height` pixels.
    SurfaceTooSmall { required: usize, actual: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ZeroDimension => f.write_str("framebuffer width, height or stride is zero"),
            Self::StrideTooSmall { width, stride } => {
                write!(f, "stride {stride} is smaller than width {width}")
            }
            Self::SizeOverflow => f.write_str("framebuffer size overflows"),
            Self::BufferTooSmall { required, actual } => {
                write!(f, "framebuffer has {actual} bytes, needs {required}")
            }
            Self::BltOnly => f.write_str("GOP mode is BltOnly (no linear framebuffer)"),
            Self::UnknownFormat(code) => write!(f, "unknown GOP pixel format {code}"),
            Self::EmptyMask(ch) => write!(f, "{ch:?} mask is empty"),
            Self::NonContiguousMask(ch) => write!(f, "{ch:?} mask is not contiguous"),
            Self::OverlappingMasks(a, b) => write!(f, "{a:?} and {b:?} masks overlap"),
            Self::UnsupportedPixelSize { bits } => {
                write!(
                    f,
                    "bitmask pixel uses {bits} bits; only 4-byte pixels are supported"
                )
            }
            Self::TooSmallForCell => f.write_str("framebuffer is smaller than one 8x16 cell"),
            Self::ZeroTextCapacity => f.write_str("console text capacity is zero"),
            Self::SurfaceTooSmall { required, actual } => {
                write!(f, "pixel slice has {actual} pixels, needs {required}")
            }
        }
    }
}

/// Raw framebuffer description as reported by firmware. Nothing here is
/// trusted until [`FramebufferDesc::validate`] succeeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FramebufferDesc {
    /// Visible width in pixels (`HorizontalResolution`).
    pub width: u32,
    /// Visible height in pixels (`VerticalResolution`).
    pub height: u32,
    /// Pixels per scan line (`PixelsPerScanLine`), at least `width`.
    pub stride: u32,
    pub format: PixelFormat,
    /// Length of the framebuffer mapping in bytes (`FrameBufferSize`).
    pub size_bytes: u64,
}

/// Bytes per pixel; the only pixel size supported.
pub(crate) const BYTES_PER_PIXEL: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ChannelEncoding {
    shift: u32,
    bits: u32,
}

impl ChannelEncoding {
    const fn byte(index: u32) -> Self {
        Self {
            shift: index * 8,
            bits: 8,
        }
    }

    fn from_mask(mask: u32) -> Self {
        let shift = mask.trailing_zeros();
        Self {
            shift,
            bits: (mask >> shift).count_ones(),
        }
    }

    /// Scales an 8-bit value to the channel width with rounding, so 0 maps
    /// to 0 and 255 to all-ones. Validation guarantees `1 <= bits` and
    /// `shift + bits <= 32`; the checked shift keeps even a broken invariant
    /// from panicking.
    fn encode(self, value: u8) -> u32 {
        let max = (1u64 << self.bits.min(32)) - 1;
        let scaled = (u64::from(value) * max + 127) / 255;
        u32::try_from(scaled)
            .unwrap_or(u32::MAX)
            .checked_shl(self.shift)
            .unwrap_or(0)
    }
}

/// A validated framebuffer geometry and pixel encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FramebufferInfo {
    width: usize,
    height: usize,
    stride: usize,
    pixel_len: usize,
    size_bytes: u64,
    format: PixelFormat,
    red: ChannelEncoding,
    green: ChannelEncoding,
    blue: ChannelEncoding,
}

impl FramebufferDesc {
    /// Checks every field and returns the geometry the console relies on.
    pub fn validate(&self) -> Result<FramebufferInfo, Error> {
        let (red, green, blue) = match self.format {
            PixelFormat::Rgbx8 => (
                ChannelEncoding::byte(0),
                ChannelEncoding::byte(1),
                ChannelEncoding::byte(2),
            ),
            PixelFormat::Bgrx8 => (
                ChannelEncoding::byte(2),
                ChannelEncoding::byte(1),
                ChannelEncoding::byte(0),
            ),
            PixelFormat::Bitmask(masks) => validate_masks(masks)?,
            PixelFormat::BltOnly => return Err(Error::BltOnly),
        };
        if self.width == 0 || self.height == 0 || self.stride == 0 {
            return Err(Error::ZeroDimension);
        }
        if self.stride < self.width {
            return Err(Error::StrideTooSmall {
                width: self.width,
                stride: self.stride,
            });
        }
        let required = u64::from(self.stride)
            .checked_mul(u64::from(self.height))
            .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
            .ok_or(Error::SizeOverflow)?;
        let width = usize::try_from(self.width).map_err(|_| Error::SizeOverflow)?;
        let height = usize::try_from(self.height).map_err(|_| Error::SizeOverflow)?;
        let stride = usize::try_from(self.stride).map_err(|_| Error::SizeOverflow)?;
        let pixel_len = stride.checked_mul(height).ok_or(Error::SizeOverflow)?;
        if self.size_bytes < required {
            return Err(Error::BufferTooSmall {
                required,
                actual: self.size_bytes,
            });
        }
        Ok(FramebufferInfo {
            width,
            height,
            stride,
            pixel_len,
            size_bytes: self.size_bytes,
            format: self.format,
            red,
            green,
            blue,
        })
    }
}

fn validate_masks(
    masks: PixelMasks,
) -> Result<(ChannelEncoding, ChannelEncoding, ChannelEncoding), Error> {
    let all = [
        (Channel::Red, masks.red),
        (Channel::Green, masks.green),
        (Channel::Blue, masks.blue),
        (Channel::Reserved, masks.reserved),
    ];
    for &(channel, mask) in &all {
        if mask == 0 {
            if channel != Channel::Reserved {
                return Err(Error::EmptyMask(channel));
            }
            continue;
        }
        let shifted = mask >> mask.trailing_zeros();
        if shifted & shifted.wrapping_add(1) != 0 {
            return Err(Error::NonContiguousMask(channel));
        }
    }
    for (i, &(a, mask_a)) in all.iter().enumerate() {
        for &(b, mask_b) in all.iter().skip(i + 1) {
            if mask_a & mask_b != 0 {
                return Err(Error::OverlappingMasks(a, b));
            }
        }
    }
    let used = masks.red | masks.green | masks.blue | masks.reserved;
    let bits = 32 - used.leading_zeros();
    if bits.div_ceil(8) != 4 {
        return Err(Error::UnsupportedPixelSize { bits });
    }
    Ok((
        ChannelEncoding::from_mask(masks.red),
        ChannelEncoding::from_mask(masks.green),
        ChannelEncoding::from_mask(masks.blue),
    ))
}

impl FramebufferInfo {
    /// Visible width in pixels.
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Visible height in pixels.
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Pixels per scan line.
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// `stride * height`: the number of `u32` pixels a backing buffer needs.
    pub const fn pixel_len(&self) -> usize {
        self.pixel_len
    }

    /// Buffer length reported by firmware, at least `pixel_len() * 4`.
    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub const fn format(&self) -> PixelFormat {
        self.format
    }

    /// Whole 8x16 character columns that fit in the visible width.
    pub const fn cell_cols(&self) -> usize {
        self.width / crate::font::GLYPH_WIDTH
    }

    /// Whole 8x16 character rows that fit in the visible height.
    pub const fn cell_rows(&self) -> usize {
        self.height / crate::font::GLYPH_HEIGHT
    }

    /// Encodes a colour as a raw pixel value; reserved bits are zero.
    pub fn encode(&self, color: Color) -> u32 {
        self.red.encode(color.r) | self.green.encode(color.g) | self.blue.encode(color.b)
    }
}
