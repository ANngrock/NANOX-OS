//! TCP for the M5 provider path: a segment codec and an active-open client
//! connection implementing the profile in docs/specs/M5-TCP.md. This is a
//! deliberately limited subset, not a complete RFC 9293 implementation.
//!
//! The connection performs no I/O and reads no clock: the caller feeds parsed
//! segments and the current time in milliseconds, and pulls outgoing segments
//! with `Connection::poll_transmit`. Buffers are borrowed from the caller.

#![no_std]
#![forbid(unsafe_code)]

mod conn;
mod ring;
pub mod segment;

pub use conn::{Config, Connection, Endpoint, State, TcpError};
pub use segment::{emit_tcp, parse_tcp, SegmentError, TcpHeader, TcpSegment};

/// Sequence-number comparison modulo 2^32 (RFC 9293 section 3.4, RFC 1982).
/// Valid while the compared values are less than 2^31 apart.
pub mod seq {
    pub fn lt(a: u32, b: u32) -> bool {
        (a.wrapping_sub(b) as i32) < 0
    }

    pub fn le(a: u32, b: u32) -> bool {
        a == b || lt(a, b)
    }

    pub fn gt(a: u32, b: u32) -> bool {
        lt(b, a)
    }

    pub fn ge(a: u32, b: u32) -> bool {
        le(b, a)
    }

    /// `start <= value < start + len` modulo 2^32.
    pub fn in_window(value: u32, start: u32, len: u32) -> bool {
        value.wrapping_sub(start) < len
    }
}
