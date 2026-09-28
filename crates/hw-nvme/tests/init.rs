//! Controller bring-up against the model: the full sequence, every
//! bounded wait, CSTS.CFS in each phase, admin errors, invalid CAP and
//! configuration values, Identify validation.

mod common;

use common::*;
use hw_nvme::command::admin;
use hw_nvme::controller::AdminStep;
use hw_nvme::status::specific;
use hw_nvme::{
    CapError, ConfigError, Controller, Error, IdentifyError, Phase, Status, StatusClass,
    TimeoutPhase, Unsupported,
};

fn init_err(mcfg: ModelConfig, faults: Faults) -> (Model, Controller, Error) {
    let mut m = Model::new(mcfg);
    m.faults = faults;
    let mut c = Controller::new(&mut m, config()).expect("new");
    let e = c.init(&mut m).expect_err("init must fail");
    assert_eq!(c.phase(), Phase::Failed);
    (m, c, e)
}

#[test]
fn full_init_programs_controller_and_parses_identify() {
    let (m, c) = ready(ModelConfig::default(), config());
    assert_eq!(c.phase(), Phase::Ready);
    let info = c.controller_info().unwrap();
    assert_eq!(info.vid, 0x144D);
    assert_eq!(info.mdts, 9);
    assert_eq!(info.nn, 1);
    assert_eq!(&info.serial()[..13], b"SN-NANOX-0001");
    assert_eq!(&info.model()[..16], b"NANOX MODEL NVME");
    let dbg = format!("{info:?}");
    assert!(
        !dbg.contains("SN-NANOX") && !dbg.contains("NANOX MODEL"),
        "{dbg}"
    );
    let ns = c.namespace().unwrap();
    assert_eq!(ns.nsze, 16384);
    assert_eq!(ns.lba_size(), 512);
    assert_eq!(ns.formats().len(), 2);
    // MDTS 2^9 * 4 KiB = 2 MiB is below the PRP capacity of 2 list pages.
    assert_eq!(c.max_transfer_bytes(), 2 << 20);
    // CC: EN, IOSQES 6, IOCQES 4, everything else 0.
    assert_eq!(m.cc(), 0x0046_0001);
    assert_eq!(m.io_queues(), (1, 1));
    assert_eq!(m.stats.fetched, 5);
    m.assert_clean();
}

#[test]
fn smi_profile_limits() {
    let (m, c) = ready(ModelConfig::smi(), config());
    assert_eq!(c.controller_info().unwrap().vid, 0x126F);
    assert_eq!(c.max_transfer_bytes(), 128 * 1024);
    m.assert_clean();
}

#[test]
fn enabled_controller_is_disabled_first() {
    let mcfg = ModelConfig {
        initially_enabled: true,
        ..ModelConfig::default()
    };
    let (m, c) = ready(mcfg, config());
    assert_eq!(c.phase(), Phase::Ready);
    assert_eq!(m.stats.resets, 1);
    m.assert_clean();
}

#[test]
fn enable_timeout() {
    let faults = Faults {
        rdy1_never: true,
        ..Faults::default()
    };
    let (m, _, e) = init_err(ModelConfig::default(), faults);
    assert_eq!(e, Error::Timeout(TimeoutPhase::Enable));
    // Bounded by CAP.TO (2 units = 1 s) plus a little.
    assert!(m.now >= 1_000_000_000 && m.now < 1_100_000_000, "{}", m.now);
    m.assert_clean();
}

#[test]
fn disable_timeout() {
    let mcfg = ModelConfig {
        initially_enabled: true,
        to: 1,
        ..ModelConfig::default()
    };
    let faults = Faults {
        rdy0_never: true,
        ..Faults::default()
    };
    let (m, _, e) = init_err(mcfg, faults);
    assert_eq!(e, Error::Timeout(TimeoutPhase::Disable));
    assert!(m.now >= 500_000_000 && m.now < 600_000_000, "{}", m.now);
    m.assert_clean();
}

const INIT_STEPS: [AdminStep; 5] = [
    AdminStep::IdentifyController,
    AdminStep::IdentifyNamespace,
    AdminStep::SetNumQueues,
    AdminStep::CreateIoCq,
    AdminStep::CreateIoSq,
];

