use svc::modecfg::{self, DecodeError};
use svc::share::{allocate, Claim, ShareError, TOTAL};
use svc::Mode;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn bound(c: &Claim) -> u32 {
    c.cap.min(c.demand).min(TOTAL)
}

fn floor(c: &Claim) -> u32 {
    c.reserve.min(bound(c))
}

#[test]
fn allocation_satisfies_its_properties_on_random_claims() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let (mut ok, mut over) = (0, 0);
    for _ in 0..200_000 {
        let n = 1 + rng.below(8) as usize;
        let claims: Vec<Claim> = (0..n)
            .map(|_| {
                let cap = rng.below(1200) as u32;
                Claim {
                    reserve: if rng.below(3) == 0 {
                        0
                    } else {
                        rng.below(400) as u32
                    },
                    weight: if rng.below(8) == 0 {
                        0
                    } else {
                        1 + rng.below(500) as u32
                    },
                    cap: if rng.below(4) == 0 { TOTAL } else { cap },
                    demand: if rng.below(4) == 0 {
                        TOTAL
                    } else {
                        rng.below(1200) as u32
                    },
                }
            })
            .collect();
        let mut out = vec![0u32; n];
        let floors: u32 = claims.iter().map(floor).sum();
        let r = allocate(&claims, &mut out);
        if floors > TOTAL {
            assert_eq!(r, Err(ShareError::Overcommitted), "{claims:?}");
            over += 1;
            continue;
        }
        r.unwrap();
        ok += 1;
        // Guarantee and bound.
        for (c, &a) in claims.iter().zip(&out) {
            assert!(a >= floor(c), "guarantee broken: {claims:?} -> {out:?}");
            assert!(a <= bound(c), "bound broken: {claims:?} -> {out:?}");
        }
        assert!(out.iter().sum::<u32>() <= TOTAL, "{claims:?} -> {out:?}");
        // Work conservation: someone who can take more (weight > 0) means
        // the whole machine is handed out.
        let hungry = claims
            .iter()
            .zip(&out)
            .any(|(c, &a)| c.weight > 0 && a < bound(c));
        if hungry {
            assert_eq!(
                out.iter().sum::<u32>(),
                TOTAL,
                "not work-conserving: {claims:?} -> {out:?}"
            );
        }
        // Fairness among claimants that could take more: the extra above
        // the guarantee is proportional to the weight, within rounding.
        let open: Vec<usize> = (0..n)
            .filter(|&i| claims[i].weight > 0 && out[i] < bound(&claims[i]))
            .collect();
        for &i in &open {
            for &j in &open {
                let (ei, ej) = (
                    i64::from(out[i] - floor(&claims[i])),
                    i64::from(out[j] - floor(&claims[j])),
                );
                let (wi, wj) = (i64::from(claims[i].weight), i64::from(claims[j].weight));
                assert!(
                    (ei * wj - ej * wi).abs() <= wi + wj,
                    "unfair {i} vs {j}: {claims:?} -> {out:?}"
                );
            }
        }
        // Nobody who reached its bound has more than the fair level would
        // give, i.e. no open claimant has less extra-per-weight than a
        // capped one that got *more* than its weight entitles it to.
        for &i in &open {
            for (j, cj) in claims.iter().enumerate() {
                if cj.weight > 0 && out[j] == bound(cj) && out[j] > floor(cj) {
                    let (ei, ej) = (
                        i64::from(out[i] - floor(&claims[i])),
                        i64::from(out[j] - floor(cj)),
                    );
                    let (wi, wj) = (i64::from(claims[i].weight), i64::from(cj.weight));
                    assert!(
                        ej * wi <= ei * wj + wi + wj,
                        "capped {j} got more per weight than open {i}: {claims:?} -> {out:?}"
                    );
                }
            }
        }
    }
    assert!(
        ok > 100_000 && over > 100,
        "{ok} valid, {over} overcommitted"
    );
}

