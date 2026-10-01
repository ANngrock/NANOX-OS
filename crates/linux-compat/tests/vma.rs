//! The address-space bookkeeping: scenarios, and random operations against a
//! page-by-page model.

use linux_compat::errno::{EINVAL, ENOMEM, EPERM};
use linux_compat::vma::{Backing, Space, PROT_EXEC, PROT_READ, PROT_WRITE};

const P: u64 = 4096;
const LO: u64 = 0x1_0000;
const HI: u64 = 0x1_0000 + 64 * P;

fn space() -> Space<16> {
    Space::new(LO, HI, LO, 1 << 40, false)
}

fn map(
    s: &mut Space<16>,
    hint: u64,
    len: u64,
    prot: u8,
    fixed: bool,
) -> Result<u64, linux_compat::Errno> {
    let p = s.plan_mmap(hint, len, prot, fixed, Backing::Anon)?;
    s.commit_mmap(p);
    s.check().unwrap();
    Ok(p.addr)
}

fn unmap(s: &mut Space<16>, a: u64, l: u64) -> Result<(), linux_compat::Errno> {
    let (x, y) = s.plan_munmap(a, l)?;
    s.commit_munmap(x, y);
    s.check().unwrap();
    Ok(())
}

fn protect(s: &mut Space<16>, a: u64, l: u64, prot: u8) -> Result<(), linux_compat::Errno> {
    let (x, y) = s.plan_mprotect(a, l, prot)?;
    s.commit_mprotect(x, y, prot);
    s.check().unwrap();
    Ok(())
}

const RW: u8 = PROT_READ | PROT_WRITE;

#[test]
fn allocation_goes_top_down_and_fills_holes() {
    let mut s = space();
    let a = map(&mut s, 0, P, RW, false).unwrap();
    assert_eq!(a, HI - P, "the first mapping sits at the top");
    let b = map(&mut s, 0, 3 * P, RW, false).unwrap();
    assert_eq!(b, a - 3 * P, "the next one just below it");
    assert_eq!(s.entries().len(), 1, "identical neighbours merge");
    unmap(&mut s, b + P, P).unwrap();
    assert_eq!(s.entries().len(), 2, "a hole splits the mapping");
    let c = map(&mut s, 0, P, RW, false).unwrap();
    assert_eq!(
        c,
        b + P,
        "the highest gap that fits is the hole, so it is filled first"
    );
    let d = map(&mut s, 0, 2 * P, RW, false).unwrap();
    assert_eq!(d, b - 2 * P, "with no hole left the next one goes below");
}

#[test]
fn lengths_round_up_and_bad_requests_are_refused() {
    let mut s = space();
    let a = map(&mut s, 0, 1, RW, false).unwrap();
    assert_eq!(s.entries()[0].end - s.entries()[0].start, P);
    assert_eq!(map(&mut s, 0, 0, RW, false), Err(EINVAL));
    assert_eq!(
        map(&mut s, 0, 100 * P, RW, false),
        Err(ENOMEM),
        "bigger than the window"
    );
    assert_eq!(map(&mut s, 0, u64::MAX, RW, false), Err(ENOMEM));
    assert_eq!(
        map(&mut s, 123, P, RW, true),
        Err(EINVAL),
        "fixed must be aligned"
    );
    assert_eq!(
        map(&mut s, 0, P, 8, false),
        Err(EINVAL),
        "unknown protection bits"
    );
    assert_eq!(
        map(&mut s, LO - P, P, RW, true),
        Err(ENOMEM),
        "fixed outside the window"
    );
    assert_eq!(map(&mut s, HI, P, RW, true), Err(ENOMEM));
    assert_eq!(unmap(&mut s, a + 1, P), Err(EINVAL));
    assert_eq!(unmap(&mut s, a, 0), Err(EINVAL));
    assert_eq!(unmap(&mut s, 0, P), Err(EINVAL), "outside the window");
}

#[test]
fn a_hint_is_honoured_when_free_and_fixed_replaces() {
    let mut s = space();
    let h = LO + 10 * P;
    assert_eq!(map(&mut s, h, 2 * P, RW, false), Ok(h));
    // A hint on top of something does not overwrite it without MAP_FIXED.
    let other = map(&mut s, h, P, RW, false).unwrap();
    assert_ne!(other, h);
    // Fixed replaces what is there, splitting it.
    let p = s
        .plan_mmap(h + P, P, PROT_READ, true, Backing::Anon)
        .unwrap();
    assert!(p.replaces);
    s.commit_mmap(p);
    s.check().unwrap();
    assert!(s.covers(h, P, RW) && s.covers(h + P, P, PROT_READ) && !s.covers(h + P, P, PROT_WRITE));
}

#[test]
fn writable_and_executable_is_refused_unless_allowed() {
    let mut s = space();
    assert_eq!(map(&mut s, 0, P, RW | PROT_EXEC, false), Err(EPERM));
    let a = map(&mut s, 0, P, RW, false).unwrap();
    assert_eq!(protect(&mut s, a, P, RW | PROT_EXEC), Err(EPERM));
    assert_eq!(
        protect(&mut s, a, P, PROT_READ | PROT_EXEC),
        Ok(()),
        "execute without write is fine"
    );
    let mut open: Space<16> = Space::new(LO, HI, LO, 1 << 40, true);
    let p = open
        .plan_mmap(0, P, RW | PROT_EXEC, false, Backing::Anon)
        .unwrap();
    open.commit_mmap(p);
    open.check().unwrap();
}

