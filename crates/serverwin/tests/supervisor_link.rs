//! The window against the real supervisor: effects become supervisor calls,
//! and the supervisor's answers are grants, refusals and service phases.

use serverwin::{Effect, Event, GuestState, Key, Notice, Presentation, Screen, ServerWindow};
use svc::modecfg::Boot;
use svc::{
    Backoff, Class, Def, Event as SvcEvent, Health, Limits, Mode, ModeSet, Phase, Platform, Policy,
    Restart, ServiceId, StartError, Supervisor,
};

const SHELL: ServiceId = 0;
const PVE: ServiceId = 1;

#[derive(Default)]
struct Rec {
    starts: Vec<ServiceId>,
    stops: Vec<ServiceId>,
}

impl Platform for Rec {
    fn start(&mut self, id: ServiceId, _d: &Def<'_>) -> Result<(), StartError> {
        self.starts.push(id);
        Ok(())
    }
    fn stop(&mut self, id: ServiceId) {
        self.stops.push(id);
    }
    fn kill(&mut self, _id: ServiceId) {}
    fn probe(&mut self, _id: ServiceId) {}
}

const LIM: Limits = Limits {
    cpu_reserve: 300,
    cpu_weight: 100,
    cpu_cap: 600,
    mem_reserve: 256 << 20,
    mem_limit: 512 << 20,
};
const BACKOFF: Backoff = Backoff {
    initial_ms: 100,
    max_ms: 800,
    stable_ms: 5_000,
    window_ms: 60_000,
    limit: 5,
};

fn def(name: &'static str, class: Class, modes: ModeSet) -> Def<'static> {
    Def {
        name,
        class,
        modes,
        deps: &[],
        restart: Restart::Always,
        backoff: BACKOFF,
        health: None::<Health>,
        ready_timeout_ms: 0,
        stop_timeout_ms: 1_000,
        limits: Limits {
            cpu_reserve: if class == Class::Interactive {
                100
            } else {
                300
            },
            ..LIM
        },
        safe: false,
    }
}

struct Shell {
    win: ServerWindow,
    sup: Supervisor<'static, Rec>,
    now: u64,
    notices: Vec<Notice>,
    /// Refuse this many switches to hybrid (a supervisor that cannot make room).
    refuse_hybrid: u32,
}

fn shell(policy: Policy, mode: Mode) -> Shell {
    let defs: &'static [Def<'static>] = Box::leak(
        vec![
            def(
                "shell",
                Class::Interactive,
                ModeSet::DESKTOP.union(ModeSet::HYBRID),
            ),
            def(
                "proxmox-guest",
                Class::Service,
                ModeSet::HYBRID.union(ModeSet::SERVER),
            ),
        ]
        .into_boxed_slice(),
    );
    let boot = Boot {
        mode,
        safe: false,
        reason: None,
    };
    let sup = Supervisor::new(defs, policy, Rec::default(), boot).unwrap();
    let mut s = Shell {
        win: ServerWindow::new(Screen { w: 1920, h: 1080 }, mode),
        sup,
        now: 0,
        notices: Vec::new(),
        refuse_hybrid: 0,
    };
    s.rest();
    s
}

impl Shell {
    /// Runs the supervisor to rest, answering stops and readiness.
    fn rest(&mut self) {
        self.sup.tick(self.now);
        for _ in 0..50 {
            let starts = std::mem::take(&mut self.sup.platform().starts);
            let stops = std::mem::take(&mut self.sup.platform().stops);
            if starts.is_empty() && stops.is_empty() {
                break;
            }
            for id in stops {
                self.sup.event(self.now, SvcEvent::Exited(id, 0));
            }
            for id in starts {
                if self.sup.phase(id) == Phase::Starting {
                    self.sup.event(self.now, SvcEvent::Ready(id));
                }
            }
        }
    }

    fn guest_state(&self) -> GuestState {
        match self.sup.phase(PVE) {
            Phase::Running => GuestState::Running,
            Phase::Starting => GuestState::Starting,
            Phase::Stopping => GuestState::Stopping,
            Phase::Stopped => GuestState::Off,
            _ => GuestState::Crashed,
        }
    }

    /// Feeds an event and carries out every effect, including the answers.
    fn feed(&mut self, ev: Event) {
        let mut queue = vec![ev];
        while let Some(ev) = queue.pop() {
            let step = self.win.step(ev);
            self.win.invariants().unwrap();
            let mut answers = Vec::new();
            for e in step.effects() {
                match *e {
                    Effect::RequestMode(m) => {
                        let refuse = m == Mode::Hybrid && self.refuse_hybrid > 0;
                        if refuse {
                            self.refuse_hybrid -= 1;
                        }
                        match (!refuse).then(|| self.sup.set_mode(self.now, m)) {
                            Some(Ok(())) => {
                                self.rest();
                                answers.push(Event::ModeGranted(m));
                            }
                            _ => answers.push(Event::ModeDenied),
                        }
                    }
                    Effect::StartGuest => {
                        self.sup.start(self.now, PVE);
                        self.sup.tick(self.now);
                        answers.push(Event::Guest(GuestState::Starting));
                        self.rest();
                        answers.push(Event::Guest(self.guest_state()));
                    }
                    Effect::Notice(n) => self.notices.push(n),
                    _ => {}
                }
            }
            // Answers are delivered in the order they were produced.
            answers.reverse();
            queue.extend(answers);
        }
    }
}

