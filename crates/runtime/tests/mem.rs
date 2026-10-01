//! The memory routines against the standard library, exhaustively on small
//! sizes and with every overlap.

use nanox_runtime::mem::{compare, copy_backward, copy_forward, fill, move_bytes};

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
}

fn pattern(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed);
    (0..n).map(|_| r.next() as u8).collect()
}

#[test]
fn copy_forward_matches_a_slice_copy_at_every_size_and_alignment() {
    for n in 0..100 {
        for src_off in 0..9 {
            for dst_off in 0..9 {
                let src = pattern(n + 16, 1 + n as u64);
                let mut got = vec![0xEE; n + 16];
                let mut want = got.clone();
                unsafe {
                    copy_forward(got.as_mut_ptr().add(dst_off), src.as_ptr().add(src_off), n)
                };
                want[dst_off..dst_off + n].copy_from_slice(&src[src_off..src_off + n]);
                assert_eq!(got, want, "n {n} src {src_off} dst {dst_off}");
            }
        }
    }
}

#[test]
fn move_handles_every_overlap() {
    // A 64-byte window; every source, destination and length inside it.
    for src in 0..32usize {
        for dst in 0..32usize {
            for n in 0..=(64 - src.max(dst)).min(32) {
                let mut got = pattern(64, 99);
                let mut want = got.clone();
                unsafe { move_bytes(got.as_mut_ptr().add(dst), got.as_ptr().add(src), n) };
                want.copy_within(src..src + n, dst);
                assert_eq!(got, want, "src {src} dst {dst} n {n}");
            }
        }
    }
}

#[test]
fn backward_copy_matches_and_leaves_the_direction_flag_clear() {
    for n in 0..70 {
        let mut got = pattern(200, n as u64);
        let mut want = got.clone();
        let (src, dst) = (10usize, 10 + n / 2 + 1);
        unsafe { copy_backward(got.as_mut_ptr().add(dst), got.as_ptr().add(src), n) };
        want.copy_within(src..src + n, dst);
        assert_eq!(got, want, "n {n}");
    }
    #[cfg(target_arch = "x86_64")]
    {
        let mut buf = [1u8; 64];
        unsafe { copy_backward(buf.as_mut_ptr().add(8), buf.as_ptr(), 40) };
        let flags: u64;
        unsafe { std::arch::asm!("pushfq", "pop {}", out(reg) flags) };
        assert_eq!(
            flags & (1 << 10),
            0,
            "the direction flag must be clear (C ABI)"
        );
    }
}

#[test]
fn fill_matches_at_every_size_and_alignment() {
    for n in 0..100 {
        for off in 0..9 {
            for value in [0u8, 1, 0x7F, 0x80, 0xFF] {
                let mut got = vec![0x55u8; n + 16];
                let mut want = got.clone();
                unsafe { fill(got.as_mut_ptr().add(off), value, n) };
                want[off..off + n].fill(value);
                assert_eq!(got, want, "n {n} off {off} value {value}");
            }
        }
    }
}

#[test]
fn compare_returns_the_difference_of_the_first_mismatch() {
    let a = [1u8, 2, 3, 200, 5];
    let mut b = a;
    assert_eq!(unsafe { compare(a.as_ptr(), b.as_ptr(), 5) }, 0);
    assert_eq!(unsafe { compare(a.as_ptr(), b.as_ptr(), 0) }, 0);
    b[3] = 10;
    assert_eq!(
        unsafe { compare(a.as_ptr(), b.as_ptr(), 5) },
        190,
        "unsigned bytes"
    );
    assert_eq!(unsafe { compare(b.as_ptr(), a.as_ptr(), 5) }, -190);
    assert_eq!(
        unsafe { compare(a.as_ptr(), b.as_ptr(), 3) },
        0,
        "mismatch beyond n is invisible"
    );
    b[0] = 0;
    assert!(
        unsafe { compare(a.as_ptr(), b.as_ptr(), 5) } > 0,
        "the first difference decides"
    );
    // Randomly: agrees in sign with slice comparison.
    let mut r = Rng(5);
    for _ in 0..2000 {
        let n = (r.next() % 40) as usize;
        let x = pattern(n, r.next());
        let mut y = x.clone();
        if n > 0 && r.next().is_multiple_of(2) {
            let i = (r.next() % n as u64) as usize;
            y[i] = y[i].wrapping_add(1 + (r.next() % 255) as u8);
        }
        let got = unsafe { compare(x.as_ptr(), y.as_ptr(), n) };
        assert_eq!(got.signum(), x.cmp(&y) as i32);
    }
}
