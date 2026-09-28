//! M9 framebuffer text console for early diagnostics (docs/specs/M9-HARDWARE.md).
//!
//! The target laptop (LENOVO 82K8) has no COM port, so early kernel output
//! has to go to the linear framebuffer that UEFI GOP leaves behind. This crate
//! turns a caller-supplied framebuffer description into a validated geometry
//! and draws text on it with an original 8x16 bitmap font.
//!
//! Layers:
//!
//! * [`FramebufferDesc`] is the raw GOP-style description (size, stride,
//!   pixel format, buffer length). [`FramebufferDesc::validate`] rejects every
//!   inconsistent description with an [`Error`] and yields a
//!   [`FramebufferInfo`], which also knows how to encode a [`Color`].
//! * [`Surface`] is the only way pixels are written. The kernel implements it
//!   over the real framebuffer with volatile, write-combining stores (and keeps
//!   the `unsafe` there); [`SliceSurface`] implements it over `&mut [u32]` for
//!   tests and shadow buffers. Reading pixels back is optional.
//! * [`Console`] works on a borrowed fixed-size [`TextBuffer`] (const
//!   generics, no `alloc`; the buffer has a `const fn new` so it can be a
//!   `static` instead of kilobytes on a kernel stack) and implements cursor
//!   movement, wrapping, scrolling, colours and [`core::fmt::Write`].
//!   Scrolling either copies pixel rows or repaints only the changed cells
//!   from the text buffer, so an MMIO framebuffer is never read.
//! * [`emergency_message`] and [`EmergencyWriter`] draw into a fixed band at
//!   the top of the screen using only the framebuffer geometry, for panic
//!   paths where the console state cannot be trusted. A panic handler should
//!   give them their own [`Surface`] over the framebuffer rather than the one
//!   inside a [`Console`], whose lock the panicking code may hold.
//!
//! No input makes this crate panic or write outside the validated geometry.

#![no_std]
#![forbid(unsafe_code)]

mod console;
mod emergency;
pub mod font;
mod font_data;
mod format;
mod render;
mod surface;

pub use console::{Attr, Cell, Console, Scroll, TextBuffer, DEFAULT_PALETTE};
pub use emergency::{emergency_message, EmergencyWriter, EMERGENCY_ROWS};
pub use format::{
    Channel, Color, Error, FramebufferDesc, FramebufferInfo, PixelFormat, PixelMasks,
};
pub use surface::{SliceSurface, Surface};
