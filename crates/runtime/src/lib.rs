//! The part of the native userspace runtime that does not depend on syscall
//! numbers (docs/specs/M10-NATIVE.md): a heap, the memory routines a
//! freestanding program needs, the process startup contract and the entry
//! point. The syscall layer itself is the ABI crate of M2 (`abi-user`); this
//! crate asks it for memory through one function pointer and for nothing else.
//!
//! `no_std`, no dependencies. The only unsafe code is in `heap` (raw free
//! list), `mem` (inline assembly) and `start` (the entry); every other module
//! and the tests around them are safe.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod heap;
pub mod mem;
pub mod start;

pub use heap::{Heap, HeapError, LockedHeap};
pub use start::{StartError, StartInfo};
