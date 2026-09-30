//! Service supervisor and mode switch for NANOX server mode
//! (docs/specs/M11-SERVER.md §2–3).
//!
//! A deterministic state machine on a caller-supplied millisecond clock:
//! it never reads a clock, spawns nothing itself and allocates nothing.
//! The kernel (or a test) implements [`Platform`] (start, stop, kill,
//! probe) and feeds back [`Event`]s; the supervisor decides. The same
//! inputs always give the same calls, so the behaviour is reproducible.
//!
//! * services are `Def`s in a table where dependencies come first;
//! * [`Mode`] (desktop / hybrid / server) selects which run; switching is
//!   atomic — checked against the budgets first, then a transition the
//!   supervisor drives: unwanted parts stop dependents-first (the
//!   interactive part before any service), wanted ones start
//!   dependencies-first (the interactive part after every startable
//!   service), and no start ever overcommits the reservations;
//! * restart policy with exponential backoff, a crash-loop breaker and
//!   health checks; a service that keeps failing ends `Failed` instead of
//!   spinning;
//! * `next_deadline` says when the supervisor must next run: nothing
//!   scheduled means no wake-ups (tickless idle);
//! * [`share`] turns reservations, weights, caps and demands into CPU
//!   shares; [`modecfg`] is the persisted mode record and the safe boot.

#![no_std]
#![forbid(unsafe_code)]

pub mod modecfg;
pub mod share;

pub const MAX_SERVICES: usize = 16;
pub const NAME_MAX: usize = 32;
pub const NOTICES: usize = 32;
const CRASH_RING: usize = 8;

/// Index into the table; dependencies have lower indexes.
pub type ServiceId = u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    Desktop = 0,
    Hybrid = 1,
    Server = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeSet(pub u8);

impl ModeSet {
    pub const NONE: Self = Self(0);
    pub const DESKTOP: Self = Self(1);
    pub const HYBRID: Self = Self(2);
    pub const SERVER: Self = Self(4);
    pub const ALL: Self = Self(7);

    pub const fn union(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }

    pub fn contains(self, m: Mode) -> bool {
        self.0 & (1 << m as u8) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// The desktop / shell side; never runs in server mode.
    Interactive,
    Service,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    Never,
    OnFailure,
    Always,
}

#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub initial_ms: u32,
    pub max_ms: u32,
    /// Uptime after which the failure count starts over.
    pub stable_ms: u32,
    /// More than `limit` exits inside `window_ms` end in `Failed`.
    pub window_ms: u32,
    pub limit: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Health {
    pub interval_ms: u32,
    pub timeout_ms: u32,
    /// Consecutive failed probes before the service is killed.
    pub failures: u8,
}

/// Resource claims; CPU in permille of the machine, memory in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub cpu_reserve: u16,
    pub cpu_weight: u16,
    pub cpu_cap: u16,
    pub mem_reserve: u64,
    pub mem_limit: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Def<'a> {
    pub name: &'a str,
    pub class: Class,
    pub modes: ModeSet,
    pub deps: &'a [ServiceId],
    pub restart: Restart,
    pub backoff: Backoff,
    pub health: Option<Health>,
    /// Time to report ready after start; 0: ready as soon as it is spawned.
    pub ready_timeout_ms: u32,
    /// Graceful stop time before the service is killed.
    pub stop_timeout_ms: u32,
    pub limits: Limits,
    /// May run in safe mode (no network, minimal).
    pub safe: bool,
}

/// What the modes may reserve: per mode (desktop, hybrid, server) the CPU
/// permille and the memory the services together may reserve. The
/// interactive part's reservation comes on top, within the whole machine.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub service_cpu: [u16; 3],
    pub service_mem: [u64; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    NoSuchImage,
    OutOfMemory,
    Denied,
}

