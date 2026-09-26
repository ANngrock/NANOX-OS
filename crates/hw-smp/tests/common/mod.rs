//! Test-only helpers: a minimal MADT reader (types 0 and 9 only, independent
//! of `hw-acpi`) and a watchdog so that a hang fails instead of blocking.

#![allow(dead_code)]

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use hw_smp::ApicEntry;

/// Reads `tests/fixtures/acpi/<rel>` and returns its processor entries in
/// table order. Panics on any structural problem: fixtures must be intact.
pub fn madt_cpus(rel: &str) -> Vec<ApicEntry> {
    let path = format!(
        "{}/../../tests/fixtures/acpi/{rel}",
        env!("CARGO_MANIFEST_DIR")
    );
    let d = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(d.len() >= 44, "MADT shorter than its header");
    assert_eq!(&d[0..4], b"APIC", "signature");
    let len = u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize;
    assert_eq!(len, d.len(), "header length vs file length");
    assert_eq!(d.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0, "checksum");
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let mut out = Vec::new();
    let mut off = 44;
    while off < len {
        assert!(off + 2 <= len, "truncated entry header at {off}");
        let (ty, l) = (d[off], d[off + 1] as usize);
        assert!(l >= 2 && off + l <= len, "bad entry length {l} at {off}");
        match ty {
            0 => {
                assert_eq!(l, 8, "LAPIC entry length");
                out.push(ApicEntry::from_madt_flags(
                    u32::from(d[off + 3]),
                    u32_at(off + 4),
                ));
            }
            9 => {
                assert_eq!(l, 16, "x2APIC entry length");
                out.push(ApicEntry::from_madt_flags(u32_at(off + 4), u32_at(off + 8)));
            }
            _ => {}
        }
        off += l;
    }
    out
}

/// Runs `f` on its own thread and fails the test if it does not finish in
/// `secs` seconds (a hang is a failure, not a pass).
pub fn with_watchdog<F: FnOnce() + Send + 'static>(secs: u64, f: F) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(()) => handle.join().unwrap(),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("watchdog: test did not finish in {secs} s"),
    }
}
