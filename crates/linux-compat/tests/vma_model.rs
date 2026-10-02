//! Random mmap, munmap and mprotect against a page-by-page model.

use linux_compat::errno::ENOMEM;
use linux_compat::vma::{Backing, Space, PROT_EXEC, PROT_READ, PROT_WRITE};

const P: u64 = 4096;
const PAGES: usize = 64;
const LO: u64 = 0x10_0000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

type Model = [Option<u8>; PAGES];

fn agree(s: &Space<12>, m: &Model, ctx: &str) {
    s.check().unwrap_or_else(|e| panic!("{e} {ctx}"));
    for (i, want) in m.iter().enumerate() {
        let got = s.find(LO + i as u64 * P).map(|v| v.prot);
        assert_eq!(got, *want, "page {i} {ctx}");
    }
    // Neighbours that could be one entry are one entry.
    for w in s.entries().windows(2) {
        assert!(
            !(w[0].end == w[1].start && w[0].prot == w[1].prot && w[0].backing == w[1].backing),
            "unmerged neighbours {w:?} {ctx}"
        );
    }
}

#[test]
fn random_operations_agree_with_the_model() {
    let prots = [PROT_READ, PROT_READ | PROT_WRITE, PROT_READ | PROT_EXEC, 0];
    let mut refused = 0;
    for seed in 1..=6u64 {
        let mut rng = Rng(seed * 0x9E37_79B9_7F4A);
        let mut s: Space<12> = Space::new(LO, LO + PAGES as u64 * P, LO, 1 << 40, false);
        let mut m: Model = [None; PAGES];
        for step in 0..60_000 {
            let ctx = format!("seed {seed} step {step}");
            let (first, n) = (rng.below(PAGES as u64) as usize, 1 + rng.below(8) as usize);
            let end = (first + n).min(PAGES);
            let prot = prots[rng.below(4) as usize];
            match rng.below(5) {
                0 | 1 => {
                    // Without a hint: the highest gap that fits.
                    match s.plan_mmap(0, n as u64 * P, prot, false, Backing::Anon) {
                        Ok(p) => {
                            let expect = (n..=PAGES)
                                .rev()
                                .find(|e| m[e - n..*e].iter().all(|x| x.is_none()));
                            let at = ((p.addr - LO) / P) as usize;
                            assert_eq!(Some(at + n), expect, "not the highest gap {ctx}");
                            s.commit_mmap(p);
                            m[at..at + n].fill(Some(prot));
                        }
                        Err(e) => {
                            assert_eq!(e, ENOMEM, "{ctx}");
                            refused += 1;
                        }
                    }
                }
                2 => {
                    // Fixed: replaces whatever is there.
                    if let Ok(p) = s.plan_mmap(
                        LO + first as u64 * P,
                        (end - first) as u64 * P,
                        prot,
                        true,
                        Backing::Anon,
                    ) {
                        s.commit_mmap(p);
                        m[first..end].fill(Some(prot));
                    } else {
                        refused += 1;
                    }
                }
                3 => {
                    if let Ok((a, b)) =
                        s.plan_munmap(LO + first as u64 * P, (end - first) as u64 * P)
                    {
                        s.commit_munmap(a, b);
                        m[first..end].fill(None);
                    } else {
                        refused += 1;
                    }
                }
                _ => match s.plan_mprotect(LO + first as u64 * P, (end - first) as u64 * P, prot) {
                    Ok((a, b)) => {
                        assert!(
                            m[first..end].iter().all(|x| x.is_some()),
                            "mprotect on unmapped pages {ctx}"
                        );
                        s.commit_mprotect(a, b, prot);
                        m[first..end].fill(Some(prot));
                    }
                    Err(e) => {
                        assert_eq!(e, ENOMEM, "{ctx}");
                        refused += 1;
                    }
                },
            }
            agree(&s, &m, &ctx);
        }
    }
    assert!(
        refused > 100,
        "the table was meant to fill up sometimes: {refused}"
    );
}