pub trait Platform {
    /// Spawns the service; its end is reported with [`Event::Exited`].
    fn start(&mut self, id: ServiceId, def: &Def<'_>) -> Result<(), StartError>;
    /// Asks the service to stop gracefully; [`Event::Exited`] follows.
    fn stop(&mut self, id: ServiceId);
    /// Terminates it now; the supervisor treats it as gone.
    fn kill(&mut self, id: ServiceId);
    /// Starts a health probe; the answer is [`Event::Health`].
    fn probe(&mut self, id: ServiceId);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Ready(ServiceId),
    Exited(ServiceId, i32),
    Health(ServiceId, bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Backoff,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitKind {
    Code(i32),
    Killed,
    StartFailed,
    HealthKilled,
    ReadyTimeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Started,
    Ready,
    Stopping,
    Stopped,
    Exited(ExitKind),
    Backoff(u32),
    CrashLoop,
    HealthFailed,
    KillEscalated,
    ModeAccepted(Mode),
    ModeSettled(Mode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice {
    pub at: u64,
    pub svc: ServiceId,
    pub kind: NoticeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableError {
    TooMany,
    Name,
    DuplicateName,
    DepOrder,
    InteractiveInServer,
    Limits,
    Backoff,
    Health,
    /// The boot mode does not fit the budgets.
    Budget(SwitchError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchError {
    /// The mode's services reserve more CPU than the mode allows.
    ServiceCpu { need: u32, cap: u32 },
    /// More memory than the mode allows.
    ServiceMem { need: u64, cap: u64 },
    /// All reservations together exceed the machine.
    Machine { need: u32 },
}

#[derive(Clone, Copy, Debug)]
struct St {
    phase: Phase,
    since: u64,
    until: u64,
    manual_stop: bool,
    /// Ended without a restart being due (normal exit, `Never`).
    done: bool,
    /// Stop and start again (manual restart).
    bounce: bool,
    fails: u8,
    crashes: [u64; CRASH_RING],
    ncrash: u8,
    next_probe: u64,
    probe_since: Option<u64>,
    health_fail: u8,
}

const ST0: St = St {
    phase: Phase::Stopped,
    since: 0,
    until: 0,
    manual_stop: false,
    done: false,
    bounce: false,
    fails: 0,
    crashes: [0; CRASH_RING],
    ncrash: 0,
    next_probe: 0,
    probe_since: None,
    health_fail: 0,
};

const NOTICE0: Notice = Notice {
    at: 0,
    svc: 0,
    kind: NoticeKind::Started,
};

pub struct Supervisor<'a, P: Platform> {
    defs: &'a [Def<'a>],
    policy: Policy,
    platform: P,
    st: [St; MAX_SERVICES],
    target: Mode,
    safe: bool,
    was_settled: bool,
    notices: [Notice; NOTICES],
    n_head: usize,
    n_len: usize,
    lost: u32,
}

impl<'a, P: Platform> Supervisor<'a, P> {
    /// Validates the table and the boot mode; nothing is started until the
    /// first `tick`.
    pub fn new(
        defs: &'a [Def<'a>],
        policy: Policy,
        platform: P,
        boot: modecfg::Boot,
    ) -> Result<Self, TableError> {
        validate_table(defs)?;
        let s = Self {
            defs,
            policy,
            platform,
            st: [ST0; MAX_SERVICES],
            target: boot.mode,
            safe: boot.safe,
            was_settled: false,
            notices: [NOTICE0; NOTICES],
            n_head: 0,
            n_len: 0,
            lost: 0,
        };
        s.budget(boot.mode).map_err(TableError::Budget)?;
        Ok(s)
    }

    pub fn platform(&mut self) -> &mut P {
        &mut self.platform
    }

    pub fn mode(&self) -> Mode {
        self.target
    }

    pub fn safe_mode(&self) -> bool {
        self.safe
    }

    pub fn phase(&self, id: ServiceId) -> Phase {
        self.st[usize::from(id)].phase
    }

    pub fn defs(&self) -> &'a [Def<'a>] {
        self.defs
    }

    /// Notices dropped because nobody drained the ring.
    pub fn lost_notices(&self) -> u32 {
        self.lost
    }

    /// The oldest undrained notice.
    pub fn notice(&mut self) -> Option<Notice> {
        if self.n_len == 0 {
            return None;
        }
        let n = self.notices[self.n_head];
        self.n_head = (self.n_head + 1) % NOTICES;
        self.n_len -= 1;
        Some(n)
    }

    fn note(&mut self, at: u64, svc: ServiceId, kind: NoticeKind) {
        if self.n_len == NOTICES {
            self.n_head = (self.n_head + 1) % NOTICES;
            self.n_len -= 1;
            self.lost += 1;
        }
        let i = (self.n_head + self.n_len) % NOTICES;
        self.notices[i] = Notice { at, svc, kind };
        self.n_len += 1;
    }

    /// Reservations of the services that run in `mode` against the budgets.
    fn budget(&self, mode: Mode) -> Result<(), SwitchError> {
        let (mut svc_cpu, mut all_cpu, mut mem) = (0u32, 0u32, 0u64);
        for d in self.defs {
            if !self.in_mode(d, mode) {
                continue;
            }
            all_cpu += u32::from(d.limits.cpu_reserve);
            if d.class == Class::Service {
                svc_cpu += u32::from(d.limits.cpu_reserve);
                mem += d.limits.mem_reserve;
            }
        }
        let m = mode as usize;
        if svc_cpu > u32::from(self.policy.service_cpu[m]) {
            return Err(SwitchError::ServiceCpu {
                need: svc_cpu,
                cap: u32::from(self.policy.service_cpu[m]),
            });
        }
        if mem > self.policy.service_mem[m] {
            return Err(SwitchError::ServiceMem {
                need: mem,
                cap: self.policy.service_mem[m],
            });
        }
        if all_cpu > share::TOTAL {
            return Err(SwitchError::Machine { need: all_cpu });
        }
        Ok(())
    }

    fn in_mode(&self, d: &Def<'_>, mode: Mode) -> bool {
        d.modes.contains(mode) && (!self.safe || d.safe)
    }

    /// Switches the mode. Either the whole switch is accepted (and then
    /// carried out over the following ticks) or nothing changes.
    pub fn set_mode(&mut self, now: u64, mode: Mode) -> Result<(), SwitchError> {
        self.budget(mode)?;
        if mode != self.target {
            self.target = mode;
            self.was_settled = false;
            self.note(now, 0, NoticeKind::ModeAccepted(mode));
        }
        self.reconcile(now);
        Ok(())
    }

    /// Manual start: clears a stop, a finished state or a `Failed`.
    pub fn start(&mut self, now: u64, id: ServiceId) {
        let s = &mut self.st[usize::from(id)];
        s.manual_stop = false;
        s.done = false;
        s.fails = 0;
        s.ncrash = 0;
        if s.phase == Phase::Failed || s.phase == Phase::Backoff {
            s.phase = Phase::Stopped;
        }
        self.reconcile(now);
    }

    /// Manual stop: the service and everything that depends on it stop and
    /// stay stopped until `start`.
    pub fn stop(&mut self, now: u64, id: ServiceId) {
        self.st[usize::from(id)].manual_stop = true;
        self.reconcile(now);
    }

    /// Stops the service and its dependents, then starts them again.
    pub fn restart(&mut self, now: u64, id: ServiceId) {
        let s = &mut self.st[usize::from(id)];
        s.manual_stop = false;
        s.done = false;
        s.fails = 0;
        s.ncrash = 0;
        match s.phase {
            Phase::Failed | Phase::Backoff => s.phase = Phase::Stopped,
            Phase::Starting | Phase::Running => s.bounce = true,
            _ => {}
        }
        self.reconcile(now);
    }

    pub fn tick(&mut self, now: u64) {
        self.reconcile(now);
    }

    pub fn event(&mut self, now: u64, e: Event) {
        match e {
            Event::Ready(id) => {
                let i = usize::from(id);
                if i < self.defs.len() && self.st[i].phase == Phase::Starting {
                    self.st[i].phase = Phase::Running;
                    self.st[i].since = now;
                    self.arm_probe(i, now);
                    self.note(now, id, NoticeKind::Ready);
                }
            }
            Event::Exited(id, code) => {
                let i = usize::from(id);
                if i < self.defs.len() {
                    self.exited(now, i, ExitKind::Code(code));
                }
            }
            Event::Health(id, ok) => {
                let i = usize::from(id);
                if i < self.defs.len()
                    && self.st[i].phase == Phase::Running
                    && self.st[i].probe_since.is_some()
                {
                    self.st[i].probe_since = None;
                    self.probe_result(now, i, ok);
                }
            }
        }
        self.reconcile(now);
    }

    fn arm_probe(&mut self, i: usize, now: u64) {
        let s = &mut self.st[i];
        s.health_fail = 0;
        s.probe_since = None;
        s.next_probe = self.defs[i]
            .health
            .map_or(0, |h| now + u64::from(h.interval_ms));
    }

    fn probe_result(&mut self, now: u64, i: usize, ok: bool) {
        let Some(h) = self.defs[i].health else { return };
        self.st[i].next_probe = now + u64::from(h.interval_ms);
        if ok {
            self.st[i].health_fail = 0;
            return;
        }
        self.st[i].health_fail += 1;
        if self.st[i].health_fail >= h.failures {
            self.note(now, i as u8, NoticeKind::HealthFailed);
            self.platform.kill(i as u8);
            self.exited(now, i, ExitKind::HealthKilled);
        }
    }

    /// Wanted by mode, manual state and dependencies.
    fn want(&self) -> [bool; MAX_SERVICES] {
        let mut w = [false; MAX_SERVICES];
        for (i, d) in self.defs.iter().enumerate() {
            let s = &self.st[i];
            w[i] = self.in_mode(d, self.target)
                && !s.manual_stop
                && !s.done
                && d.deps.iter().all(|&x| w[usize::from(x)]);
        }
        w
    }

    fn deps_in(&self, i: usize, phases: &[Phase]) -> bool {
        self.defs[i]
            .deps
            .iter()
            .all(|&d| phases.contains(&self.st[usize::from(d)].phase))
    }

    fn active(&self, i: usize) -> bool {
        matches!(
            self.st[i].phase,
            Phase::Starting | Phase::Running | Phase::Stopping
        )
    }

    /// A start must not push the reservations of what is active over the
    /// target mode's budgets.
    fn fits(&self, i: usize) -> bool {
        let d = &self.defs[i];
        let m = self.target as usize;
        let (mut all_cpu, mut svc_cpu, mut mem) = (u32::from(d.limits.cpu_reserve), 0u32, 0u64);
        if d.class == Class::Service {
            svc_cpu = u32::from(d.limits.cpu_reserve);
            mem = d.limits.mem_reserve;
        }
        for (j, e) in self.defs.iter().enumerate() {
            if j != i && self.active(j) {
                all_cpu += u32::from(e.limits.cpu_reserve);
                if e.class == Class::Service {
                    svc_cpu += u32::from(e.limits.cpu_reserve);
                    mem += e.limits.mem_reserve;
                }
            }
        }
        all_cpu <= share::TOTAL
            && svc_cpu <= u32::from(self.policy.service_cpu[m])
            && mem <= self.policy.service_mem[m]
    }

    fn try_start(&mut self, now: u64, i: usize) {
        let d = self.defs[i];
        match self.platform.start(i as u8, &d) {
            Ok(()) => {
                let s = &mut self.st[i];
                s.since = now;
                s.probe_since = None;
                s.health_fail = 0;
                s.phase = if d.ready_timeout_ms == 0 {
                    Phase::Running
                } else {
                    Phase::Starting
                };
                if d.ready_timeout_ms == 0 {
                    self.arm_probe(i, now);
                }
                self.note(now, i as u8, NoticeKind::Started);
            }
            Err(_) => {
                let s = &mut self.st[i];
                s.phase = Phase::Starting;
                s.since = now;
                self.exited(now, i, ExitKind::StartFailed);
            }
        }
    }

    fn begin_stop(&mut self, now: u64, i: usize) {
        let d = self.defs[i];
        self.platform.stop(i as u8);
        let s = &mut self.st[i];
        s.phase = Phase::Stopping;
        s.until = now + u64::from(d.stop_timeout_ms);
        self.note(now, i as u8, NoticeKind::Stopping);
    }

    fn exited(&mut self, now: u64, i: usize, kind: ExitKind) {
        let d = self.defs[i];
        // Any end of the run ends a manual restart's stop phase too.
        self.st[i].bounce = false;
        match self.st[i].phase {
            Phase::Stopping => {
                let s = &mut self.st[i];
                s.phase = Phase::Stopped;
                s.bounce = false;
                self.note(now, i as u8, NoticeKind::Stopped);
                return;
            }
            Phase::Starting | Phase::Running => {}
            _ => return,
        }
        self.note(now, i as u8, NoticeKind::Exited(kind));
        let failure = !matches!(kind, ExitKind::Code(0));
        let s = &mut self.st[i];
        s.probe_since = None;
        if now.saturating_sub(s.since) >= u64::from(d.backoff.stable_ms) {
            s.fails = 0;
        }
        let no_restart = match d.restart {
            Restart::Never => true,
            Restart::OnFailure => !failure,
            Restart::Always => false,
        };
        if no_restart {
            s.phase = Phase::Stopped;
            s.done = true;
            return;
        }
        // Crash-loop breaker over a sliding window of recent exits.
        let slot = usize::from(s.ncrash) % CRASH_RING;
        s.crashes[slot] = now;
        s.ncrash = s.ncrash.saturating_add(1);
        let recent = s.crashes[..usize::from(s.ncrash).min(CRASH_RING)]
            .iter()
            .filter(|&&t| now.saturating_sub(t) <= u64::from(d.backoff.window_ms))
            .count();
        if recent > usize::from(d.backoff.limit) {
            s.phase = Phase::Failed;
            self.note(now, i as u8, NoticeKind::CrashLoop);
            return;
        }
        let shift = u32::from(s.fails.min(20));
        let delay =
            (u64::from(d.backoff.initial_ms) << shift).min(u64::from(d.backoff.max_ms)) as u32;
        s.fails = s.fails.saturating_add(1);
        s.phase = Phase::Backoff;
        s.until = now + u64::from(delay);
        self.note(now, i as u8, NoticeKind::Backoff(delay));
    }

    fn reconcile(&mut self, now: u64) {
        for _ in 0..(2 * MAX_SERVICES + 4) {
            if !self.pass(now) {
                break;
            }
        }
        let settled = self.settled();
        if settled && !self.was_settled {
            let m = self.target;
            self.note(now, 0, NoticeKind::ModeSettled(m));
        }
        self.was_settled = settled;
    }

    /// One sweep; true if anything changed (another sweep may follow).
    fn pass(&mut self, now: u64) -> bool {
        let n = self.defs.len();
        let want = self.want();
        let mut changed = false;
        // A service is fit to run only if everything below it, transitively,
        // is running; a bounce (manual restart) reaches all dependents.
        let mut deps_ok = [false; MAX_SERVICES];
        let mut stale = [false; MAX_SERVICES];
        for i in 0..n {
            deps_ok[i] = self.defs[i].deps.iter().all(|&d| {
                self.st[usize::from(d)].phase == Phase::Running && deps_ok[usize::from(d)]
            });
            stale[i] =
                self.st[i].bounce || self.defs[i].deps.iter().any(|&d| stale[usize::from(d)]);
        }

        // Timers.
        for i in 0..n {
            let d = self.defs[i];
            match self.st[i].phase {
                Phase::Backoff if now >= self.st[i].until => {
                    self.st[i].phase = Phase::Stopped;
                    changed = true;
                }
                Phase::Stopping if now >= self.st[i].until => {
                    self.note(now, i as u8, NoticeKind::KillEscalated);
                    self.platform.kill(i as u8);
                    self.exited(now, i, ExitKind::Killed);
                    changed = true;
                }
                Phase::Starting
                    if d.ready_timeout_ms > 0
                        && now >= self.st[i].since + u64::from(d.ready_timeout_ms) =>
                {
                    self.platform.kill(i as u8);
                    self.exited(now, i, ExitKind::ReadyTimeout);
                    changed = true;
                }
                Phase::Running => {
                    if let Some(h) = d.health {
                        match self.st[i].probe_since {
                            Some(t) if now >= t + u64::from(h.timeout_ms) => {
                                self.st[i].probe_since = None;
                                self.probe_result(now, i, false);
                                changed = true;
                            }
                            None if now >= self.st[i].next_probe => {
                                self.platform.probe(i as u8);
                                self.st[i].probe_since = Some(now);
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        // Stops, dependents (higher index) first.
        for i in (0..n).rev() {
            if !matches!(self.st[i].phase, Phase::Starting | Phase::Running) {
                continue;
            }
            let runs = want[i] && !stale[i] && deps_ok[i];
            if runs {
                continue;
            }
            // Wait for dependents to be gone first.
            let dependent_active =
                (i + 1..n).any(|j| self.defs[j].deps.contains(&(i as u8)) && self.active(j));
            // The interactive part stops before any service does.
            let interactive_pending = self.defs[i].class == Class::Service
                && (0..n).any(|j| {
                    self.defs[j].class == Class::Interactive && !want[j] && self.active(j)
                });
            if dependent_active || interactive_pending {
                continue;
            }
            self.begin_stop(now, i);
            changed = true;
        }

        // Starts: services first, dependencies first, then the interactive
        // part once nothing startable is left.
        for class in [Class::Service, Class::Interactive] {
            for i in 0..n {
                if self.defs[i].class != class
                    || self.st[i].phase != Phase::Stopped
                    || !want[i]
                    || stale[i]
                    || !self.deps_in(i, &[Phase::Running])
                    || !self.fits(i)
                {
                    continue;
                }
                if class == Class::Interactive {
                    let pending = (0..n).any(|j| {
                        self.defs[j].class == Class::Service
                            && want[j]
                            && self.st[j].phase == Phase::Stopped
                            && self.deps_in(j, &[Phase::Running, Phase::Starting])
                    });
                    if pending {
                        continue;
                    }
                }
                self.try_start(now, i);
                changed = true;
            }
        }
        changed
    }

    /// Every wanted service is `Running` (or `Failed`, which needs a human)
    /// and everything else is stopped.
    pub fn settled(&self) -> bool {
        let want = self.want();
        (0..self.defs.len()).all(|i| {
            let p = self.st[i].phase;
            if want[i] {
                matches!(p, Phase::Running | Phase::Failed)
            } else {
                matches!(p, Phase::Stopped | Phase::Failed)
            }
        })
    }

    /// When the supervisor must run next; None: nothing is scheduled, no
    /// wake-up is needed.
    pub fn next_deadline(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        let mut take = |t: u64| best = Some(best.map_or(t, |b| b.min(t)));
        for (i, d) in self.defs.iter().enumerate() {
            let s = &self.st[i];
            match s.phase {
                Phase::Backoff | Phase::Stopping => take(s.until),
                Phase::Starting if d.ready_timeout_ms > 0 => {
                    take(s.since + u64::from(d.ready_timeout_ms));
                }
                Phase::Running => {
                    if let Some(h) = d.health {
                        match s.probe_since {
                            Some(t) => take(t + u64::from(h.timeout_ms)),
                            None => take(s.next_probe),
                        }
                    }
                }
                _ => {}
            }
        }
        best
    }

    /// CPU shares (permille) for the running services given each one's
    /// current demand; the interactive part's reservation is protected.
    pub fn cpu_plan(&self, demand: &[u32], out: &mut [u32]) -> Result<(), share::ShareError> {
        let mut claims = [share::Claim {
            reserve: 0,
            weight: 0,
            cap: 0,
            demand: 0,
        }; MAX_SERVICES];
        for (i, d) in self.defs.iter().enumerate() {
            if self.st[i].phase == Phase::Running {
                claims[i] = share::Claim {
                    reserve: u32::from(d.limits.cpu_reserve),
                    weight: u32::from(d.limits.cpu_weight),
                    cap: u32::from(d.limits.cpu_cap),
                    demand: demand.get(i).copied().unwrap_or(0),
                };
            }
        }
        share::allocate(&claims[..self.defs.len()], out)
    }

    /// Consistency checks used by tests after every step.
    pub fn check(&self) -> Result<(), &'static str> {
        let n = self.defs.len();
        let want = self.want();
        for i in 0..n {
            let p = self.st[i].phase;
            if matches!(p, Phase::Starting | Phase::Running) {
                let dependent_active =
                    (i + 1..n).any(|j| self.defs[j].deps.contains(&(i as u8)) && self.active(j));
                let interactive_pending = self.defs[i].class == Class::Service
                    && (0..n).any(|j| {
                        self.defs[j].class == Class::Interactive && !want[j] && self.active(j)
                    });
                // A service that has to stop (unwanted, being bounced or
                // above a dependency that is down) may only linger for the
                // two reasons the stop order allows: a dependent is still
                // being stopped, or the interactive part goes first.
                let must_stop =
                    !want[i] || self.st[i].bounce || !self.deps_in(i, &[Phase::Running]);
                if must_stop && !dependent_active && !interactive_pending {
                    return Err("service that must stop is still running without a reason");
                }
            }
        }
        // Wanted and active reservations stay inside the target budgets.
        let m = self.target as usize;
        let (mut svc_cpu, mut mem, mut all_cpu) = (0u32, 0u64, 0u32);
        for (i, d) in self.defs.iter().enumerate() {
            if want[i] && self.active(i) {
                all_cpu += u32::from(d.limits.cpu_reserve);
                if d.class == Class::Service {
                    svc_cpu += u32::from(d.limits.cpu_reserve);
                    mem += d.limits.mem_reserve;
                }
            }
        }
        if all_cpu > share::TOTAL
            || svc_cpu > u32::from(self.policy.service_cpu[m])
            || mem > self.policy.service_mem[m]
        {
            return Err("wanted reservations exceed the target budgets");
        }
        // The interactive part is never active in server mode once settled.
        Ok(())
    }
}

fn validate_table(defs: &[Def<'_>]) -> Result<(), TableError> {
    if defs.len() > MAX_SERVICES {
        return Err(TableError::TooMany);
    }
    for (i, d) in defs.iter().enumerate() {
        if d.name.is_empty() || d.name.len() > NAME_MAX {
            return Err(TableError::Name);
        }
        if defs[..i].iter().any(|e| e.name == d.name) {
            return Err(TableError::DuplicateName);
        }
        if d.deps.iter().any(|&x| usize::from(x) >= i) {
            return Err(TableError::DepOrder);
        }
        if d.class == Class::Interactive && d.modes.contains(Mode::Server) {
            return Err(TableError::InteractiveInServer);
        }
        let l = &d.limits;
        if l.cpu_cap < l.cpu_reserve
            || l.cpu_cap > share::TOTAL as u16
            || l.mem_limit < l.mem_reserve
            || l.cpu_weight == 0
        {
            return Err(TableError::Limits);
        }
        let b = &d.backoff;
        if b.initial_ms == 0
            || b.max_ms < b.initial_ms
            || usize::from(b.limit) >= CRASH_RING
            || b.window_ms == 0
        {
            return Err(TableError::Backoff);
        }
        if let Some(h) = d.health {
            if h.interval_ms == 0 || h.timeout_ms == 0 || h.failures == 0 {
                return Err(TableError::Health);
            }
        }
    }
    Ok(())
}
