//! The core of the Linux personality: an isolated NANOX service that runs
//! unmodified static Linux x86-64 programs (the Rust toolchain built for
//! musl) by translating their system calls into requests on capabilities it
//! was delegated (docs/specs/M10-ROUTES.md, route B).
//!
//! What lives here is everything that does not need a kernel: the
//! system-call table, the policy (what is handled, stubbed, refused), the
//! errno vocabulary, path resolution confined to the delegated root, the
//! file-descriptor table with shared open files, the memory map bookkeeping
//! for `mmap`/`brk` with a no-writable-and-executable rule, guest memory
//! access, and the dispatcher that ties them to a [`Backend`] (files, memory,
//! time, threads) which the real service implements over NANOX objects and
//! the tests implement in memory. The process never gets more authority than
//! its [`Config`] and its delegated handles give.
//!
//! `no_std`, no allocation, safe Rust.

#![no_std]
#![forbid(unsafe_code)]

pub mod abi;
pub mod backend;
pub mod errno;
pub mod fdtable;
pub mod mem;
pub mod path;
pub mod personality;
pub mod policy;
pub mod table;
pub mod vma;

mod fdops;
mod fs;
mod handlers;
mod memcalls;
mod misc;
mod process;

pub use backend::Backend;
pub use errno::Errno;
pub use personality::{Config, Outcome, Personality, Refusal};
