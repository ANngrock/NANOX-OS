//! Bounded host-testable IPv4 network-service primitives for M5.
//!
//! This crate contains no driver, DMA access, global clock, or allocator.
//! Callers supply time and packet buffers. It is not wired into the M0 guest;
//! the integration tests exercise the packet path and TCP client in a
//! deterministic simulated link.

#![no_std]
#![forbid(unsafe_code)]

pub mod arp;
pub mod demux;
pub mod dns;

pub use demux::{demux_frame, InboundPacket, StackError};
