//! Properties of the svc crate (docs/specs/M11-SERVER.md).

use svc::modecfg::{boot, crc32, decode, encode, DecodeError, LEN};
use svc::share::{allocate, Claim, ShareError, TOTAL};
use svc::Mode;

use crate::{Proof, Property};

pub fn properties() -> Vec<Property> {
    vec![
        Property {
            id: "svc.share.allocation-invariants",
            version: 1,
            statement: "allocate respects every claimant's guarantee min(reserve, bound) \
                        and its bound min(cap, demand), never exceeds the machine, hands \
                        the whole machine out whenever a claimant with positive weight can \
                        still take more, and reports Overcommitted exactly when the \
                        guarantees alone exceed the machine.",
            bound: "0..2 claimants over a grid of 8 reserves x 6 weights x 6 caps x 6 \
                    demands each; 3 claimants over 5 x 3 x 4 x 3 each (TOTAL = 1000)",
            component: &["crates/svc/src/share.rs"],
            checker: "crates/proofs/src/props/svc.rs",
            run: share_invariants,
        },
        Property {
            id: "svc.modecfg.corruption-is-detected",
            version: 1,
            statement: "A stored mode record that has any 1 to 4 bits flipped, or any \
                        length other than 12, is rejected, and boot then chooses safe \
                        server mode; every intact record decodes to its mode.",
            bound: "all 3 modes; every subset of 1..=4 of the 96 record bits flipped \
                    (3,469,496 records per mode); lengths 0..=40 except 12; no stored record",
            component: &["crates/svc/src/modecfg.rs"],
            checker: "crates/proofs/src/props/svc.rs",
            run: modecfg_corruption,
        },
    ]
}

fn grid(reserves: &[u32], weights: &[u32], caps: &[u32], demands: &[u32]) -> Vec<Claim> {
    let mut v = Vec::new();
    for &reserve in reserves {
        for &weight in weights {
            for &cap in caps {
                for &demand in demands {
                    v.push(Claim {
                        reserve,
                        weight,
                        cap,
                        demand,
                    });
                }
            }
        }
    }
    v
}

fn check_share(claims: &[Claim]) -> Result<(), String> {
    let bound = |c: &Claim| c.cap.min(c.demand).min(TOTAL);
    let floor = |c: &Claim| c.reserve.min(bound(c));
    let floors: u32 = claims.iter().map(floor).sum();
    let mut out = [0u32; 4];
    let res = allocate(claims, &mut out);
    if floors > TOTAL {
        return if res == Err(ShareError::Overcommitted) {
            Ok(())
        } else {
            Err(format!("guarantees {floors} > {TOTAL} but {res:?}"))
        };
    }
    if res != Ok(()) {
        return Err(format!("guarantees fit but {res:?}"));
    }
    let mut sum = 0;
    let mut room_left = false;
    for (c, &o) in claims.iter().zip(out.iter()) {
        if o < floor(c) {
            return Err(format!("guarantee broken: {o} < {}", floor(c)));
        }
        if o > bound(c) {
            return Err(format!("bound exceeded: {o} > {}", bound(c)));
        }
        sum += o;
        room_left |= c.weight > 0 && o < bound(c);
    }
    if sum > TOTAL {
        return Err(format!("machine oversubscribed: {sum}"));
    }
    if room_left && sum != TOTAL {
        return Err(format!(
            "not work-conserving: {sum} handed out with room left"
        ));
    }
    if !claims.is_empty()
        && allocate(claims, &mut out[..claims.len() - 1]) != Err(ShareError::Buffer)
    {
        return Err("a short output buffer was accepted".to_string());
    }
    Ok(())
}

fn share_invariants() -> Proof {
    let big = grid(
        &[0, 1, 2, 250, 333, 500, 999, 1000],
        &[0, 1, 2, 3, 5, 7],
        &[0, 1, 2, 333, 500, 1000],
        &[0, 1, 2, 400, 500, 1000],
    );
    let small = grid(
        &[0, 1, 250, 500, 1000],
        &[0, 1, 3],
        &[0, 1, 333, 1000],
        &[0, 400, 1000],
    );
    let mut cases = 0;
    let mut check = |cs: &[Claim]| -> Result<(), Proof> {
        cases += 1;
        check_share(cs).map_err(|e| Proof::failed(cases, format!("{cs:?}: {e}")))
    };
    let res = (|| {
        check(&[])?;
        for a in &big {
            check(&[*a])?;
            for b in &big {
                check(&[*a, *b])?;
            }
        }
        for a in &small {
            for b in &small {
                for c in &small {
                    check(&[*a, *b, *c])?;
                }
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => Proof::held(cases),
        Err(p) => p,
    }
}

fn flip(rec: &mut [u8; LEN], bit: usize) {
    rec[bit / 8] ^= 1 << (bit % 8);
}

fn modecfg_corruption() -> Proof {
    let mut cases = 1;
    if crc32(b"123456789") != 0xCBF4_3926 {
        return Proof::failed(cases, "CRC-32 check value".to_string());
    }
    let bad_boot = |bytes: &[u8]| {
        let b = boot(Some(bytes));
        b.safe && b.mode == Mode::Server && b.reason.is_some()
    };
    for mode in [Mode::Desktop, Mode::Hybrid, Mode::Server] {
        let rec = encode(mode);
        cases += 1;
        if decode(&rec) != Ok(mode) || boot(Some(&rec)).mode != mode || boot(Some(&rec)).safe {
            return Proof::failed(cases, format!("intact record for {mode:?}"));
        }
        let mut check = |r: &[u8; LEN], what: &str| -> Option<Proof> {
            cases += 1;
            (decode(r).is_ok() || !bad_boot(r))
                .then(|| Proof::failed(cases, format!("{mode:?} {what} {r:02x?}")))
        };
        for a in 0..96 {
            let mut r1 = rec;
            flip(&mut r1, a);
            if let Some(p) = check(&r1, "1 flip") {
                return p;
            }
            for b in a + 1..96 {
                let mut r2 = r1;
                flip(&mut r2, b);
                if let Some(p) = check(&r2, "2 flips") {
                    return p;
                }
                for c in b + 1..96 {
                    let mut r3 = r2;
                    flip(&mut r3, c);
                    if let Some(p) = check(&r3, "3 flips") {
                        return p;
                    }
                    for d in c + 1..96 {
                        let mut r4 = r3;
                        flip(&mut r4, d);
                        if let Some(p) = check(&r4, "4 flips") {
                            return p;
                        }
                    }
                }
            }
        }
        for len in (0..=40).filter(|l| *l != LEN) {
            let bytes: Vec<u8> = rec.iter().copied().cycle().take(len).collect();
            cases += 1;
            if decode(&bytes) != Err(DecodeError::Length) || !bad_boot(&bytes) {
                return Proof::failed(cases, format!("length {len}"));
            }
        }
    }
    cases += 1;
    let none = boot(None);
    if !(none.safe && none.mode == Mode::Server) {
        return Proof::failed(cases, "no stored record".to_string());
    }
    Proof::held(cases)
}
