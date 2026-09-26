use hw_smp::{LayoutError, LayoutField, PerCpuLayout, PerCpuSpec, VaRange, MAX_CPUS, PAGE_SIZE};

const P: u64 = PAGE_SIZE;
const WINDOW_BASE: u64 = 0xFFFF_FF00_0000_0000;

fn spec() -> PerCpuSpec {
    PerCpuSpec {
        stack_size: 16 * P,
        guard_size: P,
        data_size: 2 * P,
        ist_count: 3,
        ist_size: 4 * P,
    }
}

/// Every region of every CPU, tagged as mapped (`true`) or guard (`false`).
fn all_regions(layout: &PerCpuLayout) -> Vec<(usize, &'static str, bool, VaRange)> {
    let mut out = Vec::new();
    for cpu in 0..layout.cpu_count() {
        let r = layout.cpu(cpu).unwrap();
        out.push((cpu, "stack_guard", false, r.stack_guard));
        out.push((cpu, "stack", true, r.stack));
        for k in 0..r.ist_count {
            out.push((cpu, "ist_guard", false, r.ist_guards[k]));
            out.push((cpu, "ist", true, r.ist[k]));
        }
        out.push((cpu, "data", true, r.data));
    }
    out
}

#[test]
fn sixteen_cpu_layout_is_disjoint_aligned_and_guarded() {
    let n = 16;
    let stride = P + 16 * P + 3 * (P + 4 * P) + 2 * P;
    let window = VaRange::new(WINDOW_BASE, stride * n as u64);
    let layout = PerCpuLayout::new(window, n, spec(), &[]).unwrap();
    assert_eq!(layout.stride(), stride);
    assert_eq!(layout.used(), window);

    let regions = all_regions(&layout);
    assert_eq!(regions.len(), n * (3 + 2 * 3));
    let mut total = 0;
    for (i, (cpu, name, _, r)) in regions.iter().enumerate() {
        assert!(
            r.len > 0 && r.start % P == 0 && r.len % P == 0,
            "{cpu} {name}"
        );
        assert!(
            r.start >= window.start && r.end().unwrap() <= window.end().unwrap(),
            "{cpu} {name} outside window"
        );
        for (cpu2, name2, _, r2) in &regions[i + 1..] {
            assert!(!r.overlaps(r2), "{cpu} {name} overlaps {cpu2} {name2}");
        }
        total += r.len;
    }
    assert_eq!(
        total,
        layout.used().len,
        "regions tile the used span exactly"
    );

    for cpu in 0..n {
        let r = layout.cpu(cpu).unwrap();
        assert_eq!(
            r.stack_guard.end(),
            Some(r.stack.start),
            "guard below stack"
        );
        assert_eq!(r.stack_top() % 16, 0);
        for k in 0..r.ist_count {
            assert_eq!(
                r.ist_guards[k].end(),
                Some(r.ist[k].start),
                "guard below IST"
            );
            assert_eq!(r.ist_top(k), r.ist[k].end());
        }
        assert_eq!(r.ist_top(3), None);
    }
    // A stack overflowing downwards never lands in mapped memory: the page
    // below every stack start belongs to a guard.
    for (cpu, name, mapped, r) in &regions {
        if *mapped && (*name == "stack" || *name == "ist") {
            let below = r.start - 1;
            let hit = regions
                .iter()
                .find(|(_, _, _, g)| g.contains(below))
                .unwrap();
            assert!(!hit.2, "cpu {cpu} {name}: page below is mapped ({})", hit.1);
        }
    }
    assert_eq!(layout.cpu(n), None);
}

#[test]
fn window_must_fit_all_slots() {
    let stride = PerCpuLayout::new(VaRange::new(WINDOW_BASE, 1 << 30), 1, spec(), &[])
        .unwrap()
        .stride();
    let exact = VaRange::new(WINDOW_BASE, stride * 8);
    assert!(PerCpuLayout::new(exact, 8, spec(), &[]).is_ok());
    let short = VaRange::new(WINDOW_BASE, stride * 8 - P);
    assert_eq!(
        PerCpuLayout::new(short, 8, spec(), &[]).unwrap_err(),
        LayoutError::WindowTooSmall {
            needed: stride * 8,
            available: stride * 8 - P
        }
    );
}

