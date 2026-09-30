//! What does NIR cost against the alternatives, on policies of the kind the
//! system would run? Each policy exists as a native Rust function (the
//! trusted-build baseline), as a rule table where that is natural (the usual
//! configuration-driven design), and as a NIR program run by evaluator A, by
//! evaluator B, and by evaluator B after optimization. All implementations
//! are cross-checked on every input before anything is timed.
//!
//!   cargo run --release -p nanox-ir --example bench
//!
//! Prints Markdown (docs/research/ISA-DECISION.md).

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use nanox_ir::opt::{optimize, Compiled};
use nanox_ir::{Insn, Op, Program};

/// A tiny assembler with forward labels.
enum A {
    I(Insn),
    Jz(u8, &'static str),
    Jnz(u8, &'static str),
    L(&'static str),
}

fn assemble(code: &[A]) -> Vec<Insn> {
    let mut at = HashMap::new();
    let mut pc = 0;
    for a in code {
        match a {
            A::L(name) => {
                at.insert(*name, pc);
            }
            _ => pc += 1,
        }
    }
    code.iter()
        .filter_map(|a| match a {
            A::I(i) => Some(*i),
            A::Jz(r, l) => Some(Insn::jz(*r, at[l])),
            A::Jnz(r, l) => Some(Insn::jnz(*r, at[l])),
            A::L(_) => None,
        })
        .collect()
}

fn b(op: Op, d: u8, x: u8, y: u8) -> A {
    A::I(Insn::binary(op, d, x, y))
}
fn k(d: u8, v: u64) -> A {
    A::I(Insn::constant(d, v))
}
fn inp(d: u8, i: u64) -> A {
    A::I(Insn::input(d, i))
}
fn ret(r: u8) -> A {
    A::I(Insn::ret(r))
}

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

// ---- policy 1: service admission (configuration constants baked in) ----

const MAX_MEM: u64 = 1 << 30;
const MODE: u64 = 1; // hybrid; the server-only rule below is dead code

fn admission(i: &[u64; 6]) -> u64 {
    let (cpu, mem, cpu_free, mem_free, prio, safe) = (i[0], i[1], i[2], i[3], i[4], i[5]);
    if mem > MAX_MEM {
        return 0;
    }
    if MODE == 2 && prio < 1 {
        return 0;
    }
    if safe != 0 && prio < 3 {
        return 0;
    }
    if cpu <= cpu_free && mem <= mem_free {
        return 2;
    }
    if prio < 5 {
        return 0;
    }
    1
}

fn admission_nir() -> Vec<Insn> {
    assemble(&[
        inp(1, 0),
        inp(2, 1),
        inp(3, 2),
        inp(4, 3),
        inp(5, 4),
        inp(6, 5),
        k(7, MAX_MEM),
        k(8, MODE),
        k(9, 2),
        b(Op::Ltu, 10, 7, 2),
        A::Jnz(10, "deny"),
        b(Op::Eq, 11, 8, 9),
        A::Jz(11, "skip"),
        k(12, 1),
        b(Op::Ltu, 10, 5, 12),
        A::Jnz(10, "deny"),
        A::L("skip"),
        A::Jz(6, "fit"),
        k(13, 3),
        b(Op::Ltu, 10, 5, 13),
        A::Jnz(10, "deny"),
        A::L("fit"),
        b(Op::Leu, 10, 1, 3),
        b(Op::Leu, 11, 2, 4),
        b(Op::And, 10, 10, 11),
        A::Jnz(10, "admit"),
        k(13, 5),
        b(Op::Ltu, 10, 5, 13),
        A::Jnz(10, "deny"),
        k(0, 1),
        ret(0),
        A::L("deny"),
        k(0, 0),
        ret(0),
        A::L("admit"),
        k(0, 2),
        ret(0),
    ])
}

fn admission_inputs(r: &mut Rng) -> [u64; 6] {
    [
        r.next() % 64,
        r.next() % (1 << 31),
        r.next() % 64,
        r.next() % (1 << 31),
        r.next() % 8,
        r.next() % 2,
    ]
}

// ---- policy 2: frame filter, also as a rule table ----

fn filter(i: &[u64; 5]) -> u64 {
    let (et, vlan, port, proto, flags) = (i[0], i[1], i[2], i[3], i[4]);
    if et == 0x0806 {
        return 1;
    }
    if et == 0x0800 && proto == 6 && flags & 2 == 0 && matches!(port, 80 | 443 | 8006 | 22) {
        return 1;
    }
    u64::from(vlan == 10 && proto == 17 && port.wrapping_sub(5000) < 100)
}

fn filter_nir() -> Vec<Insn> {
    assemble(&[
        inp(1, 0),
        inp(2, 1),
        inp(3, 2),
        inp(4, 3),
        inp(5, 4),
        k(6, 0x0806),
        b(Op::Eq, 7, 1, 6),
        A::Jnz(7, "allow"),
        k(6, 0x0800),
        b(Op::Eq, 7, 1, 6),
        A::Jz(7, "udp"),
        k(6, 6),
        b(Op::Eq, 7, 4, 6),
        A::Jz(7, "udp"),
        k(6, 2),
        b(Op::And, 7, 5, 6),
        A::Jnz(7, "udp"),
        k(6, 80),
        b(Op::Eq, 7, 3, 6),
        A::Jnz(7, "allow"),
        k(6, 443),
        b(Op::Eq, 7, 3, 6),
        A::Jnz(7, "allow"),
        k(6, 8006),
        b(Op::Eq, 7, 3, 6),
        A::Jnz(7, "allow"),
        k(6, 22),
        b(Op::Eq, 7, 3, 6),
        A::Jnz(7, "allow"),
        A::L("udp"),
        k(6, 10),
        b(Op::Eq, 7, 2, 6),
        A::Jz(7, "deny"),
        k(6, 17),
        b(Op::Eq, 7, 4, 6),
        A::Jz(7, "deny"),
        k(6, 5000),
        b(Op::Sub, 7, 3, 6),
        k(6, 100),
        b(Op::Ltu, 7, 7, 6),
        A::Jz(7, "deny"),
        A::L("allow"),
        k(0, 1),
        ret(0),
        A::L("deny"),
        k(0, 0),
        ret(0),
    ])
}

#[derive(Clone, Copy)]
enum Cond {
    Eq(usize, u64),
    MaskEq(usize, u64, u64),
    Range(usize, u64, u64),
}

/// First matching rule wins; every condition of a rule must hold.
fn rule_table(rules: &[(&[Cond], u64)], i: &[u64; 5]) -> u64 {
    'rules: for (conds, verdict) in rules {
        for c in *conds {
            let ok = match *c {
                Cond::Eq(f, v) => i[f] == v,
                Cond::MaskEq(f, m, v) => i[f] & m == v,
                Cond::Range(f, lo, hi) => (lo..=hi).contains(&i[f]),
            };
            if !ok {
                continue 'rules;
            }
        }
        return *verdict;
    }
    0
}

fn filter_inputs(r: &mut Rng) -> [u64; 5] {
    let ports = [80, 443, 8006, 22, 5050, 25, 53, 8080];
    [
        [0x0800, 0x0806, 0x86DD][(r.next() % 3) as usize],
        [10, 20, 0][(r.next() % 3) as usize],
        ports[(r.next() % 8) as usize],
        [6, 17, 1][(r.next() % 3) as usize],
        r.next() % 4,
    ]
}

// ---- policies 3 and 4: branch-free arithmetic ----

fn score(i: &[u64; 6]) -> u64 {
    let s = (i[0]
        .wrapping_mul(3)
        .wrapping_add(i[1].wrapping_mul(5))
        .wrapping_add(i[2].wrapping_mul(7))
        .wrapping_add(i[3])
        ^ i[4])
        >> 2;
    let class = (s / 37) % 8;
    class.min(6) + u64::from((i[5] & 0xFF).count_ones())
}

fn score_nir() -> Vec<Insn> {
    assemble(&[
        inp(1, 0),
        inp(2, 1),
        inp(3, 2),
        inp(4, 3),
        inp(5, 4),
        inp(6, 5),
        k(7, 3),
        b(Op::Mul, 8, 1, 7),
        k(7, 5),
        b(Op::Mul, 9, 2, 7),
        b(Op::Add, 8, 8, 9),
        k(7, 7),
        b(Op::Mul, 9, 3, 7),
        b(Op::Add, 8, 8, 9),
        b(Op::Add, 8, 8, 4),
        b(Op::Xor, 8, 8, 5),
        k(7, 2),
        b(Op::Shr, 8, 8, 7),
        k(7, 37),
        b(Op::Udiv, 9, 8, 7),
        k(7, 8),
        b(Op::Urem, 9, 9, 7),
        k(7, 255),
        b(Op::And, 10, 6, 7),
        A::I(Insn::unary(Op::Popcnt, 10, 10)),
        k(7, 6),
        b(Op::Minu, 9, 9, 7),
        b(Op::Add, 0, 9, 10),
        ret(0),
    ])
}

fn place(i: &[u64; 4]) -> u64 {
    let (mut best, mut load) = (0, i[0]);
    for (j, &l) in i.iter().enumerate().skip(1) {
        if l < load {
            best = j as u64;
            load = l;
        }
    }
    best
}

fn place_nir() -> Vec<Insn> {
    let mut v = vec![
        inp(1, 0),
        inp(2, 1),
        inp(3, 2),
        inp(4, 3),
        k(5, 0),
        A::I(Insn::unary(Op::Mov, 6, 1)),
    ];
    for j in 1..4u8 {
        v.push(b(Op::Ltu, 7, j + 1, 6));
        v.push(k(8, u64::from(j)));
        v.push(A::I(Insn::sel(5, 7, 8, 5)));
        v.push(A::I(Insn::sel(6, 7, j + 1, 6)));
    }
    v.push(ret(5));
    assemble(&v)
}

// ---- harness ----

fn time<F: FnMut(&[u64; 8]) -> u64>(inputs: &[[u64; 8]], total: usize, mut f: F) -> f64 {
    let reps = total / inputs.len();
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let mut acc = 0u64;
        for _ in 0..reps {
            for x in inputs {
                acc = acc.wrapping_add(f(black_box(x)));
            }
        }
        black_box(acc);
        best = best.min(t.elapsed().as_secs_f64() * 1e9 / (reps * inputs.len()) as f64);
    }
    best
}

fn run_policy<N, T>(
    name: &str,
    insns: Vec<Insn>,
    inputs: Vec<[u64; 8]>,
    native: N,
    table: Option<T>,
) where
    N: Fn(&[u64; 8]) -> u64,
    T: Fn(&[u64; 8]) -> u64,
{
    let prog = Program::new(&insns, 64).expect("program verifies");
    let opt = optimize(&prog).expect("optimizes");
    let (plain, fast) = (Compiled::new(&prog), Compiled::new(&opt));
    // Cross-check everything first.
    let (mut steps_a, mut steps_o) = (0usize, 0usize);
    let mut results = std::collections::BTreeMap::new();
    for x in &inputs {
        let want = native(x);
        let a = prog.eval(x);
        let o = opt.eval(x);
        assert_eq!(a.value, want, "{name}: A differs from native on {x:?}");
        assert_eq!(o.value, want, "{name}: optimized differs on {x:?}");
        assert_eq!(plain.run(x).value, want, "{name}: B differs on {x:?}");
        assert_eq!(
            fast.run(x).value,
            want,
            "{name}: B optimized differs on {x:?}"
        );
        if let Some(t) = &table {
            assert_eq!(t(x), want, "{name}: rule table differs on {x:?}");
        }
        steps_a += a.steps;
        steps_o += o.steps;
        *results.entry(want).or_insert(0usize) += 1;
    }
    let n = inputs.len() as f64;
    let native_ns = time(&inputs, 40_000_000, |x| native(x));
    let table_ns = table.as_ref().map(|t| time(&inputs, 40_000_000, |x| t(x)));
    let a_ns = time(&inputs, 4_000_000, |x| prog.eval(x).value);
    let b_ns = time(&inputs, 4_000_000, |x| plain.run(x).value);
    let o_ns = time(&inputs, 4_000_000, |x| fast.run(x).value);
    println!(
        "| {name} | {} to {} | {:.1} to {:.1} | {native_ns:.1} | {} | {a_ns:.1} | {b_ns:.1} | {o_ns:.1} | {:.0}x |",
        prog.insns().len(),
        opt.insns().len(),
        steps_a as f64 / n,
        steps_o as f64 / n,
        table_ns.map_or("n/a".to_string(), |t| format!("{t:.1}")),
        o_ns / native_ns,
    );
    let spread: Vec<String> = results.iter().map(|(k, v)| format!("{k}:{v}")).collect();
    eprintln!(
        "{name}: result distribution over {} inputs: {}",
        inputs.len(),
        spread.join(" ")
    );
}

fn main() {
    let mut rng = Rng(0xBE4C_0001);
    let count = 4096;
    let mut gen = |f: &mut dyn FnMut(&mut Rng) -> Vec<u64>| -> Vec<[u64; 8]> {
        (0..count)
            .map(|_| {
                let v = f(&mut rng);
                let mut a = [0u64; 8];
                a[..v.len()].copy_from_slice(&v);
                a
            })
            .collect()
    };
    println!("| policy | instructions | steps per run | native Rust ns | rule table ns | NIR A ns | NIR B ns | NIR B optimized ns | B opt / native |");
    println!("|---|---|---|---|---|---|---|---|---|");
    run_policy(
        "service admission",
        admission_nir(),
        gen(&mut |r| admission_inputs(r).to_vec()),
        |x| admission(&[x[0], x[1], x[2], x[3], x[4], x[5]]),
        None::<fn(&[u64; 8]) -> u64>,
    );
    let rules: Vec<(Vec<Cond>, u64)> = vec![
        (vec![Cond::Eq(0, 0x0806)], 1),
        (
            vec![
                Cond::Eq(0, 0x0800),
                Cond::Eq(3, 6),
                Cond::MaskEq(4, 2, 0),
                Cond::Eq(2, 80),
            ],
            1,
        ),
        (
            vec![
                Cond::Eq(0, 0x0800),
                Cond::Eq(3, 6),
                Cond::MaskEq(4, 2, 0),
                Cond::Eq(2, 443),
            ],
            1,
        ),
        (
            vec![
                Cond::Eq(0, 0x0800),
                Cond::Eq(3, 6),
                Cond::MaskEq(4, 2, 0),
                Cond::Eq(2, 8006),
            ],
            1,
        ),
        (
            vec![
                Cond::Eq(0, 0x0800),
                Cond::Eq(3, 6),
                Cond::MaskEq(4, 2, 0),
                Cond::Eq(2, 22),
            ],
            1,
        ),
        (
            vec![Cond::Eq(1, 10), Cond::Eq(3, 17), Cond::Range(2, 5000, 5099)],
            1,
        ),
    ];
    let table: Vec<(&[Cond], u64)> = rules.iter().map(|(c, v)| (c.as_slice(), *v)).collect();
    run_policy(
        "frame filter",
        filter_nir(),
        gen(&mut |r| filter_inputs(r).to_vec()),
        |x| filter(&[x[0], x[1], x[2], x[3], x[4]]),
        Some(|x: &[u64; 8]| rule_table(&table, &[x[0], x[1], x[2], x[3], x[4]])),
    );
    run_policy(
        "score (arithmetic)",
        score_nir(),
        gen(&mut |r| (0..6).map(|_| r.next()).collect()),
        |x| score(&[x[0], x[1], x[2], x[3], x[4], x[5]]),
        None::<fn(&[u64; 8]) -> u64>,
    );
    run_policy(
        "placement (select)",
        place_nir(),
        gen(&mut |r| (0..4).map(|_| r.next() % 1000).collect()),
        |x| place(&[x[0], x[1], x[2], x[3]]),
        None::<fn(&[u64; 8]) -> u64>,
    );
}
