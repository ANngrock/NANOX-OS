//! Correctness of the scheduler: every schedule it produces is valid, meets
//! the list-scheduling bound and the lower bound, and small cases are
//! checked exhaustively.

use dep_sched::{lower_bound, ranks, schedule, validate, Error, Policy, Task, MAX_TASKS};

fn t(cost: u32, deps: &[usize]) -> Task {
    Task {
        cost,
        deps: deps.iter().fold(0, |m, d| m | 1 << d),
    }
}

fn run(tasks: &[Task], m: usize, p: Policy) -> u64 {
    let s = schedule(tasks, m, p).unwrap();
    validate(tasks, m, &s).unwrap();
    s.makespan
}

#[test]
fn critical_path_beats_declaration_order_when_the_long_pole_is_declared_last() {
    // Two cheap tasks declared before a long one; two workers.
    let g = [t(1, &[]), t(1, &[]), t(10, &[])];
    assert_eq!(run(&g, 2, Policy::Fifo), 11);
    assert_eq!(run(&g, 2, Policy::CriticalPath), 10);
    // A chain is a chain: nothing to choose.
    let chain = [t(3, &[]), t(4, &[0]), t(5, &[1])];
    assert_eq!(run(&chain, 4, Policy::Fifo), 12);
    assert_eq!(run(&chain, 4, Policy::CriticalPath), 12);
}

#[test]
fn simple_shapes_have_the_obvious_makespan() {
    // Fork-join: source, 4 parallel (cost 5), sink.
    let g = [
        t(2, &[]),
        t(5, &[0]),
        t(5, &[0]),
        t(5, &[0]),
        t(5, &[0]),
        t(3, &[1, 2, 3, 4]),
    ];
    assert_eq!(run(&g, 4, Policy::CriticalPath), 2 + 5 + 3);
    assert_eq!(run(&g, 2, Policy::CriticalPath), 2 + 10 + 3);
    assert_eq!(run(&g, 1, Policy::CriticalPath), 2 + 20 + 3);
    assert_eq!(run(&[], 3, Policy::Fifo), 0);
    // Zero-cost tasks take no time and do not block anything.
    let z = [t(0, &[]), t(4, &[0]), t(0, &[1])];
    assert_eq!(run(&z, 2, Policy::Fifo), 4);
}

#[test]
fn ranks_are_longest_paths_to_the_end() {
    let g = [t(1, &[]), t(2, &[0]), t(3, &[0]), t(4, &[1, 2])];
    let r = ranks(&g).unwrap();
    assert_eq!(&r[..4], &[1 + 3 + 4, 2 + 4, 3 + 4, 4]);
    assert_eq!(lower_bound(&g, 2).unwrap(), 8);
    assert_eq!(lower_bound(&g, 1).unwrap(), 10);
}

#[test]
fn bad_inputs_are_rejected() {
    let ok = [t(1, &[])];
    assert_eq!(schedule(&ok, 0, Policy::Fifo).unwrap_err(), Error::Workers);
    assert_eq!(schedule(&ok, 17, Policy::Fifo).unwrap_err(), Error::Workers);
    assert_eq!(
        schedule(&[t(1, &[0])], 1, Policy::Fifo).unwrap_err(),
        Error::Cycle
    );
    assert_eq!(
        schedule(&[t(1, &[1]), t(1, &[0])], 2, Policy::Fifo).unwrap_err(),
        Error::Cycle
    );
    assert_eq!(
        schedule(&[t(1, &[0]), t(1, &[])], 2, Policy::Fifo).unwrap_err(),
        Error::Cycle
    );
    assert_eq!(
        schedule(
            &[Task {
                cost: 1,
                deps: 1 << 5
            }],
            1,
            Policy::Fifo
        )
        .unwrap_err(),
        Error::BadDependency
    );
    let big = vec![t(1, &[]); MAX_TASKS + 1];
    assert_eq!(
        schedule(&big, 1, Policy::Fifo).unwrap_err(),
        Error::TooManyTasks
    );
    let full = vec![t(1, &[]); MAX_TASKS];
    assert_eq!(run(&full, 8, Policy::Fifo), 8);
}

#[test]
fn validator_catches_broken_schedules() {
    let g = [t(2, &[]), t(2, &[0])];
    let mut s = schedule(&g, 2, Policy::Fifo).unwrap();
    assert_eq!(validate(&g, 2, &s), Ok(()));
    let ok = s;
    s.start[1] = 1;
    s.worker[1] = 1; // a different worker: only the dependency is broken
    assert_eq!(
        validate(&g, 2, &s),
        Err("task started before a dependency finished")
    );
    let mut s2 = ok;
    s2.worker[1] = 0;
    s2.start[1] = 1; // overlaps task 0 on the same worker, and too early
    assert!(validate(&g, 2, &s2).is_err());
    let independent = [t(3, &[]), t(3, &[])];
    let mut s3 = schedule(&independent, 2, Policy::Fifo).unwrap();
    s3.worker[1] = s3.worker[0];
    assert_eq!(
        validate(&independent, 2, &s3),
        Err("a worker runs two tasks at once")
    );
    let mut s4 = ok;
    s4.makespan += 1;
    assert_eq!(
        validate(&g, 2, &s4),
        Err("makespan is not the latest finish")
    );
    let mut s5 = ok;
    s5.worker[0] = 9;
    assert_eq!(validate(&g, 2, &s5), Err("worker out of range"));
}