#[test]
fn mprotect_splits_changes_and_merges_back() {
    let mut s = space();
    let a = map(&mut s, 0, 4 * P, RW, false).unwrap();
    protect(&mut s, a + P, 2 * P, PROT_READ).unwrap();
    assert_eq!(s.entries().len(), 3);
    assert!(
        s.covers(a, P, RW) && s.covers(a + P, 2 * P, PROT_READ) && !s.covers(a + P, P, PROT_WRITE)
    );
    protect(&mut s, a + P, 2 * P, RW).unwrap();
    assert_eq!(s.entries().len(), 1, "the pieces join again");
    assert_eq!(protect(&mut s, a + 8 * P, P, RW), Err(ENOMEM), "not mapped");
    assert_eq!(
        protect(&mut s, a, 10 * P, RW),
        Err(ENOMEM),
        "partly not mapped"
    );
    assert_eq!(protect(&mut s, a + 1, P, RW), Err(EINVAL));
    assert_eq!(protect(&mut s, a, 0, RW), Ok(()));
}

#[test]
fn brk_grows_shrinks_and_never_runs_into_mappings() {
    let mut s = space();
    let base = s.brk_end();
    assert_eq!(base, LO);
    let p = s.plan_brk(0);
    assert_eq!(
        (p.old_end, p.new_end),
        (base, base),
        "asking changes nothing"
    );
    let p = s.plan_brk(base + 3 * P - 5);
    assert_eq!(p.new_end, base + 3 * P, "rounded up to a page");
    s.commit_brk(p);
    s.check().unwrap();
    assert!(s.covers(base, 3 * P, RW));
    let p = s.plan_brk(base + 5 * P);
    s.commit_brk(p);
    assert_eq!(s.entries().len(), 1, "the heap is one mapping");
    let p = s.plan_brk(base + P);
    s.commit_brk(p);
    assert!(s.covers(base, P, RW) && !s.covers(base + P, P, 0), "shrunk");
    // A mapping in the way stops growth; the old end is kept.
    let m = map(&mut s, base + 4 * P, P, RW, true).unwrap();
    let p = s.plan_brk(m + 2 * P);
    assert_eq!(p.new_end, p.old_end);
    // Below the base is only a query.
    assert_eq!(s.plan_brk(base - P).new_end, s.brk_end());
    // Beyond the window.
    assert_eq!(s.plan_brk(HI + P).new_end, s.brk_end());
}

#[test]
fn the_memory_limit_and_a_full_table_are_enforced() {
    let mut s: Space<16> = Space::new(LO, HI, LO, 4 * P, false);
    map(&mut s, 0, 3 * P, RW, false).unwrap();
    assert_eq!(
        map(&mut s, 0, 2 * P, RW, false),
        Err(ENOMEM),
        "over the limit"
    );
    let p = s.plan_brk(LO + 5 * P);
    assert_eq!(p.new_end, p.old_end, "the heap respects it too");
    // A full table: alternate protections so nothing merges.
    let mut t: Space<6> = Space::new(LO, HI, LO, 1 << 40, false);
    for i in 0..5u64 {
        let prot = if i % 2 == 0 { RW } else { PROT_READ };
        let p = t
            .plan_mmap(LO + (2 * i) * P, P, prot, true, Backing::Anon)
            .unwrap();
        t.commit_mmap(p);
    }
    assert_eq!(t.entries().len(), 5);
    assert_eq!(
        t.plan_mmap(0, P, RW, false, Backing::Anon).err(),
        Some(ENOMEM),
        "two free entries are kept for splits"
    );
}

#[test]
fn file_mappings_keep_their_offsets_through_splits() {
    let mut s = space();
    let p = s
        .plan_mmap(
            0,
            4 * P,
            PROT_READ,
            false,
            Backing::File {
                obj: 9,
                offset: 0x1000,
            },
        )
        .unwrap();
    s.commit_mmap(p);
    let a = p.addr;
    unmap(&mut s, a + P, P).unwrap();
    let e = s.entries();
    assert_eq!(e.len(), 2);
    assert_eq!(
        e[0].backing,
        Backing::File {
            obj: 9,
            offset: 0x1000
        }
    );
    assert_eq!(
        e[1].backing,
        Backing::File {
            obj: 9,
            offset: 0x1000 + 2 * P
        },
        "the upper piece starts later in the file"
    );
    protect(&mut s, a + 2 * P, P, RW).unwrap();
    let e = s.entries();
    assert_eq!(e[1].end - e[1].start, P);
    assert_eq!(
        e[1].backing,
        Backing::File {
            obj: 9,
            offset: 0x1000 + 2 * P
        }
    );
    assert_eq!(
        e[2].backing,
        Backing::File {
            obj: 9,
            offset: 0x1000 + 3 * P
        }
    );
}
