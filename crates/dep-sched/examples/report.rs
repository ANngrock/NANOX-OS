//! Measures dependency-aware scheduling against the declaration-order
//! baseline: synthetic graph families, then this workspace crate graph.
//!
//!   cargo run --release -p dep-sched --example report -- [--root DIR]
//!
//! Prints Markdown. Everything is simulated time units: no claim here about
//! wall-clock build times except where the cost file says it was measured.

// Graph code indexes tasks by number (they are bits in masks); iterator
// chains would hide that.
#![allow(clippy::needless_range_loop)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use dep_sched::{lower_bound, schedule, validate, Policy, Task};

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
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Layered,
    Random,
    ForkJoin,
    OutTree,
    InTree,
}

const SHAPES: [(Shape, &str); 5] = [
    (Shape::Layered, "layered"),
    (Shape::Random, "random"),
    (Shape::ForkJoin, "fork-join"),
    (Shape::OutTree, "out-tree"),
    (Shape::InTree, "in-tree"),
];

#[derive(Clone, Copy, PartialEq)]
enum Costs {
    Uniform,
    Bimodal,
}

fn cost(rng: &mut Rng, c: Costs) -> u32 {
    match c {
        Costs::Uniform => rng.range(1, 10) as u32,
        Costs::Bimodal => {
            if rng.below(5) == 0 {
                rng.range(20, 40) as u32
            } else {
                rng.range(1, 3) as u32
            }
        }
    }
}

/// A graph whose indices are a topological order (dependencies first).
fn generate(rng: &mut Rng, shape: Shape, n: usize, c: Costs) -> Vec<Task> {
    let mut t: Vec<Task> = (0..n)
        .map(|_| Task {
            cost: cost(rng, c),
            deps: 0,
        })
        .collect();
    match shape {
        Shape::Layered => {
            let mut prev: Vec<usize> = Vec::new();
            let mut i = 0;
            while i < n {
                let width = (rng.range(1, 6) as usize).min(n - i);
                let layer: Vec<usize> = (i..i + width).collect();
                for &v in &layer {
                    for _ in 0..rng.range(1, 3) {
                        if !prev.is_empty() {
                            t[v].deps |= 1 << prev[rng.below(prev.len() as u64) as usize];
                        }
                    }
                }
                prev = layer;
                i += width;
            }
        }
        Shape::Random => {
            for i in 0..n {
                for j in 0..i {
                    if rng.below(1000) < 70 {
                        t[i].deps |= 1 << j;
                    }
                }
            }
        }
        Shape::ForkJoin => {
            let chains = rng.range(3, 8) as usize;
            let mut last = vec![0usize; chains];
            for i in 1..n - 1 {
                let c = (i - 1) % chains;
                t[i].deps |= 1 << last[c];
                last[c] = i;
            }
            for l in last {
                t[n - 1].deps |= 1 << l;
            }
        }
        Shape::OutTree => {
            for i in 1..n {
                t[i].deps |= 1 << rng.below(i as u64);
            }
        }
        Shape::InTree => {
            for i in 0..n - 1 {
                let parent = rng.range(i as u64 + 1, n as u64 - 1) as usize;
                t[parent].deps |= 1 << i;
            }
        }
    }
    t
}

/// The same graph with the declaration order shuffled.
fn relabel(rng: &mut Rng, t: &[Task]) -> Vec<Task> {
    let n = t.len();
    let mut perm: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        perm.swap(i, rng.below(i as u64 + 1) as usize);
    }
    let mut out = vec![Task { cost: 0, deps: 0 }; n];
    for (old, task) in t.iter().enumerate() {
        let mut deps = 0u64;
        for d in 0..n {
            if task.deps >> d & 1 == 1 {
                deps |= 1 << perm[d];
            }
        }
        out[perm[old]] = Task {
            cost: task.cost,
            deps,
        };
    }
    out
}

fn makespan(t: &[Task], m: usize, p: Policy) -> u64 {
    let s = schedule(t, m, p).expect("acyclic");
    validate(t, m, &s).expect("valid schedule");
    s.makespan
}

#[derive(Default)]
struct Stats {
    n: u32,
    fifo_ratio: f64,
    cp_ratio: f64,
    better: u32,
    equal: u32,
    worse: u32,
    gain: f64,
    worst_regression: f64,
}

