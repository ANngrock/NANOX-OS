use svc::modecfg::{self, Boot};
use svc::{
    Backoff, Class, Def, Event, Health, Limits, Mode, ModeSet, NoticeKind, Phase, Platform, Policy,
    Restart, ServiceId, StartError, Supervisor, SwitchError, TableError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Start,
    Stop,
    Kill,
    Probe,
}

#[derive(Default)]
struct Rec {
    calls: Vec<(Op, ServiceId)>,
    fail_start: u16,
}

impl Platform for Rec {
    fn start(&mut self, id: ServiceId, _d: &Def<'_>) -> Result<(), StartError> {
        self.calls.push((Op::Start, id));
        if self.fail_start & (1 << id) != 0 {
            Err(StartError::Denied)
        } else {
            Ok(())
        }
    }
    fn stop(&mut self, id: ServiceId) {
        self.calls.push((Op::Stop, id));
    }
    fn kill(&mut self, id: ServiceId) {
        self.calls.push((Op::Kill, id));
    }
    fn probe(&mut self, id: ServiceId) {
        self.calls.push((Op::Probe, id));
    }
}

type Sup = Supervisor<'static, Rec>;

const LIM: Limits = Limits {
    cpu_reserve: 50,
    cpu_weight: 100,
    cpu_cap: 500,
    mem_reserve: 16 << 20,
    mem_limit: 64 << 20,
};
const BACKOFF: Backoff = Backoff {
    initial_ms: 100,
    max_ms: 800,
    stable_ms: 5_000,
    window_ms: 60_000,
    limit: 5,
};
const POLICY: Policy = Policy {
    service_cpu: [200, 400, 800],
    service_mem: [128 << 20, 512 << 20, 1 << 30],
};

