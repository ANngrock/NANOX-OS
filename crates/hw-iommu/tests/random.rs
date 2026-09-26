//! Randomised map/unmap sequences (deterministic PRNG) checked against a
//! reference model: return values, software lookups, frame accounting,
//! and translations by the independent hardware model after a global
//! invalidation. Allocation failures are injected along the way.

mod common;

use std::collections::BTreeMap;

use common::*;
use hw_iommu::amd::{self, cmd, AmdDomain, DeviceTable};
use hw_iommu::vtd::{self, inv, Capability, ContextEntry, RootTable, VtdDomain};
use hw_iommu::{
    Access, Bdf, CommandRing, Domain, DomainConfig, Error, InvalidationQueue, PagingLevels, Perms,
    PteFormat, QueueFormat, PAGE_SIZE,
};

const PAGE: u64 = PAGE_SIZE;
const OPS: usize = 1500;
const SEEDS: [u64; 3] = [1, 0x5eed_0f10, 0xdead_beef];

/// Hardware side of the check: flush all IOTLBs, translate one access.
trait Hw {
    fn flush_all(&mut self, mem: &mut ArrayMem);
    fn translate(&mut self, mem: &ArrayMem, iova: u64, access: Access) -> Result<u64, Fault>;
}

struct Vtd {
    hw: VtdHw,
    queue: InvalidationQueue<vtd::Invalidator>,
    dev: Bdf,
}

struct Amd {
    hw: AmdHw,
    queue: InvalidationQueue<amd::Invalidator>,
    dev: Bdf,
}

fn flush<Q: QueueFormat>(
    mem: &mut ArrayMem,
    queue: &mut InvalidationQueue<Q>,
    command: [u64; 2],
    run: impl FnOnce(&mut ArrayMem, u64) -> u64,
) {
    let token = queue.submit(mem, &[command]).unwrap();
    let head = run(mem, queue.ring().tail_offset());
    queue.update_head(head).unwrap();
    queue.poll(mem).unwrap();
    assert!(queue.is_complete(token).unwrap());
}

impl Hw for Vtd {
    fn flush_all(&mut self, mem: &mut ArrayMem) {
        let hw = &mut self.hw;
        flush(
            mem,
            &mut self.queue,
            inv::iotlb_global(inv::Drain::default()),
            |m, t| hw.run_queue(m, RING_BASE, t, usize::MAX),
        );
    }
    fn translate(&mut self, mem: &ArrayMem, iova: u64, access: Access) -> Result<u64, Fault> {
        self.hw.translate(mem, self.dev.raw(), iova, access)
    }
}

impl Hw for Amd {
    fn flush_all(&mut self, mem: &mut ArrayMem) {
        let hw = &mut self.hw;
        flush(mem, &mut self.queue, cmd::invalidate_all(), |m, t| {
            hw.run_queue(m, RING_BASE, t, usize::MAX)
        });
    }
    fn translate(&mut self, mem: &ArrayMem, iova: u64, access: Access) -> Result<u64, Fault> {
        self.hw.translate(mem, self.dev.raw(), iova, access)
    }
}

type Reference = BTreeMap<u64, (u64, Perms)>;

fn random_perms(rng: &mut Prng) -> Perms {
    [Perms::R, Perms::W, Perms::RW][rng.below(3) as usize]
}

/// Checks one page against the reference through both the software
/// lookup and the hardware model.
fn check_page<F: PteFormat, H: Hw>(
    mem: &ArrayMem,
    d: &Domain<F>,
    hw: &mut H,
    reference: &Reference,
    page: u64,
    rng: &mut Prng,
) {
    let iova = page * PAGE + rng.below(PAGE / 8) * 8;
    let expected = reference
        .get(&page)
        .map(|&(pp, perms)| (pp * PAGE + iova % PAGE, perms));
    assert_eq!(d.lookup(mem, iova), Ok(expected), "lookup {iova:#x}");
    let access = if rng.below(2) == 0 {
        Access::Read
    } else {
        Access::Write
    };
    let want = match expected {
        None => Err(Fault::NotPresent),
        Some((_, p)) if !p.allows(access) => Err(if access == Access::Read {
            Fault::ReadDenied
        } else {
            Fault::WriteDenied
        }),
        Some((pa, _)) => Ok(pa),
    };
    assert_eq!(
        hw.translate(mem, iova, access),
        want,
        "hardware {iova:#x} {access:?}"
    );
}

