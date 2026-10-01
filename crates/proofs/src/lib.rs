//! Bounded exhaustive checks of properties of concrete components, and
//! attestations that bind every result to the exact sources it was obtained
//! on. A result is **not** a proof for all inputs: each property states its
//! bound, and an attestation says "within this bound, on these sources, this
//! held" — and goes stale the moment a source changes.

pub mod attest;
pub mod props;
pub mod sha256;

pub use attest::{hash_files, Proof, Property, Record, Status};