#[test]
fn allocation_edge_cases() {
    let c = |reserve, weight, cap, demand| Claim {
        reserve,
        weight,
        cap,
        demand,
    };
    let mut out = [0u32; 3];
    // Short buffer.
    assert_eq!(
        allocate(&[c(0, 1, 10, 10); 3], &mut out[..2]),
        Err(ShareError::Buffer)
    );
    // Nothing to give.
    allocate(&[], &mut []).unwrap();
    allocate(&[c(0, 1, 1000, 0); 3], &mut out).unwrap();
    assert_eq!(out, [0, 0, 0]);
    // Equal weights split evenly, the rounding remainder goes in index order.
    allocate(&[c(0, 1, 1000, 1000); 3], &mut out).unwrap();
    assert_eq!(out, [334, 333, 333]);
    // A capped claimant frees its excess for the others.
    allocate(
        &[c(0, 1, 100, 1000), c(0, 1, 1000, 1000), c(0, 1, 1000, 1000)],
        &mut out,
    )
    .unwrap();
    assert_eq!(out, [100, 450, 450]);
    // Weight 0 gets only its reservation.
    allocate(
        &[
            c(200, 0, 1000, 1000),
            c(0, 1, 1000, 1000),
            c(0, 0, 1000, 1000),
        ],
        &mut out,
    )
    .unwrap();
    assert_eq!(out, [200, 800, 0]);
    // Reservations are honoured even against a heavy neighbour.
    allocate(
        &[c(300, 1, 1000, 1000), c(0, 1000, 1000, 1000), c(0, 0, 0, 0)],
        &mut out,
    )
    .unwrap();
    assert!(out[0] >= 300);
    assert_eq!(out[0] + out[1], TOTAL);
    // A reservation above demand is not held back.
    allocate(
        &[c(500, 1, 1000, 100), c(0, 1, 1000, 1000), c(0, 0, 0, 0)],
        &mut out,
    )
    .unwrap();
    assert_eq!(out, [100, 900, 0]);
    // Guarantees that do not fit are refused whole.
    assert_eq!(
        allocate(
            &[c(600, 1, 1000, 1000), c(600, 1, 1000, 1000), c(0, 0, 0, 0)],
            &mut out
        ),
        Err(ShareError::Overcommitted)
    );
}

#[test]
fn crc32_matches_the_standard_check_value() {
    assert_eq!(modecfg::crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(modecfg::crc32(b""), 0);
}

#[test]
fn mode_record_round_trips_and_rejects_every_single_bit_flip() {
    for mode in [Mode::Desktop, Mode::Hybrid, Mode::Server] {
        let rec = modecfg::encode(mode);
        assert_eq!(modecfg::decode(&rec), Ok(mode));
        let b = modecfg::boot(Some(&rec));
        assert_eq!((b.mode, b.safe, b.reason), (mode, false, None));
        for byte in 0..rec.len() {
            for bit in 0..8 {
                let mut bad = rec;
                bad[byte] ^= 1 << bit;
                assert!(
                    modecfg::decode(&bad).is_err(),
                    "{mode:?} byte {byte} bit {bit}"
                );
                let b = modecfg::boot(Some(&bad));
                assert_eq!((b.mode, b.safe), (Mode::Server, true));
                assert!(b.reason.is_some());
            }
        }
    }
    // Length and content errors are told apart.
    let rec = modecfg::encode(Mode::Hybrid);
    assert_eq!(modecfg::decode(&rec[..11]), Err(DecodeError::Length));
    let mut long = rec.to_vec();
    long.push(0);
    assert_eq!(modecfg::decode(&long), Err(DecodeError::Length));
    let mut m = rec;
    m[0] = b'X';
    assert_eq!(modecfg::decode(&m), Err(DecodeError::Magic));
    let mut v = rec;
    v[4] = 2;
    assert_eq!(modecfg::decode(&v), Err(DecodeError::Version));
    assert_eq!(modecfg::boot(None).reason, Some(DecodeError::Length));
}