#[allow(clippy::too_many_arguments)]
fn drive<F: PteFormat, H: Hw>(
    mem: &mut ArrayMem,
    alloc: &mut TestAlloc,
    d: &mut Domain<F>,
    hw: &mut H,
    fixed_frames: usize,
    windows: &[(u64, u64)],
    seed: u64,
) -> (usize, usize) {
    let mut rng = Prng::new(seed);
    let mut reference = Reference::new();
    let (mut ok, mut refused) = (0, 0);
    for step in 0..OPS {
        let injected = rng.below(10) == 0;
        if injected {
            alloc.fail_after = Some(rng.below(3) as usize);
        }
        let (w_start, w_pages) = windows[rng.below(windows.len() as u64) as usize];
        let touched: (u64, u64);
        let error = if rng.below(100) < 55 || reference.is_empty() {
            let start = w_start + rng.below(w_pages);
            let n = (1 + rng.below(8)).min(w_start + w_pages - start);
            let phys_page = 0x10_0000 + rng.below(1 << 20);
            let perms = random_perms(&mut rng);
            let conflict = (start..start + n).find(|p| reference.contains_key(p));
            let res = d.map(mem, alloc, start * PAGE, phys_page * PAGE, n * PAGE, perms);
            touched = (start, n);
            match (conflict, res) {
                (Some(p), r) => {
                    assert_eq!(
                        r,
                        Err(Error::AlreadyMapped { iova: p * PAGE }),
                        "step {step}"
                    );
                    true
                }
                (None, Err(Error::OutOfFrames)) => {
                    assert!(injected, "step {step}: OutOfFrames without injection");
                    true
                }
                (None, Ok(())) => {
                    for i in 0..n {
                        reference.insert(start + i, (phys_page + i, perms));
                    }
                    false
                }
                (None, r) => panic!("step {step}: unexpected {r:?}"),
            }
        } else {
            let start = if rng.below(4) != 0 {
                *reference
                    .keys()
                    .nth(rng.below(reference.len() as u64) as usize)
                    .unwrap()
            } else {
                w_start + rng.below(w_pages)
            };
            let n = 1 + rng.below(4);
            let missing = (start..start + n).find(|p| !reference.contains_key(p));
            let res = d.unmap(mem, start * PAGE, n * PAGE);
            touched = (start, n);
            let beyond = (start + n) * PAGE > d.config().iova_limit();
            match missing {
                Some(_) if beyond => {
                    assert_eq!(res, Err(Error::OutOfRange), "step {step}");
                    true
                }
                Some(p) => {
                    assert_eq!(res, Err(Error::NotMapped { iova: p * PAGE }), "step {step}");
                    true
                }
                None => {
                    assert_eq!(res, Ok(()), "step {step}");
                    for p in start..start + n {
                        reference.remove(&p);
                    }
                    false
                }
            }
        };
        alloc.fail_after = None;
        if error {
            refused += 1;
        } else {
            ok += 1;
        }

        assert_eq!(d.mapped_pages(), reference.len() as u64, "step {step}");
        assert_eq!(
            alloc.live.len(),
            d.table_frames() as usize + fixed_frames,
            "step {step}: frame leak"
        );
        hw.flush_all(mem);
        let limit_pages = d.config().iova_limit() / PAGE;
        for p in touched.0.saturating_sub(1)..(touched.0 + touched.1 + 1).min(limit_pages) {
            check_page(mem, d, hw, &reference, p, &mut rng);
        }
        if error || step % 50 == 0 {
            for &(ws, wp) in windows {
                for p in ws..ws + wp {
                    check_page(mem, d, hw, &reference, p, &mut rng);
                }
            }
        }
    }
    (ok, refused)
}

const GIB_PAGES: u64 = 1 << 18;

#[test]
fn vtd_three_level_random_sequences_match_reference() {
    let caps = Capability(0x00d2_008c_2226_0206);
    let cfg = caps.domain_config(39).unwrap();
    assert_eq!(cfg.levels(), PagingLevels::Three);
    let windows = [(GIB_PAGES - 32, 64), (0x200 - 16, 32), ((1 << 27) - 16, 16)];
    for seed in SEEDS {
        let mut mem = ArrayMem::new();
        let mut alloc = TestAlloc::new();
        let mut root = RootTable::new(&mut mem, &mut alloc, 39).unwrap();
        let mut d = VtdDomain::new(&mut mem, &mut alloc, 5, cfg).unwrap();
        let dev = Bdf::new(0, 1, 0).unwrap();
        root.attach(
            &mut mem,
            &mut alloc,
            dev,
            &ContextEntry::for_domain(&d, caps).unwrap(),
        )
        .unwrap();
        let ring = CommandRing::new(RING_BASE, 256).unwrap();
        let queue =
            InvalidationQueue::new(&mut mem, ring, STATUS_ADDR, 1, vtd::Invalidator::new(caps))
                .unwrap();
        let mut hw = Vtd {
            hw: VtdHw::new(root.address(), 39),
            queue,
            dev,
        };
        let (ok, refused) = drive(&mut mem, &mut alloc, &mut d, &mut hw, 2, &windows, seed);
        assert!(
            ok > OPS / 4 && refused > OPS / 10,
            "seed {seed}: ok {ok}, refused {refused}"
        );
    }
}

#[test]
fn amd_four_level_random_sequences_match_reference() {
    let cfg = DomainConfig::new(PagingLevels::Four, 48, 48).unwrap();
    let windows = [
        (GIB_PAGES - 32, 64),
        (0x200 - 16, 32),
        ((1 << 27) - 32, 64),
        ((1 << 36) - 16, 16),
    ];
    for seed in SEEDS {
        let mut mem = ArrayMem::new();
        let mut alloc = TestAlloc::new();
        let mut dt = DeviceTable::new(&mut mem, DEV_TABLE_BASE, DEV_TABLE_PAGES, 48).unwrap();
        let mut d = AmdDomain::new(&mut mem, &mut alloc, 5, cfg).unwrap();
        let dev = Bdf::new(0, 1, 0).unwrap();
        dt.attach(&mut mem, dev, &d).unwrap();
        let ring = CommandRing::new(RING_BASE, 256).unwrap();
        let queue =
            InvalidationQueue::new(&mut mem, ring, STATUS_ADDR, 1, amd::Invalidator).unwrap();
        let mut hw = Amd {
            hw: AmdHw::new(dt.register_value()),
            queue,
            dev,
        };
        let (ok, refused) = drive(&mut mem, &mut alloc, &mut d, &mut hw, 0, &windows, seed);
        assert!(
            ok > OPS / 4 && refused > OPS / 10,
            "seed {seed}: ok {ok}, refused {refused}"
        );
    }
}
