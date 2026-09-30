//! Properties of the dependency scheduler (docs/research/DEP-SCHED.md).

use dep_sched::{lower_bound, ranks, schedule, validate, Policy, Task};

use crate::{Proof, Property};

pub fn properties() -> Vec<Property> {
    vec![Property {
        id: "dep-sched.list-schedule-invariants",
        version: 1,
        statement: "For every dependency graph, both policies produce a valid schedule \
                    (dependencies respected, no worker runs two tasks at once, makespan \
                    is the latest finish) that is not below the lower bound and within \
                    Grahams list-scheduling bound; with one worker the makespan is the \
                    total cost, with at least as many workers as tasks it is the \
                    critical path; scheduling is deterministic.",
        bound:
            "every labeled DAG (any numbering) on 1..=4 tasks with costs from {0,1,2,3} and on 5 \
                tasks with costs from {1,3}; 1..=3 workers; both policies",
        component: &["crates/dep-sched/src/lib.rs"],
        checker: "crates/proofs/src/props/sched.rs",
        run: invariants,
    }]
}

fn check(tasks: &[Task], m: usize) -> Result<(), String> {
    let total: u64 = tasks.iter().map(|t| u64::from(t.cost)).sum();
    let critical = *ranks(tasks).map_err(|e| format!("{e:?}"))?[..tasks.len()]
        .iter()
        .max()
        .unwrap_or(&0);
    let lb = lower_bound(tasks, m).map_err(|e| format!("{e:?}"))?;
    for p in [Policy::Fifo, Policy::CriticalPath] {
        let s = schedule(tasks, m, p).map_err(|e| format!("{e:?}"))?;
        validate(tasks, m, &s)?;
        let again = schedule(tasks, m, p).map_err(|e| format!("{e:?}"))?;
        if again.start[..tasks.len()] != s.start[..tasks.len()] || again.makespan != s.makespan {
            return Err("not deterministic".to_string());
        }
        if s.makespan < lb {
            return Err(format!(
                "{p:?} makespan {} below the lower bound {lb}",
                s.makespan
            ));
        }
        if m as u64 * s.makespan > total + (m as u64 - 1) * critical {
            return Err(format!(
                "{p:?} makespan {} breaks the list-scheduling bound",
                s.makespan
            ));
        }
        if m == 1 && s.makespan != total {
            return Err(format!("{p:?} on one worker: {} != {total}", s.makespan));
        }
        if m >= tasks.len() && s.makespan != critical {
            return Err(format!(
                "{p:?} with enough workers: {} != {critical}",
                s.makespan
            ));
        }
    }
    Ok(())
}

fn invariants() -> Proof {
    let mut cases = 0;
    for n in 1..=5usize {
        // Every directed graph on n labeled tasks, in any numbering; the
        // cyclic ones are skipped (the scheduler rejects them, see its tests).
        let pairs: Vec<(usize, usize)> = (0..n)
            .flat_map(|j| (0..n).filter(move |i| *i != j).map(move |i| (i, j)))
            .collect();
        let costs: &[u32] = if n == 5 { &[1, 3] } else { &[0, 1, 2, 3] };
        for edges in 0..1u32 << pairs.len() {
            let mut deps = vec![0u64; n];
            for (k, (i, j)) in pairs.iter().enumerate() {
                if edges >> k & 1 == 1 {
                    deps[*j] |= 1 << i;
                }
            }
            let shape: Vec<Task> = deps.iter().map(|d| Task { cost: 0, deps: *d }).collect();
            if ranks(&shape).is_err() {
                continue;
            }
            for c in 0..costs.len().pow(n as u32) {
                let tasks: Vec<Task> = deps
                    .iter()
                    .enumerate()
                    .map(|(i, d)| Task {
                        cost: costs[c / costs.len().pow(i as u32) % costs.len()],
                        deps: *d,
                    })
                    .collect();
                for m in 1..=3 {
                    cases += 1;
                    if let Err(why) = check(&tasks, m) {
                        return Proof::failed(cases, format!("{tasks:?} on {m} workers: {why}"));
                    }
                }
            }
        }
    }
    Proof::held(cases)
}