fn def(name: &'static str) -> Def<'static> {
    Def {
        name,
        class: Class::Service,
        modes: ModeSet::ALL,
        deps: &[],
        restart: Restart::Always,
        backoff: BACKOFF,
        health: None,
        ready_timeout_ms: 0,
        stop_timeout_ms: 1_000,
        limits: LIM,
        safe: false,
    }
}

fn table(v: Vec<Def<'static>>) -> &'static [Def<'static>] {
    Box::leak(v.into_boxed_slice())
}

fn boot(mode: Mode) -> Boot {
    Boot {
        mode,
        safe: false,
        reason: None,
    }
}

fn sup(defs: &'static [Def<'static>], mode: Mode) -> Sup {
    Supervisor::new(defs, POLICY, Rec::default(), boot(mode)).unwrap()
}

fn calls(s: &mut Sup) -> Vec<(Op, ServiceId)> {
    std::mem::take(&mut s.platform().calls)
}

/// Runs the supervisor to rest at `now`, answering stops and readiness.
fn settle(s: &mut Sup, now: u64) -> Vec<(Op, ServiceId)> {
    let mut log = Vec::new();
    s.tick(now);
    for _ in 0..50 {
        let c = calls(s);
        if c.is_empty() {
            break;
        }
        for &(op, id) in &c {
            match op {
                Op::Stop => s.event(now, Event::Exited(id, 0)),
                Op::Start if s.phase(id) == Phase::Starting => s.event(now, Event::Ready(id)),
                _ => {}
            }
        }
        log.extend(c);
    }
    log
}

fn pos(log: &[(Op, ServiceId)], op: Op, id: ServiceId) -> usize {
    log.iter()
        .position(|&c| c == (op, id))
        .unwrap_or_else(|| panic!("{op:?} {id} not in {log:?}"))
}

#[test]
fn dependencies_start_in_order_and_wait_for_readiness() {
    let defs = table(vec![
        Def {
            ready_timeout_ms: 5_000,
            ..def("db")
        },
        Def {
            deps: &[0],
            ready_timeout_ms: 5_000,
            ..def("app")
        },
        Def {
            deps: &[1],
            ..def("web")
        },
    ]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    assert_eq!(calls(&mut s), [(Op::Start, 0)], "only db can start");
    assert_eq!(s.phase(0), Phase::Starting);
    s.event(100, Event::Ready(0));
    assert_eq!(calls(&mut s), [(Op::Start, 1)]);
    assert_eq!(s.phase(2), Phase::Stopped);
    s.event(200, Event::Ready(1));
    assert_eq!(calls(&mut s), [(Op::Start, 2)]);
    assert_eq!(
        s.phase(2),
        Phase::Running,
        "no readiness protocol: running at spawn"
    );
    assert!(s.settled());
    s.check().unwrap();
}

#[test]
fn a_service_that_never_reports_ready_is_killed_and_retried() {
    let defs = table(vec![Def {
        ready_timeout_ms: 1_000,
        ..def("slow")
    }]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    assert_eq!(calls(&mut s), [(Op::Start, 0)]);
    assert_eq!(s.next_deadline(), Some(1_000));
    s.tick(999);
    assert!(calls(&mut s).is_empty());
    s.tick(1_000);
    assert_eq!(calls(&mut s), [(Op::Kill, 0)]);
    assert_eq!(s.phase(0), Phase::Backoff);
    s.tick(1_100);
    assert_eq!(calls(&mut s), [(Op::Start, 0)]);
}

#[test]
fn restart_backs_off_exponentially_then_resets_after_stable_uptime() {
    // Six exits in a minute: the breaker limit is raised so it stays out of
    // the way (it has its own test).
    let defs = table(vec![Def {
        backoff: Backoff {
            limit: 7,
            ..BACKOFF
        },
        ..def("svc")
    }]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    calls(&mut s);
    let mut now = 0;
    for want in [100, 200, 400, 800, 800] {
        now += 10;
        s.event(now, Event::Exited(0, 1));
        assert_eq!(s.phase(0), Phase::Backoff);
        assert_eq!(s.next_deadline(), Some(now + want), "delay {want}");
        s.tick(now + want - 1);
        assert!(calls(&mut s).is_empty(), "not before the delay");
        now += want;
        s.tick(now);
        assert_eq!(calls(&mut s), [(Op::Start, 0)]);
    }
    // Long enough up: the next failure starts from the initial delay again.
    now += 6_000;
    s.event(now, Event::Exited(0, 1));
    assert_eq!(s.next_deadline(), Some(now + 100));
}

#[test]
fn a_crash_loop_ends_failed_and_needs_a_manual_restart() {
    let defs = table(vec![def("flaky")]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    let mut now = 0;
    for i in 0..5 {
        now += 10;
        s.event(now, Event::Exited(0, 1));
        assert_eq!(s.phase(0), Phase::Backoff, "exit {i}");
        now += 900;
        s.tick(now);
    }
    now += 10;
    s.event(now, Event::Exited(0, 1));
    assert_eq!(s.phase(0), Phase::Failed);
    assert_eq!(
        s.next_deadline(),
        None,
        "a failed service costs no wake-ups"
    );
    s.tick(now + 1_000_000);
    assert_eq!(s.phase(0), Phase::Failed);
    let mut saw = false;
    while let Some(n) = s.notice() {
        saw |= n.kind == NoticeKind::CrashLoop;
    }
    assert!(saw);
    calls(&mut s);
    s.restart(now + 2_000_000, 0);
    assert_eq!(calls(&mut s), [(Op::Start, 0)]);
    assert_eq!(s.phase(0), Phase::Running);
}

#[test]
fn restart_policies() {
    let defs = table(vec![
        Def {
            restart: Restart::Never,
            ..def("never")
        },
        Def {
            restart: Restart::OnFailure,
            ..def("onfail")
        },
        Def {
            restart: Restart::Always,
            ..def("always")
        },
    ]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    calls(&mut s);
    // Never: gone for good, even after a failure.
    s.event(10, Event::Exited(0, 3));
    assert_eq!(s.phase(0), Phase::Stopped);
    s.tick(100_000);
    assert!(calls(&mut s).iter().all(|c| c.1 != 0));
    // OnFailure: success ends it, failure restarts it.
    s.event(20, Event::Exited(1, 0));
    assert_eq!(s.phase(1), Phase::Stopped);
    s.tick(100_000);
    assert!(calls(&mut s).iter().all(|c| c.1 != 1));
    // Always: even a clean exit restarts.
    s.event(30, Event::Exited(2, 0));
    assert_eq!(s.phase(2), Phase::Backoff);
    // A manual start brings the finished ones back.
    s.start(200_000, 0);
    assert_eq!(s.phase(0), Phase::Running);
}

#[test]
fn a_failed_spawn_is_a_crash() {
    let defs = table(vec![def("nope")]);
    let mut s = sup(defs, Mode::Server);
    s.platform().fail_start = 1;
    s.tick(0);
    assert_eq!(s.phase(0), Phase::Backoff);
    assert_eq!(s.next_deadline(), Some(100));
    s.platform().fail_start = 0;
    s.tick(100);
    assert_eq!(s.phase(0), Phase::Running);
}

#[test]
fn health_checks_kill_after_consecutive_failures() {
    let defs = table(vec![Def {
        health: Some(Health {
            interval_ms: 1_000,
            timeout_ms: 200,
            failures: 2,
        }),
        ..def("h")
    }]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    calls(&mut s);
    assert_eq!(s.next_deadline(), Some(1_000));
    s.tick(1_000);
    assert_eq!(calls(&mut s), [(Op::Probe, 0)]);
    s.event(1_050, Event::Health(0, true));
    assert_eq!(
        s.next_deadline(),
        Some(2_050),
        "the interval counts from the answer"
    );
    s.tick(2_050);
    calls(&mut s);
    s.event(2_100, Event::Health(0, false));
    s.tick(3_100);
    assert_eq!(calls(&mut s), [(Op::Probe, 0)]);
    // A healthy answer resets the streak.
    s.event(3_120, Event::Health(0, true));
    s.tick(4_120);
    calls(&mut s);
    s.event(4_130, Event::Health(0, false));
    s.tick(5_130);
    calls(&mut s);
    assert_eq!(
        s.phase(0),
        Phase::Running,
        "one failure since the last success"
    );
    // No answer at all: timeout counts as a failure, the second kills.
    s.tick(5_330);
    assert_eq!(calls(&mut s), [(Op::Kill, 0)]);
    assert_eq!(s.phase(0), Phase::Backoff);
    let mut kinds = vec![];
    while let Some(n) = s.notice() {
        kinds.push(n.kind);
    }
    assert!(kinds.contains(&NoticeKind::HealthFailed));
}

#[test]
fn a_stuck_stop_escalates_to_kill() {
    let defs = table(vec![def("stubborn")]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    calls(&mut s);
    s.stop(10, 0);
    assert_eq!(calls(&mut s), [(Op::Stop, 0)]);
    assert_eq!(s.phase(0), Phase::Stopping);
    assert_eq!(s.next_deadline(), Some(1_010));
    s.tick(1_009);
    assert!(calls(&mut s).is_empty());
    s.tick(1_010);
    assert_eq!(calls(&mut s), [(Op::Kill, 0)]);
    assert_eq!(
        s.phase(0),
        Phase::Stopped,
        "stopped, not restarted: it was asked to stop"
    );
    s.tick(100_000);
    assert!(calls(&mut s).is_empty());
}

#[test]
fn a_dependency_failure_cascades_and_recovery_restarts_dependents() {
    let defs = table(vec![
        def("db"),
        Def {
            deps: &[0],
            ..def("app")
        },
        Def {
            deps: &[1],
            ..def("web")
        },
        def("independent"),
    ]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    calls(&mut s);
    s.event(50, Event::Exited(0, 1));
    let c = calls(&mut s);
    // web (deepest dependent) is asked to stop first, then app; db is gone.
    assert_eq!(c, [(Op::Stop, 2)], "{c:?}");
    s.event(50, Event::Exited(2, 0));
    assert_eq!(calls(&mut s), [(Op::Stop, 1)]);
    s.event(50, Event::Exited(1, 0));
    assert_eq!(s.phase(3), Phase::Running, "unrelated service untouched");
    s.tick(150);
    // db restarts; its dependents follow in order.
    assert_eq!(
        calls(&mut s),
        [(Op::Start, 0), (Op::Start, 1), (Op::Start, 2)]
    );
    assert!(s.settled());
    s.check().unwrap();
}

#[test]
fn manual_stop_takes_dependents_along_and_start_brings_them_back() {
    let defs = table(vec![
        def("db"),
        Def {
            deps: &[0],
            ..def("app")
        },
    ]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    calls(&mut s);
    s.stop(10, 0);
    assert_eq!(calls(&mut s), [(Op::Stop, 1)], "the dependent goes first");
    s.event(11, Event::Exited(1, 0));
    assert_eq!(calls(&mut s), [(Op::Stop, 0)]);
    s.event(12, Event::Exited(0, 0));
    s.tick(1_000_000);
    assert!(calls(&mut s).is_empty(), "stays stopped");
    s.start(2_000_000, 0);
    assert_eq!(calls(&mut s), [(Op::Start, 0), (Op::Start, 1)]);
}

#[test]
fn restart_bounces_the_service_and_its_dependents() {
    let defs = table(vec![
        def("db"),
        Def {
            deps: &[0],
            ..def("app")
        },
    ]);
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    calls(&mut s);
    s.restart(10, 0);
    assert_eq!(calls(&mut s), [(Op::Stop, 1)]);
    s.event(11, Event::Exited(1, 0));
    assert_eq!(calls(&mut s), [(Op::Stop, 0)]);
    s.event(12, Event::Exited(0, 0));
    assert_eq!(calls(&mut s), [(Op::Start, 0), (Op::Start, 1)]);
    assert!(s.settled());
}

fn mode_table() -> &'static [Def<'static>] {
    table(vec![
        Def {
            class: Class::Interactive,
            modes: ModeSet::DESKTOP.union(ModeSet::HYBRID),
            ..def("shell")
        },
        def("db"),
        Def {
            deps: &[1],
            modes: ModeSet::HYBRID.union(ModeSet::SERVER),
            ..def("app")
        },
        Def {
            modes: ModeSet::SERVER,
            ..def("batch")
        },
    ])
}

#[test]
fn the_interactive_part_starts_last_and_stops_first() {
    let mut s = sup(mode_table(), Mode::Hybrid);
    let log = settle(&mut s, 0);
    let starts: Vec<_> = log
        .iter()
        .filter(|c| c.0 == Op::Start)
        .map(|c| c.1)
        .collect();
    assert_eq!(starts, [1, 2, 0], "db, app, then the shell");
    assert!(s.settled());

    // hybrid -> server: the shell goes first, batch comes up.
    s.set_mode(1_000, Mode::Server).unwrap();
    let log = settle(&mut s, 1_000);
    assert_eq!(log[0], (Op::Stop, 0), "{log:?}");
    assert!(log.contains(&(Op::Start, 3)));
    assert_eq!(s.phase(0), Phase::Stopped);
    assert_eq!(s.phase(3), Phase::Running);
    assert!(s.settled());

    // server -> desktop: app and batch go, dependents first; the shell
    // returns after the services that stay.
    s.set_mode(2_000, Mode::Desktop).unwrap();
    let log = settle(&mut s, 2_000);
    assert_eq!(s.phase(1), Phase::Running);
    assert_eq!(s.phase(2), Phase::Stopped);
    assert_eq!(s.phase(3), Phase::Stopped);
    assert_eq!(s.phase(0), Phase::Running);
    assert!(pos(&log, Op::Stop, 2) < pos(&log, Op::Start, 0), "{log:?}");

    // desktop -> hybrid: app comes back, the shell stays up.
    s.set_mode(3_000, Mode::Hybrid).unwrap();
    settle(&mut s, 3_000);
    assert!(s.settled());
    assert_eq!(s.phase(0), Phase::Running);
    assert_eq!(s.phase(2), Phase::Running);
}

#[test]
fn the_shell_waits_for_services_that_are_about_to_start_but_not_for_blocked_ones() {
    let defs = table(vec![
        Def {
            class: Class::Interactive,
            modes: ModeSet::HYBRID,
            ..def("shell")
        },
        Def {
            ready_timeout_ms: 500,
            ..def("db")
        },
        Def {
            deps: &[1],
            ..def("app")
        },
    ]);
    let mut s = sup(defs, Mode::Hybrid);
    s.tick(0);
    assert_eq!(
        calls(&mut s),
        [(Op::Start, 1)],
        "db only; app waits for db, so does the shell"
    );
    assert_eq!(s.phase(0), Phase::Stopped);
    s.event(100, Event::Ready(1));
    assert_eq!(
        calls(&mut s),
        [(Op::Start, 2), (Op::Start, 0)],
        "app, then the shell"
    );

    // A service blocked by a *failed* dependency must not hold the shell back.
    let mut s = sup(defs, Mode::Hybrid);
    s.platform().fail_start = 1 << 1; // db never starts
    s.tick(0);
    let mut now = 0;
    for _ in 0..6 {
        now += 2_000;
        s.tick(now);
    }
    assert_eq!(s.phase(1), Phase::Failed);
    assert_eq!(s.phase(2), Phase::Stopped);
    assert_eq!(
        s.phase(0),
        Phase::Running,
        "the desktop is up although a service is down"
    );
}

#[test]
fn a_mode_switch_stops_services_that_are_not_wanted_and_keeps_always_services() {
    let mut s = sup(mode_table(), Mode::Server);
    settle(&mut s, 0);
    assert_eq!(s.phase(1), Phase::Running);
    s.set_mode(10, Mode::Desktop).unwrap();
    settle(&mut s, 10);
    assert_eq!(s.phase(1), Phase::Running, "db runs in every mode");
    assert_eq!(s.phase(1), Phase::Running);
    assert!(s.settled());
    // Going back to the same mode is a no-op.
    s.set_mode(20, Mode::Desktop).unwrap();
    assert!(calls(&mut s).is_empty());
}

#[test]
fn shell_stop_precedes_every_service_stop() {
    // Hybrid -> Server with a hybrid-only service: the shell must stop
    // before that service is even asked to.
    let defs = table(vec![
        Def {
            class: Class::Interactive,
            modes: ModeSet::HYBRID,
            ..def("shell")
        },
        Def {
            modes: ModeSet::HYBRID,
            ..def("sync")
        },
        Def {
            modes: ModeSet::HYBRID.union(ModeSet::SERVER),
            ..def("core")
        },
    ]);
    let mut s = sup(defs, Mode::Hybrid);
    settle(&mut s, 0);
    s.set_mode(100, Mode::Server).unwrap();
    let first = calls(&mut s);
    assert_eq!(first, [(Op::Stop, 0)], "only the shell at first");
    assert_eq!(s.phase(1), Phase::Running);
    s.event(110, Event::Exited(0, 0));
    assert_eq!(calls(&mut s), [(Op::Stop, 1)]);
}

#[test]
fn a_switch_over_budget_changes_nothing() {
    // Hybrid caps service reservations at 400 permille: five at 100 do not fit.
    let big = Limits {
        cpu_reserve: 100,
        cpu_cap: 500,
        ..LIM
    };
    let defs = table(vec![
        Def {
            modes: ModeSet::SERVER,
            limits: big,
            ..def("a")
        },
        Def {
            modes: ModeSet::ALL,
            limits: big,
            ..def("b")
        },
        Def {
            modes: ModeSet::ALL,
            limits: big,
            ..def("c")
        },
        Def {
            modes: ModeSet::HYBRID.union(ModeSet::SERVER),
            limits: big,
            ..def("d")
        },
        Def {
            modes: ModeSet::HYBRID.union(ModeSet::SERVER),
            limits: big,
            ..def("e")
        },
        Def {
            modes: ModeSet::HYBRID,
            limits: big,
            ..def("f")
        },
    ]);
    let mut s = sup(defs, Mode::Server);
    settle(&mut s, 0);
    let before: Vec<_> = (0..6).map(|i| s.phase(i)).collect();
    assert_eq!(
        s.set_mode(10, Mode::Hybrid),
        Err(SwitchError::ServiceCpu {
            need: 500,
            cap: 400
        })
    );
    assert_eq!(s.mode(), Mode::Server);
    assert!(
        calls(&mut s).is_empty(),
        "no platform call on a refused switch"
    );
    assert_eq!((0..6).map(|i| s.phase(i)).collect::<Vec<_>>(), before);
    // Memory budget.
    let mem = table(vec![
        Def {
            modes: ModeSet::SERVER,
            limits: Limits {
                mem_reserve: 200 << 20,
                mem_limit: 300 << 20,
                ..LIM
            },
            ..def("m")
        },
        Def {
            modes: ModeSet::ALL,
            limits: Limits {
                mem_reserve: 100 << 20,
                mem_limit: 300 << 20,
                ..LIM
            },
            ..def("n")
        },
    ]);
    let mut s = sup(mem, Mode::Server);
    settle(&mut s, 0);
    assert_eq!(
        s.set_mode(10, Mode::Desktop),
        Ok(()),
        "100 MiB fits the desktop budget of 128 MiB"
    );
    let mut s = sup(mem, Mode::Desktop);
    settle(&mut s, 0);
    assert!(s.set_mode(10, Mode::Server).is_ok());
    // A boot mode over budget is a table error.
    let bad = table(vec![Def {
        limits: Limits {
            mem_reserve: 1 << 40,
            mem_limit: 1 << 41,
            ..LIM
        },
        ..def("huge")
    }]);
    assert!(matches!(
        Supervisor::new(bad, POLICY, Rec::default(), boot(Mode::Server)),
        Err(TableError::Budget(SwitchError::ServiceMem { .. }))
    ));
}

#[test]
fn starts_never_overcommit_while_the_old_mode_is_still_stopping() {
    // Server -> desktop: the desktop budget for services is 200 permille;
    // "old" (server only) holds 150 while stopping, "new" (desktop only)
    // needs 150: it may only start after "old" is gone.
    let l = Limits {
        cpu_reserve: 150,
        cpu_cap: 500,
        ..LIM
    };
    let defs = table(vec![
        Def {
            modes: ModeSet::SERVER,
            limits: l,
            ..def("old")
        },
        Def {
            modes: ModeSet::DESKTOP,
            limits: l,
            ..def("new")
        },
    ]);
    let mut s = sup(defs, Mode::Server);
    settle(&mut s, 0);
    s.set_mode(10, Mode::Desktop).unwrap();
    assert_eq!(calls(&mut s), [(Op::Stop, 0)]);
    assert_eq!(
        s.phase(1),
        Phase::Stopped,
        "waits for the reservation to be free"
    );
    s.check().unwrap();
    s.event(20, Event::Exited(0, 0));
    assert_eq!(calls(&mut s), [(Op::Start, 1)]);
    assert!(s.settled());
}

#[test]
fn a_damaged_mode_record_boots_the_safe_server() {
    let defs = table(vec![
        Def {
            class: Class::Interactive,
            modes: ModeSet::DESKTOP.union(ModeSet::HYBRID),
            ..def("shell")
        },
        Def {
            safe: true,
            ..def("logger")
        },
        Def {
            deps: &[1],
            ..def("network-service")
        },
    ]);
    let good = modecfg::encode(Mode::Hybrid);
    let b = modecfg::boot(Some(&good));
    assert_eq!((b.mode, b.safe), (Mode::Hybrid, false));
    let mut bad = good;
    bad[5] ^= 1;
    for stored in [Some(&bad[..]), Some(&good[..5]), None] {
        let b = modecfg::boot(stored);
        assert_eq!((b.mode, b.safe), (Mode::Server, true));
        let mut s = Supervisor::new(defs, POLICY, Rec::default(), b).unwrap();
        settle(&mut s, 0);
        assert_eq!(s.phase(1), Phase::Running, "the safe service runs");
        assert_eq!(s.phase(0), Phase::Stopped);
        assert_eq!(s.phase(2), Phase::Stopped, "no other service, no shell");
        assert!(s.safe_mode());
    }
}

#[test]
fn tables_are_validated() {
    let mk = |v: Vec<Def<'static>>| {
        Supervisor::new(table(v), POLICY, Rec::default(), boot(Mode::Server)).err()
    };
    assert_eq!(
        mk(vec![Def {
            deps: &[0],
            ..def("a")
        }]),
        Some(TableError::DepOrder)
    );
    assert_eq!(
        mk(vec![def("a"), def("a")]),
        Some(TableError::DuplicateName)
    );
    assert_eq!(mk(vec![def("")]), Some(TableError::Name));
    assert_eq!(
        mk(vec![Def {
            class: Class::Interactive,
            ..def("shell")
        }]),
        Some(TableError::InteractiveInServer)
    );
    assert_eq!(
        mk(vec![Def {
            limits: Limits { cpu_cap: 10, ..LIM },
            ..def("a")
        }]),
        Some(TableError::Limits)
    );
    assert_eq!(
        mk(vec![Def {
            backoff: Backoff {
                limit: 8,
                ..BACKOFF
            },
            ..def("a")
        }]),
        Some(TableError::Backoff)
    );
    assert_eq!(
        mk(vec![Def {
            health: Some(Health {
                interval_ms: 0,
                timeout_ms: 1,
                failures: 1,
            }),
            ..def("a")
        }]),
        Some(TableError::Health)
    );
    let many: Vec<Def<'static>> = (0..17)
        .map(|i| def(Box::leak(format!("s{i}").into_boxed_str())))
        .collect();
    assert_eq!(mk(many), Some(TableError::TooMany));
}

#[test]
fn idle_costs_no_wake_ups_and_little_memory() {
    // Twelve steady services without health checks: nothing is scheduled.
    let defs = table(
        (0..12)
            .map(|i| def(Box::leak(format!("s{i}").into_boxed_str())))
            .collect(),
    );
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    assert!(s.settled());
    assert_eq!(s.next_deadline(), None);
    // With 10 s health checks the only wake-ups are the probes.
    let hc = Health {
        interval_ms: 10_000,
        timeout_ms: 500,
        failures: 3,
    };
    let defs = table(
        (0..12)
            .map(|i| Def {
                health: Some(hc),
                ..def(Box::leak(format!("h{i}").into_boxed_str()))
            })
            .collect(),
    );
    let mut s = sup(defs, Mode::Server);
    s.tick(0);
    let (mut wakeups, mut now) = (0, 0);
    while let Some(t) = s.next_deadline() {
        if t > 3_600_000 {
            break;
        }
        now = t;
        s.tick(now);
        for (op, id) in calls(&mut s) {
            if op == Op::Probe {
                s.event(now + 1, Event::Health(id, true));
            }
        }
        wakeups += 1;
    }
    // One wake-up per probe (the answer re-arms it), 360 per service-hour.
    assert!(now > 3_000_000);
    // Each probe is answered 1 ms later and re-arms 10 s after the answer:
    // about 360 wake-ups an hour. The twelve services share them: all are
    // due at the same instants, so one wake-up serves every one.
    assert!(
        (350..=370).contains(&wakeups),
        "{wakeups} wake-ups in an hour"
    );
    // The whole supervisor is a few kilobytes.
    let size = std::mem::size_of::<Sup>();
    assert!(size < 8 * 1024, "{size} bytes");
}

#[test]
fn cpu_plan_protects_the_interactive_reservation() {
    let defs = table(vec![
        Def {
            class: Class::Interactive,
            modes: ModeSet::HYBRID,
            limits: Limits {
                cpu_reserve: 300,
                cpu_weight: 100,
                cpu_cap: 1000,
                ..LIM
            },
            ..def("shell")
        },
        Def {
            limits: Limits {
                cpu_reserve: 100,
                cpu_weight: 100,
                cpu_cap: 1000,
                ..LIM
            },
            ..def("worker-a")
        },
        Def {
            limits: Limits {
                cpu_reserve: 50,
                cpu_weight: 300,
                cpu_cap: 1000,
                ..LIM
            },
            ..def("worker-b")
        },
    ]);
    let mut s = sup(defs, Mode::Hybrid);
    settle(&mut s, 0);
    let mut out = [0u32; 3];
    // Workers want everything, the shell wants only what it is guaranteed.
    s.cpu_plan(&[300, 1000, 1000], &mut out).unwrap();
    assert_eq!(out[0], 300, "the shell gets its whole demand");
    assert_eq!(out.iter().sum::<u32>(), 1000);
    assert!(out[2] > out[1], "weights split the rest");
    // The shell bursts: it keeps at least its reservation whatever the workers do.
    s.cpu_plan(&[1000, 1000, 1000], &mut out).unwrap();
    assert!(out[0] >= 300);
    assert_eq!(out.iter().sum::<u32>(), 1000);
    // A stopped service gets nothing.
    s.stop(10, 2);
    settle(&mut s, 10);
    s.cpu_plan(&[1000, 1000, 1000], &mut out).unwrap();
    assert_eq!(out[2], 0);
}

// ---- random operation sequences --------------------------------------------

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

fn chaos_table() -> &'static [Def<'static>] {
    let hc = Some(Health {
        interval_ms: 700,
        timeout_ms: 100,
        failures: 2,
    });
    table(vec![
        Def {
            class: Class::Interactive,
            modes: ModeSet::DESKTOP.union(ModeSet::HYBRID),
            ready_timeout_ms: 300,
            ..def("shell")
        },
        Def {
            ready_timeout_ms: 200,
            ..def("db")
        },
        Def {
            deps: &[1],
            modes: ModeSet::HYBRID.union(ModeSet::SERVER),
            health: hc,
            ..def("app")
        },
        Def {
            deps: &[2],
            modes: ModeSet::SERVER,
            restart: Restart::OnFailure,
            ..def("web")
        },
        Def {
            modes: ModeSet::DESKTOP.union(ModeSet::SERVER),
            health: hc,
            ..def("cache")
        },
        Def {
            deps: &[1, 4],
            modes: ModeSet::SERVER,
            ready_timeout_ms: 150,
            ..def("worker")
        },
        Def {
            restart: Restart::Never,
            modes: ModeSet::HYBRID,
            ..def("oneshot")
        },
        Def {
            modes: ModeSet::ALL,
            ..def("logger")
        },
    ])
}

#[test]
fn random_operations_keep_every_invariant_and_converge() {
    for seed in 1..=40u64 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let defs = chaos_table();
        let mut s = sup(defs, Mode::Server);
        let mut now = 0u64;
        // Events the environment delivers later: (time, event).
        let mut pending: Vec<(u64, Event)> = Vec::new();
        let n = defs.len() as u64;
        let mut chaos = true;
        for step in 0..1_500 {
            if step == 1_000 {
                chaos = false; // let it settle: no more crashes or bad health
                for id in 0..defs.len() {
                    s.start(now, id as u8); // clear manual stops, failures, finished
                }
            }
            // React to what the supervisor asked for.
            for (op, id) in calls(&mut s) {
                let delay = 1 + rng.below(60);
                match op {
                    Op::Stop => pending.push((now + delay, Event::Exited(id, 0))),
                    Op::Start => {
                        if defs[usize::from(id)].ready_timeout_ms > 0
                            && (!chaos || rng.below(6) != 0)
                        {
                            pending.push((now + delay, Event::Ready(id)));
                        }
                    }
                    Op::Probe => {
                        let ok = !chaos || rng.below(4) != 0;
                        pending.push((now + delay, Event::Health(id, ok)));
                    }
                    Op::Kill => {}
                }
            }
            // A random action.
            if chaos {
                match rng.below(9) {
                    0 => {
                        let m = [Mode::Desktop, Mode::Hybrid, Mode::Server][rng.below(3) as usize];
                        let _ = s.set_mode(now, m);
                    }
                    1 => s.start(now, rng.below(n) as u8),
                    2 => s.stop(now, rng.below(n) as u8),
                    3 => s.restart(now, rng.below(n) as u8),
                    4 | 5 => {
                        let id = rng.below(n) as u8;
                        if matches!(s.phase(id), Phase::Running | Phase::Starting) {
                            s.event(now, Event::Exited(id, rng.below(3) as i32));
                        }
                    }
                    _ => {}
                }
            }
            // Advance to the next thing that happens.
            pending.sort_by_key(|p| p.0);
            let next_env = pending.first().map(|p| p.0);
            let next_sup = s.next_deadline();
            let target = [next_env, next_sup, Some(now + 1 + rng.below(80))]
                .into_iter()
                .flatten()
                .min()
                .unwrap();
            now = target.max(now);
            while pending.first().is_some_and(|p| p.0 <= now) {
                let (_, e) = pending.remove(0);
                s.event(now, e);
                s.check()
                    .unwrap_or_else(|m| panic!("seed {seed} step {step}: {m}"));
            }
            s.tick(now);
            s.check()
                .unwrap_or_else(|m| panic!("seed {seed} step {step}: {m}"));
        }
        // Quiesce: no chaos; run long enough for every backoff to elapse.
        for _ in 0..400 {
            for (op, id) in calls(&mut s) {
                match op {
                    Op::Stop => pending.push((now + 5, Event::Exited(id, 0))),
                    Op::Start if defs[usize::from(id)].ready_timeout_ms > 0 => {
                        pending.push((now + 5, Event::Ready(id)))
                    }
                    Op::Probe => pending.push((now + 5, Event::Health(id, true))),
                    _ => {}
                }
            }
            pending.sort_by_key(|p| p.0);
            let t = [pending.first().map(|p| p.0), s.next_deadline()]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(now + 100_000);
            now = t.max(now);
            while pending.first().is_some_and(|p| p.0 <= now) {
                let (_, e) = pending.remove(0);
                s.event(now, e);
            }
            s.tick(now);
            s.check().unwrap();
        }
        // After the chaos every wanted service runs and nothing else does.
        let phases: Vec<_> = defs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.name, s.phase(i as u8)))
            .collect();
        assert!(
            s.settled(),
            "seed {seed}: not settled in {:?}: {phases:?}",
            s.mode()
        );
        for (i, d) in defs.iter().enumerate() {
            let p = s.phase(i as u8);
            if d.modes.contains(s.mode()) {
                assert_eq!(p, Phase::Running, "seed {seed}: {}", d.name);
            } else {
                assert_eq!(p, Phase::Stopped, "seed {seed}: {}", d.name);
            }
        }
    }
}
