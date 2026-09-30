//! Both Rust implementations against the independent Python reference
//! (tools/nir, fixtures generated from the written semantics).

use nanox_ir::opt::{alu_b, Compiled};
use nanox_ir::{alu, DecodeError, Op, Program, VerifyError, ALL_OPS};

const ALU_FIX: &str = include_str!("fixtures/alu.txt");
const PROG_FIX: &str = include_str!("fixtures/programs.txt");

const FNV_OFF: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
const EDGES: [u64; 22] = [
    0,
    1,
    2,
    3,
    7,
    8,
    0x7F,
    0x80,
    0xFF,
    0x100,
    0x7FFF_FFFF,
    0x8000_0000,
    0xFFFF_FFFF,
    0x1_0000_0000,
    0x7FFF_FFFF_FFFF_FFFF,
    0x8000_0000_0000_0000,
    0x8000_0000_0000_0001,
    0xFFFF_FFFF_FFFF_FFFE,
    0xFFFF_FFFF_FFFF_FFFF,
    63,
    64,
    65,
];

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

fn op_name(op: Op) -> &'static str {
    [
        "ret", "const", "in", "mov", "add", "sub", "mul", "udiv", "urem", "and", "or", "xor",
        "shl", "shr", "sar", "eq", "ne", "ltu", "leu", "lts", "les", "not", "neg", "sel", "minu",
        "maxu", "popcnt", "jmp", "jz", "jnz",
    ][op as usize]
}

fn digest(f: impl Fn(u64, u64) -> u64, w: u32) -> u64 {
    let mut h = FNV_OFF;
    let mut feed = |v: u64, nbytes: usize| {
        for b in &v.to_le_bytes()[..nbytes] {
            h = (h ^ u64::from(*b)).wrapping_mul(FNV_PRIME);
        }
    };
    if w == 8 {
        for x in 0..256u64 {
            for y in 0..256u64 {
                feed(f(x, y), 1);
            }
        }
    } else {
        for x in EDGES {
            for y in EDGES {
                feed(f(x, y), 8);
            }
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..4096 {
            let (x, y) = (rng.next(), rng.next());
            feed(f(x, y), 8);
        }
    }
    h
}

fn parse_hex(s: &str) -> u64 {
    u64::from_str_radix(s, 16).unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

#[test]
fn alu_digests_match_the_python_reference() {
    let mut seen = 0;
    for line in ALU_FIX.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split(' ').collect();
        let w: u32 = f[0].parse().unwrap();
        let op = *ALL_OPS.iter().find(|o| op_name(**o) == f[1]).unwrap();
        let want = parse_hex(f[2]);
        assert_eq!(digest(|x, y| alu(op, x, y, w), w), want, "A {} w{w}", f[1]);
        assert_eq!(
            digest(|x, y| alu_b(op, x, y, w), w),
            want,
            "B {} w{w}",
            f[1]
        );
        seen += 1;
    }
    let alu_ops = ALL_OPS.iter().filter(|o| o.is_alu()).count();
    assert_eq!(seen, 2 * alu_ops, "every ALU op at both widths");
}

fn kind(e: DecodeError) -> &'static str {
    match e {
        DecodeError::Length => "length",
        DecodeError::Op(_) => "op",
        DecodeError::Padding(_) => "padding",
        DecodeError::Verify(v) => match v {
            VerifyError::Width => "width",
            VerifyError::Empty => "empty",
            VerifyError::TooLong => "toolong",
            VerifyError::NoFinalRet => "noret",
            VerifyError::Register(_) => "register",
            VerifyError::Input(_) => "input",
            VerifyError::JumpTarget(_) => "jump",
            VerifyError::Immediate(_) => "immediate",
            VerifyError::NotCanonical(_) => "canonical",
        },
    }
}

#[test]
fn programs_match_the_python_reference() {
    let mut cur: Option<(Program, u32)> = None;
    let (mut progs, mut vectors, mut bads) = (0, 0, 0);
    for line in PROG_FIX.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split(' ').collect();
        match f[0] {
            "prog" => {
                let w: u32 = f[1].parse().unwrap();
                let bytes = unhex(f[2]);
                let p = Program::decode(&bytes, w).unwrap();
                // The encoding is canonical: it re-encodes to the same bytes.
                let mut out = vec![0u8; bytes.len()];
                assert_eq!(p.encode(&mut out), Some(bytes.len()));
                assert_eq!(out, bytes);
                cur = Some((p, w));
                progs += 1;
            }
            "v" => {
                let (p, _) = cur.as_ref().unwrap();
                let inputs: Vec<u64> = f[1].split(',').map(parse_hex).collect();
                let (want, steps) = (parse_hex(f[2]), f[3].parse::<usize>().unwrap());
                let a = p.eval(&inputs);
                assert_eq!((a.value, a.steps), (want, steps), "A on {line}");
                let b = Compiled::new(p).run(&inputs);
                assert_eq!((b.value, b.steps), (want, steps), "B on {line}");
                let o = Compiled::optimized(p).unwrap().run(&inputs);
                assert_eq!(o.value, want, "B optimized on {line}");
                assert!(o.steps <= steps);
                vectors += 1;
            }
            "bad" => {
                let w: u32 = f[1].parse().unwrap();
                let err = Program::decode(&unhex(f[2]), w).err();
                assert_eq!(err.map(kind), Some(f[3]), "{line}");
                bads += 1;
            }
            other => panic!("unknown record {other}"),
        }
    }
    assert!(
        progs >= 200 && vectors >= 2400 && bads >= 500,
        "{progs} {vectors} {bads}"
    );
}
