//! Properties over many graphs: validity, the list-scheduling bound, the
//! lower bound, and exhaustive coverage of every small DAG.

use dep_sched::{lower_bound, ranks, schedule, schedule_with, validate, Policy, Task};

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

fn random_dag(rng: &mut Rng, n: usize, per_mille: u64, max_cost: u64) -> Vec<Task> {
    (0..n)
        .map(|i| {
            let mut deps = 0u64;
            for j in 0..i {
                if rng.below(1000) < per_mille {
                    deps |= 1 << j;
                }
            }
            Task {
                cost: rng.below(max_cost + 1) as u32,
                deps,
            }
        })
        .collect()
}

/// Every property that must hold for a greedy list schedule.
fn check(tasks: &[Task], m: usize) -> (u64, u64) {
    let total: u64 = tasks.iter().map(|t| u64::from(t.cost)).sum();
    let critical = *ranks(tasks).unwrap()[..tasks.len()]
        .iter()
        .max()
        .unwrap_or(&0);
    let lb = lower_bound(tasks, m).unwrap();
    let mut out = [0; 2];
    for (k, p) in [Policy::Fifo, Policy::CriticalPath].into_iter().enumerate() {
        let s = schedule(tasks, m, p).unwrap();
        validate(tasks, m, &s).unwrap_or_else(|e| panic!("{e}: {tasks:?} m={m} {p:?}"));
        assert!(s.makespan >= lb, "below the lower bound: {tasks:?}");
        // Graham: a greedy schedule is within W/m + (1 - 1/m) * critical path.
        assert!(
            m as u64 * s.makespan <= total + (m as u64 - 1) * critical,
            "Graham bound broken: {tasks:?} m={m} {p:?} makespan {}",
            s.makespan
        );
        if m == 1 {
            assert_eq!(s.makespan, total);
        }
        if m >= tasks.len() {
            assert_eq!(s.makespan, critical);
        }
        assert_eq!(
            schedule(tasks, m, p).unwrap().start[..tasks.len()],
            s.start[..tasks.len()]
        );
        out[k] = s.makespan;
    }
    (out[0], out[1])
}

#[test]
fn random_graphs_give_valid_bounded_schedules() {
    let mut rng = Rng(2024);
    for _ in 0..3000 {
        let n = 1 + rng.below(40) as usize;
        let density = [20, 60, 150, 400][rng.below(4) as usize];
        let g = random_dag(&mut rng, n, density, 12);
        for m in [1, 2, 3, 5, 8] {
            check(&g, m);
        }
    }
}

#[test]
fn every_small_dag_with_small_costs() {
    let mut instances = 0u64;
    for n in 1..=5usize {
        let pairs: Vec<(usize, usize)> = (0..n).flat_map(|j| (0..j).map(move |i| (i, j))).collect();
        let cost_sets: &[u32] = if n == 5 { &[1, 3] } else { &[0, 1, 2, 3] };
        let ncost = cost_sets.len().pow(n as u32);
        for edges in 0..1u32 << pairs.len() {
            for c in 0..ncost {
                let mut tasks: Vec<Task> = (0..n)
                    .map(|i| Task {
                        cost: cost_sets[(c / cost_sets.len().pow(i as u32)) % cost_sets.len()],
                        deps: 0,
                    })
                    .collect();
                for (k, (i, j)) in pairs.iter().enumerate() {
                    if edges >> k & 1 == 1 {
                        tasks[*j].deps |= 1 << i;
                    }
                }
                for m in 1..=3 {
                    check(&tasks, m);
                }
                instances += 1;
            }
        }
    }
    assert_eq!(instances, 49_700);
}

#[test]
fn the_best_priority_order_is_never_worse_than_either_policy() {
    // Brute force over all priority orders of 6 tasks, on random graphs.
    let mut rng = Rng(77);
    let mut perms = Vec::new();
    let mut a = [0usize, 1, 2, 3, 4, 5];
    permute(&mut a, 0, &mut perms);
    assert_eq!(perms.len(), 720);
    for _ in 0..150 {
        let g = random_dag(&mut rng, 6, 250, 9);
        for m in [2, 3] {
            let best = perms
                .iter()
                .map(|p| {
                    let mut prio = [0u64; 6];
                    for (pos, task) in p.iter().enumerate() {
                        prio[*task] = 100 - pos as u64;
                    }
                    let s = schedule_with(&g, m, |i| (prio[i], 0)).unwrap();
                    validate(&g, m, &s).unwrap();
                    s.makespan
                })
                .min()
                .unwrap();
            let (fifo, cp) = check(&g, m);
            assert!(best <= fifo && best <= cp);
            assert!(best >= lower_bound(&g, m).unwrap());
        }
    }
}

fn permute(a: &mut [usize; 6], k: usize, out: &mut Vec<[usize; 6]>) {
    if k == a.len() {
        out.push(*a);
        return;
    }
    for i in k..a.len() {
        a.swap(k, i);
        permute(a, k + 1, out);
        a.swap(k, i);
    }
}