#[test]
fn sizes_and_counts_are_validated() {
    let w = VaRange::new(WINDOW_BASE, 1 << 32);
    let bad = |f: fn(&mut PerCpuSpec)| {
        let mut s = spec();
        f(&mut s);
        PerCpuLayout::new(w, 4, s, &[]).unwrap_err()
    };
    assert_eq!(
        bad(|s| s.stack_size = 16 * P + 8),
        LayoutError::Misaligned(LayoutField::Stack)
    );
    assert_eq!(
        bad(|s| s.guard_size = 0),
        LayoutError::ZeroSize(LayoutField::Guard)
    );
    assert_eq!(
        bad(|s| s.data_size = 0),
        LayoutError::ZeroSize(LayoutField::Data)
    );
    assert_eq!(
        bad(|s| s.ist_size = 0),
        LayoutError::ZeroSize(LayoutField::Ist)
    );
    assert_eq!(bad(|s| s.ist_count = 8), LayoutError::TooManyIst(8));
    assert_eq!(
        bad(|s| s.ist_size = 100),
        LayoutError::Misaligned(LayoutField::Ist)
    );
    // No IST stacks: an IST size of zero is fine.
    let mut no_ist = spec();
    no_ist.ist_count = 0;
    no_ist.ist_size = 0;
    assert!(PerCpuLayout::new(w, 4, no_ist, &[]).is_ok());

    assert_eq!(
        PerCpuLayout::new(w, 0, spec(), &[]).unwrap_err(),
        LayoutError::NoCpus
    );
    assert_eq!(
        PerCpuLayout::new(w, MAX_CPUS + 1, spec(), &[]).unwrap_err(),
        LayoutError::TooManyCpus(MAX_CPUS + 1)
    );
    assert_eq!(
        PerCpuLayout::new(VaRange::new(WINDOW_BASE + 8, 1 << 30), 4, spec(), &[]).unwrap_err(),
        LayoutError::Misaligned(LayoutField::WindowStart)
    );
}

#[test]
fn arithmetic_overflow_is_an_error() {
    let w = VaRange::new(WINDOW_BASE, 1 << 32);
    let mut huge = spec();
    huge.stack_size = u64::MAX & !(P - 1);
    assert_eq!(
        PerCpuLayout::new(w, 4, huge, &[]).unwrap_err(),
        LayoutError::Overflow
    );
    let mut big = spec();
    big.ist_size = 1 << 62;
    big.ist_count = 7;
    assert_eq!(
        PerCpuLayout::new(w, 4, big, &[]).unwrap_err(),
        LayoutError::Overflow
    );
    let wrapping = VaRange::new(0xFFFF_FFFF_FFFF_0000, 0x2_0000);
    assert_eq!(
        PerCpuLayout::new(wrapping, 1, spec(), &[]).unwrap_err(),
        LayoutError::Overflow
    );
}

#[test]
fn window_must_be_canonical_and_in_one_half() {
    for w in [
        VaRange::new(0x0000_8000_0000_0000, 1 << 30),
        VaRange::new(0x0000_7FFF_FFF0_0000, 1 << 30),
        VaRange::new(0xFFFF_7FFF_0000_0000, 1 << 30),
    ] {
        assert_eq!(
            PerCpuLayout::new(w, 1, spec(), &[]).unwrap_err(),
            LayoutError::NonCanonical,
            "{w:?}"
        );
    }
    assert!(
        PerCpuLayout::new(VaRange::new(0x0000_7FFF_C000_0000, 1 << 30), 1, spec(), &[]).is_ok()
    );
}

#[test]
fn used_span_must_avoid_reserved_ranges() {
    let w = VaRange::new(WINDOW_BASE, 1 << 30);
    let layout = PerCpuLayout::new(w, 4, spec(), &[]).unwrap();
    let used = layout.used();
    let reserved = [
        VaRange::new(0x1000, P),
        VaRange::new(used.end().unwrap(), P), // just after: fine
        VaRange::new(used.end().unwrap() - P, P),
    ];
    assert_eq!(
        PerCpuLayout::new(w, 4, spec(), &reserved).unwrap_err(),
        LayoutError::OverlapsReserved(2)
    );
    assert!(PerCpuLayout::new(w, 4, spec(), &reserved[..2]).is_ok());
    let below = [VaRange::new(WINDOW_BASE - P, P + 1)];
    assert_eq!(
        PerCpuLayout::new(w, 4, spec(), &below).unwrap_err(),
        LayoutError::OverlapsReserved(0)
    );
}