#[test]
fn admin_timeout_in_every_step() {
    for (n, step) in INIT_STEPS.iter().enumerate() {
        let mut m = Model::new(ModelConfig::default());
        let target = n as u64 + 1;
        m.set_hook(move |i| {
            if i.seq == target {
                Action::Hang
            } else {
                Action::Normal
            }
        });
        let mut c = Controller::new(&mut m, config()).unwrap();
        assert_eq!(
            c.init(&mut m),
            Err(Error::Timeout(TimeoutPhase::Admin(*step)))
        );
        m.assert_clean();
    }
}

#[test]
fn fatal_on_enable() {
    let faults = Faults {
        cfs_on_enable: true,
        ..Faults::default()
    };
    let (m, _, e) = init_err(ModelConfig::default(), faults);
    assert_eq!(e, Error::ControllerFatal);
    m.assert_clean();
}

#[test]
fn fatal_in_every_admin_step() {
    for n in 1..=5u64 {
        let mut m = Model::new(ModelConfig::default());
        m.set_hook(move |i| {
            if i.seq == n {
                Action::Fatal
            } else {
                Action::Normal
            }
        });
        let mut c = Controller::new(&mut m, config()).unwrap();
        assert_eq!(c.init(&mut m), Err(Error::ControllerFatal), "step {n}");
        assert_eq!(c.phase(), Phase::Failed);
        // A later reset (fault was one-shot) brings the controller up.
        m.clear_hook();
        c.reset(&mut m, &mut |_| panic!("nothing outstanding"))
            .expect("reset after fatal");
        assert_eq!(c.phase(), Phase::Ready);
        m.assert_clean();
    }
}

#[test]
fn admin_error_status_is_reported() {
    let mut m = Model::new(ModelConfig::default());
    let bad = Status::new(1, specific::INVALID_QUEUE_SIZE, false, true);
    m.set_hook(move |i| {
        if i.qid == 0 && i.opcode == admin::CREATE_IO_CQ {
            Action::Status(bad)
        } else {
            Action::Normal
        }
    });
    let mut c = Controller::new(&mut m, config()).unwrap();
    let e = c.init(&mut m).unwrap_err();
    assert_eq!(
        e,
        Error::AdminCommand {
            step: AdminStep::CreateIoCq,
            status: bad
        }
    );
    assert_eq!(bad.class(), StatusClass::CommandSpecific);
    m.assert_clean();
}

fn new_err(mcfg: ModelConfig) -> Error {
    let mut m = Model::new(mcfg);
    let e = Controller::new(&mut m, config()).unwrap_err();
    // Rejection happens before any register write.
    assert_eq!(m.cc(), 0);
    assert_eq!(m.stats.doorbells, 0);
    m.assert_clean();
    e
}

#[test]
fn invalid_capabilities_are_rejected() {
    let d = ModelConfig::default;
    assert_eq!(
        new_err(ModelConfig {
            cap_override: Some(u64::MAX),
            ..d()
        }),
        Error::DeviceGone
    );
    assert_eq!(
        new_err(ModelConfig { mqes: 0, ..d() }),
        Error::InvalidCapabilities(CapError::QueueEntries)
    );
    assert_eq!(
        new_err(ModelConfig { css: 0, ..d() }),
        Error::InvalidCapabilities(CapError::NoCommandSet)
    );
    assert_eq!(
        new_err(ModelConfig {
            mpsmin: 3,
            mpsmax: 2,
            ..d()
        }),
        Error::InvalidCapabilities(CapError::PageSizeRange)
    );
    assert_eq!(
        new_err(ModelConfig { css: 0x40, ..d() }),
        Error::Unsupported(Unsupported::CommandSet)
    );
    assert_eq!(
        new_err(ModelConfig { mpsmin: 1, ..d() }),
        Error::Unsupported(Unsupported::PageSize)
    );
    assert_eq!(
        new_err(ModelConfig { version: 0, ..d() }),
        Error::Unsupported(Unsupported::Version)
    );
    assert_eq!(
        new_err(ModelConfig {
            version: u32::MAX,
            ..d()
        }),
        Error::DeviceGone
    );
    // Doorbell stride 2^15 * 4 puts CQ1 far outside a 16 KiB BAR.
    assert_eq!(
        new_err(ModelConfig { dstrd: 15, ..d() }),
        Error::Config(ConfigError::DoorbellOutsideBar)
    );
    // MQES 31: 32 entries fit, 33 do not.
    let mut m = Model::new(ModelConfig { mqes: 31, ..d() });
    assert!(Controller::new(&mut m, config()).is_ok());
    let mut cfg = config();
    cfg.io.entries = 33;
    assert_eq!(
        Controller::new(&mut m, cfg).unwrap_err(),
        Error::Config(ConfigError::IoEntries)
    );
}