const ROOMY: Policy = Policy {
    service_cpu: [200, 800, 800],
    service_mem: [128 << 20, 1 << 30, 1 << 30],
};
const TIGHT: Policy = Policy {
    service_cpu: [200, 100, 800],
    service_mem: [128 << 20, 64 << 20, 1 << 30],
};

#[test]
fn the_switch_starts_the_server_on_a_desktop_machine() {
    let mut s = shell(ROOMY, Mode::Desktop);
    assert_eq!(s.sup.phase(SHELL), Phase::Running);
    assert_eq!(
        s.sup.phase(PVE),
        Phase::Stopped,
        "no server on a plain desktop"
    );
    s.feed(Event::Switch);
    assert_eq!(s.sup.mode(), Mode::Hybrid);
    assert_eq!(s.sup.phase(PVE), Phase::Running);
    assert_eq!(
        s.sup.phase(SHELL),
        Phase::Running,
        "the desktop keeps running beside the server"
    );
    assert_eq!(s.win.guest(), GuestState::Running);
    assert_eq!(s.win.presentation(), Presentation::Collapsed);
    assert_eq!(s.win.display_hz(), 1);
    s.feed(Event::Expand);
    assert_eq!(s.win.display_hz(), 30);
    // Hiding the window leaves the server and the desktop as they were.
    s.feed(Event::Close);
    assert_eq!(
        (s.sup.phase(PVE), s.sup.phase(SHELL)),
        (Phase::Running, Phase::Running)
    );
}

#[test]
fn full_server_stops_the_desktop_and_the_chord_brings_it_back() {
    let mut s = shell(ROOMY, Mode::Hybrid);
    s.feed(Event::Switch);
    s.feed(Event::Expand);
    s.feed(Event::GoFullServer);
    assert_eq!(s.sup.mode(), Mode::Server);
    assert_eq!(
        s.sup.phase(SHELL),
        Phase::Stopped,
        "the interactive part is suspended"
    );
    assert_eq!(s.sup.phase(PVE), Phase::Running);
    assert_eq!(s.win.presentation(), Presentation::Fullscreen);
    assert!(s.win.grabbed());
    s.feed(Event::Key {
        key: Key::Ctrl,
        down: true,
    });
    s.feed(Event::Key {
        key: Key::Alt,
        down: true,
    });
    s.feed(Event::Key {
        key: Key::G,
        down: true,
    });
    assert_eq!(s.sup.mode(), Mode::Hybrid);
    assert_eq!(s.sup.phase(SHELL), Phase::Running, "the desktop is back");
    assert_eq!(
        s.sup.phase(PVE),
        Phase::Running,
        "and the server did not stop"
    );
    assert_eq!(s.win.presentation(), Presentation::Windowed);
    assert!(!s.win.grabbed());
}

#[test]
fn a_supervisor_that_has_no_room_refuses_and_nothing_starts() {
    let mut s = shell(TIGHT, Mode::Desktop);
    s.feed(Event::Switch);
    assert_eq!(s.notices, [Notice::ModeDenied]);
    assert_eq!(
        s.sup.mode(),
        Mode::Desktop,
        "the refused switch changed nothing"
    );
    assert_eq!(s.sup.phase(PVE), Phase::Stopped);
    assert_eq!(s.win.guest(), GuestState::Off);
    assert_eq!(
        s.win.presentation(),
        Presentation::Collapsed,
        "the window explains, it does not vanish"
    );
}

#[test]
fn leaving_server_mode_survives_a_supervisor_that_says_no_for_a_while() {
    let mut s = shell(ROOMY, Mode::Hybrid);
    s.feed(Event::Switch);
    s.feed(Event::GoFullServer);
    assert_eq!(s.sup.mode(), Mode::Server);
    s.refuse_hybrid = 2;
    s.feed(Event::Close);
    assert_eq!(
        s.sup.mode(),
        Mode::Server,
        "a refused switch leaves server mode in place"
    );
    for _ in 0..5 {
        s.now += 5_000;
        s.feed(Event::Tick { now_ms: s.now });
        if s.sup.mode() == Mode::Hybrid {
            break;
        }
    }
    assert_eq!(
        s.sup.mode(),
        Mode::Hybrid,
        "the window kept asking until the desktop was back"
    );
    assert_eq!(s.sup.phase(SHELL), Phase::Running);
    assert_eq!(s.sup.phase(PVE), Phase::Running);
}