impl Stats {
    fn add(&mut self, fifo: u64, cp: u64, lb: u64) {
        self.n += 1;
        self.fifo_ratio += fifo as f64 / lb.max(1) as f64;
        self.cp_ratio += cp as f64 / lb.max(1) as f64;
        self.gain += (fifo as f64 - cp as f64) / fifo.max(1) as f64;
        match cp.cmp(&fifo) {
            std::cmp::Ordering::Less => self.better += 1,
            std::cmp::Ordering::Equal => self.equal += 1,
            std::cmp::Ordering::Greater => {
                self.worse += 1;
                self.worst_regression = self
                    .worst_regression
                    .max((cp as f64 - fifo as f64) / fifo as f64);
            }
        }
    }
}

fn synthetic(out: &mut String) {
    let instances = 1000;
    let n = 48;
    writeln!(
        out,
        "### Synthetic graphs ({n} tasks, {instances} instances per row)\n"
    )
    .unwrap();
    writeln!(
        out,
        "| shape | costs | labels | workers | FIFO/LB | CP/LB | CP better | equal | worse | mean gain | worst loss |\n|---|---|---|---|---|---|---|---|---|---|---|"
    )
    .unwrap();
    let mut total = Stats::default();
    let mut rng = Rng(0x5EED_0001);
    for (shape, name) in SHAPES {
        for (costs, cname) in [
            (Costs::Uniform, "uniform 1-10"),
            (Costs::Bimodal, "bimodal"),
        ] {
            for shuffled in [false, true] {
                for m in [2usize, 4, 8] {
                    let mut st = Stats::default();
                    for _ in 0..instances {
                        let mut g = generate(&mut rng, shape, n, costs);
                        if shuffled {
                            g = relabel(&mut rng, &g);
                        }
                        let lb = lower_bound(&g, m).unwrap();
                        let (f, c) = (
                            makespan(&g, m, Policy::Fifo),
                            makespan(&g, m, Policy::CriticalPath),
                        );
                        st.add(f, c, lb);
                        total.add(f, c, lb);
                    }
                    let k = f64::from(st.n);
                    writeln!(
                        out,
                        "| {name} | {cname} | {} | {m} | {:.3} | {:.3} | {:.1}% | {:.1}% | {:.1}% | {:.1}% | {:.1}% |",
                        if shuffled { "shuffled" } else { "declared" },
                        st.fifo_ratio / k,
                        st.cp_ratio / k,
                        100.0 * f64::from(st.better) / k,
                        100.0 * f64::from(st.equal) / k,
                        100.0 * f64::from(st.worse) / k,
                        100.0 * st.gain / k,
                        100.0 * st.worst_regression,
                    )
                    .unwrap();
                }
            }
        }
    }
    let k = f64::from(total.n);
    writeln!(
        out,
        "\nAll {} instances: FIFO/LB {:.3}, CP/LB {:.3}; critical path better on {:.1}%, equal on {:.1}%, worse on {:.1}% (worst loss {:.1}%); mean gain {:.1}%.\n",
        total.n,
        total.fifo_ratio / k,
        total.cp_ratio / k,
        100.0 * f64::from(total.better) / k,
        100.0 * f64::from(total.equal) / k,
        100.0 * f64::from(total.worse) / k,
        100.0 * total.worst_regression,
        100.0 * total.gain / k,
    )
    .unwrap();
}

struct Member {
    name: String,
    dir: PathBuf,
    deps: Vec<PathBuf>,
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn quoted(line: &str, key: &str) -> Option<String> {
    let at = line.find(key)?;
    let rest = &line[at + key.len()..];
    let start = rest.find('"')? + 1;
    let end = start + rest[start..].find('"')?;
    Some(rest[start..end].to_string())
}

fn members(root: &Path) -> Vec<Member> {
    let ws = fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml");
    let line = ws
        .lines()
        .find(|l| l.starts_with("members"))
        .expect("members line");
    let list = &line[line.find('[').unwrap() + 1..line.find(']').unwrap()];
    let mut out = Vec::new();
    for dir in list
        .split(',')
        .map(|s| s.trim().trim_matches('"'))
        .filter(|s| !s.is_empty())
    {
        let text = fs::read_to_string(root.join(dir).join("Cargo.toml")).expect("member manifest");
        let (mut section, mut name, mut deps) = (String::new(), String::new(), Vec::new());
        for l in text.lines() {
            let l = l.trim();
            if l.starts_with('[') {
                section = l.to_string();
            } else if section == "[package]" && l.starts_with("name") && name.is_empty() {
                name = quoted(l, "name").unwrap_or_default();
            } else if section.ends_with("dependencies]") {
                if let Some(p) = quoted(l, "path") {
                    deps.push(normalize(&Path::new(dir).join(p)));
                }
            }
        }
        out.push(Member {
            name,
            dir: normalize(Path::new(dir)),
            deps,
        });
    }
    out
}

fn loc(dir: &Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![dir.join("src")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = fs::read_to_string(&p).unwrap_or_default();
                n += text
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
                    .count() as u64;
            }
        }
    }
    n
}

fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let cov: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let vx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    let vy: f64 = ys.iter().map(|y| (y - my).powi(2)).sum();
    cov / (vx.sqrt() * vy.sqrt())
}

fn workspace(out: &mut String, root: &Path) {
    let ms = members(root);
    let measured: Vec<(String, u64)> =
        fs::read_to_string(root.join("docs/research/dep-sched-costs.tsv"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some((f.next()?.to_string(), f.next()?.parse().ok()?))
            })
            .collect();
    let locs: Vec<u64> = ms.iter().map(|m| loc(&root.join(&m.dir))).collect();
    let meas = |name: &str| measured.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
    let use_measured = ms.iter().all(|m| meas(&m.name).is_some());
    let costs: Vec<u64> = ms
        .iter()
        .zip(&locs)
        .map(|(m, l)| {
            if use_measured {
                meas(&m.name).unwrap().max(1)
            } else {
                (*l).max(1)
            }
        })
        .collect();
    let tasks: Vec<Task> = ms
        .iter()
        .zip(&costs)
        .map(|(m, c)| {
            let deps = m
                .deps
                .iter()
                .filter_map(|d| ms.iter().position(|o| &o.dir == d))
                .fold(0u64, |mask, i| mask | 1 << i);
            Task {
                cost: (*c).min(u64::from(u32::MAX)) as u32,
                deps,
            }
        })
        .collect();
    writeln!(out, "### This workspace ({} crates)\n", ms.len()).unwrap();
    let edges: u32 = tasks.iter().map(|t| t.deps.count_ones()).sum();
    writeln!(
        out,
        "Task graph: the workspace members in declaration order, path dependencies (including dev-dependencies) as edges: {edges} edges. Cost: {}.\n",
        if use_measured {
            "measured wall-clock milliseconds to compile the crate alone (docs/research/dep-sched-costs.tsv)"
        } else {
            "non-blank non-comment source lines, a proxy for compile time"
        }
    )
    .unwrap();
    let pairs: Vec<(f64, f64)> = ms
        .iter()
        .zip(&locs)
        .filter_map(|(m, l)| meas(&m.name).map(|v| (*l as f64, v as f64)))
        .collect();
    if pairs.len() >= 5 {
        let (xs, ys): (Vec<f64>, Vec<f64>) = pairs.iter().copied().unzip();
        writeln!(
            out,
            "Source lines against measured compile time over {} crates: correlation r = {:.2}.\n",
            pairs.len(),
            pearson(&xs, &ys)
        )
        .unwrap();
    }
    writeln!(out, "| workers | FIFO (declared) | FIFO (shuffled, mean of 200) | critical path | lower bound |\n|---|---|---|---|---|").unwrap();
    let mut rng = Rng(0xC0DE);
    for m in [1usize, 2, 4, 8, 16] {
        let shuffled: f64 = (0..200)
            .map(|_| makespan(&relabel(&mut rng, &tasks), m, Policy::Fifo) as f64)
            .sum::<f64>()
            / 200.0;
        writeln!(
            out,
            "| {m} | {} | {shuffled:.0} | {} | {} |",
            makespan(&tasks, m, Policy::Fifo),
            makespan(&tasks, m, Policy::CriticalPath),
            lower_bound(&tasks, m).unwrap()
        )
        .unwrap();
    }
    writeln!(out).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args
        .iter()
        .position(|a| a == "--root")
        .and_then(|i| args.get(i + 1))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let mut out =
        String::from("## Dependency scheduling against the declaration-order baseline\n\n");
    synthetic(&mut out);
    workspace(&mut out, &root);
    print!("{out}");
}