/// A mutation of a valid configuration.
type ConfigEdit = Box<dyn Fn(&mut hw_nvme::Config)>;

#[test]
fn invalid_config_is_rejected() {
    let mut m = Model::new(ModelConfig::default());
    let cases: Vec<(ConfigEdit, ConfigError)> = vec![
        (Box::new(|c| c.admin.entries = 1), ConfigError::AdminEntries),
        (
            Box::new(|c| c.admin.entries = 4097),
            ConfigError::AdminEntries,
        ),
        (Box::new(|c| c.io.entries = 1), ConfigError::IoEntries),
        (Box::new(|c| c.io.sq += 64), ConfigError::Alignment),
        (Box::new(|c| c.identify_buffer += 8), ConfigError::Alignment),
        (
            Box::new(|c| c.io.cq = u64::MAX & !0xFFF),
            ConfigError::Region,
        ),
        (Box::new(|c| c.nsid = 0), ConfigError::Namespace),
        (Box::new(|c| c.nsid = u32::MAX), ConfigError::Namespace),
        (Box::new(|c| c.io_timeout_ns = 0), ConfigError::Timeout),
        (
            Box::new(|c| c.prp_pages_per_command = 65),
            ConfigError::PrpPool,
        ),
        (
            Box::new(|c| c.prp_pool = u64::MAX & !0xFFF),
            ConfigError::PrpPool,
        ),
        (
            Box::new(|c| c.bar_size = 0x1004),
            ConfigError::DoorbellOutsideBar,
        ),
    ];
    for (edit, want) in cases {
        let mut cfg = config();
        edit(&mut cfg);
        assert_eq!(
            Controller::new(&mut m, cfg).unwrap_err(),
            Error::Config(want)
        );
    }
    assert_eq!(m.stats.doorbells, 0);
    m.assert_clean();
}

#[test]
fn identify_data_is_validated() {
    let d = ModelConfig::default;
    let cases = [
        (
            ModelConfig { sqes: 0x55, ..d() },
            Error::Unsupported(Unsupported::EntrySize),
        ),
        (
            ModelConfig { cqes: 0x33, ..d() },
            Error::Unsupported(Unsupported::EntrySize),
        ),
        (ModelConfig { nn: 0, ..d() }, Error::NamespaceNotFound),
        (
            ModelConfig {
                ncap: Some(20_000),
                ..d()
            },
            Error::Identify(IdentifyError::Capacity),
        ),
        (
            ModelConfig { metadata: 8, ..d() },
            Error::Unsupported(Unsupported::Metadata),
        ),
    ];
    for (mcfg, want) in cases {
        let (m, _, e) = init_err(mcfg, Faults::default());
        assert_eq!(e, want);
        m.assert_clean();
    }
}

#[test]
fn step_api_reports_phases() {
    let mut m = Model::new(ModelConfig::default());
    let mut c = Controller::new(&mut m, config()).unwrap();
    assert_eq!(c.phase(), Phase::Idle);
    c.start_reset();
    let mut seen = Vec::new();
    loop {
        let p = c.phase();
        if seen.last() != Some(&p) {
            seen.push(p);
        }
        if c.step(&mut m, &mut |_| {}).unwrap() == hw_nvme::Progress::Ready {
            break;
        }
    }
    assert_eq!(
        seen,
        [
            Phase::Disabling,
            Phase::Enabling,
            Phase::Admin(AdminStep::IdentifyController),
            Phase::Admin(AdminStep::IdentifyNamespace),
            Phase::Admin(AdminStep::SetNumQueues),
            Phase::Admin(AdminStep::CreateIoCq),
            Phase::Admin(AdminStep::CreateIoSq),
        ]
    );
    assert_eq!(c.phase(), Phase::Ready);
    m.assert_clean();
}
