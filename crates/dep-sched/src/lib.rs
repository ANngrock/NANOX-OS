//! Scheduling of tasks with dependencies on identical workers, and the
//! numbers that say whether dependency-aware ordering beats the obvious
//! baseline (docs/research/DEP-SCHED.md).
//!
//! A task has a cost (time units) and a set of predecessors that must have
//! finished before it starts. Scheduling is non-preemptive list scheduling:
//! whenever a worker is free and a task is ready, one is started; the
//! [`Policy`] picks which. [`Policy::Fifo`] is the baseline: the ready task
//! with the lowest index, i.e. declaration order, which is what a build tool
//! or a service manager does without looking at the graph.
//! [`Policy::CriticalPath`] starts the ready task with the longest path
//! still ahead of it (the classic HLFET rule).
//!
//! `no_std`, no allocation, at most 64 tasks (dependencies are a bit mask),
//! safe Rust, deterministic.

#![no_std]
#![forbid(unsafe_code)]

pub const MAX_TASKS: usize = 64;
pub const MAX_WORKERS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Task {
    pub cost: u32,
    /// Bit i set: task i must finish before this one starts.
    pub deps: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Lowest index first.
    Fifo,
    /// Longest remaining path first; ties by lowest index.
    CriticalPath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    TooManyTasks,
    /// Zero workers or more than [`MAX_WORKERS`].
    Workers,
    /// A dependency names a task that does not exist.
    BadDependency,
    Cycle,
}

#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    pub n: usize,
    pub start: [u64; MAX_TASKS],
    pub worker: [u8; MAX_TASKS],
    pub makespan: u64,
}

fn all_mask(n: usize) -> u64 {
    if n == 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Longest path (sum of costs) from each task to the end, itself included.
/// Fails on a cycle or a bad dependency.
pub fn ranks(tasks: &[Task]) -> Result<[u64; MAX_TASKS], Error> {
    let n = tasks.len();
    if n > MAX_TASKS {
        return Err(Error::TooManyTasks);
    }
    let all = all_mask(n);
    for t in tasks {
        if t.deps & !all != 0 {
            return Err(Error::BadDependency);
        }
    }
    // Peel from the tasks nothing depends on, backwards.
    let mut rank = [0u64; MAX_TASKS];
    let mut done = 0u64;
    let mut progress = true;
    while done != all && progress {
        progress = false;
        for i in 0..n {
            if done >> i & 1 == 1 {
                continue;
            }
            // Ready when every task that depends on i has its rank.
            let mut succ_best = 0;
            let mut ready = true;
            for (j, tj) in tasks.iter().enumerate() {
                if tj.deps >> i & 1 == 1 {
                    if done >> j & 1 == 0 {
                        ready = false;
                        break;
                    }
                    succ_best = succ_best.max(rank[j]);
                }
            }
            if ready {
                rank[i] = u64::from(tasks[i].cost) + succ_best;
                done |= 1 << i;
                progress = true;
            }
        }
    }
    if done == all {
        Ok(rank)
    } else {
        Err(Error::Cycle)
    }
}

/// Runs the tasks on `workers` identical workers.
pub fn schedule(tasks: &[Task], workers: usize, policy: Policy) -> Result<Schedule, Error> {
    if workers == 0 || workers > MAX_WORKERS {
        return Err(Error::Workers);
    }
    let rank = ranks(tasks)?;
    schedule_with(tasks, workers, |i| match policy {
        Policy::Fifo => (0, u64::MAX - i as u64),
        Policy::CriticalPath => (rank[i], u64::MAX - i as u64),
    })
}

/// List scheduling with an explicit priority: among ready tasks the one
/// whose key is greatest starts first. `tasks` must be acyclic.
pub fn schedule_with(
    tasks: &[Task],
    workers: usize,
    key: impl Fn(usize) -> (u64, u64),
) -> Result<Schedule, Error> {
    let n = tasks.len();
    let mut s = Schedule {
        n,
        start: [0; MAX_TASKS],
        worker: [0; MAX_TASKS],
        makespan: 0,
    };
    let all = all_mask(n);
    let (mut started, mut finished) = (0u64, 0u64);
    let mut busy_until = [0u64; MAX_WORKERS];
    let mut running = [usize::MAX; MAX_WORKERS];
    let mut now = 0u64;
    while finished != all {
        // Retire what ended by now.
        for w in 0..workers {
            if running[w] != usize::MAX && busy_until[w] <= now {
                finished |= 1 << running[w];
                running[w] = usize::MAX;
            }
        }
        // Start ready tasks on idle workers.
        loop {
            let Some(w) = (0..workers).find(|w| running[*w] == usize::MAX) else {
                break;
            };
            let mut best: Option<usize> = None;
            for (i, task) in tasks.iter().enumerate() {
                if started >> i & 1 == 0 && task.deps & !finished == 0 {
                    best = match best {
                        Some(b) if key(b) >= key(i) => Some(b),
                        _ => Some(i),
                    };
                }
            }
            let Some(i) = best else { break };
            started |= 1 << i;
            running[w] = i;
            s.start[i] = now;
            s.worker[i] = w as u8;
            busy_until[w] = now + u64::from(tasks[i].cost);
            s.makespan = s.makespan.max(busy_until[w]);
            if tasks[i].cost == 0 {
                // A zero-cost task is done at once; look again.
                finished |= 1 << i;
                running[w] = usize::MAX;
            }
        }
        if finished == all {
            break;
        }
        // Advance to the next completion.
        match (0..workers)
            .filter(|w| running[*w] != usize::MAX)
            .map(|w| busy_until[w])
            .min()
        {
            Some(t) => now = t,
            None => return Err(Error::Cycle),
        }
    }
    Ok(s)
}

/// Lower bounds on the makespan of any schedule: the longest path, and the
/// total work spread over the workers (rounded up).
pub fn lower_bound(tasks: &[Task], workers: usize) -> Result<u64, Error> {
    let rank = ranks(tasks)?;
    let critical = rank[..tasks.len()].iter().copied().max().unwrap_or(0);
    let total: u64 = tasks.iter().map(|t| u64::from(t.cost)).sum();
    Ok(critical.max(total.div_ceil(workers as u64)))
}

/// Checks a schedule against the tasks: dependencies respected, no worker
/// runs two tasks at once, makespan is the latest finish.
pub fn validate(tasks: &[Task], workers: usize, s: &Schedule) -> Result<(), &'static str> {
    if s.n != tasks.len() {
        return Err("wrong task count");
    }
    let end = |i: usize| s.start[i] + u64::from(tasks[i].cost);
    let mut latest = 0;
    for (i, t) in tasks.iter().enumerate() {
        if usize::from(s.worker[i]) >= workers {
            return Err("worker out of range");
        }
        for j in 0..tasks.len() {
            if t.deps >> j & 1 == 1 && end(j) > s.start[i] {
                return Err("task started before a dependency finished");
            }
            if j != i
                && s.worker[i] == s.worker[j]
                && s.start[i] < end(j)
                && s.start[j] < end(i)
                && tasks[i].cost > 0
                && tasks[j].cost > 0
            {
                return Err("a worker runs two tasks at once");
            }
        }
        latest = latest.max(end(i));
    }
    if latest != s.makespan {
        return Err("makespan is not the latest finish");
    }
    Ok(())
}
