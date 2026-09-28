//! Controller bring-up against the model: the full sequence, BIOS handoff,
//! every bounded wait, host system error, recovery and shutdown.

mod common;

use common::*;
use hw_xhci::controller::{InitPhase, State};
use hw_xhci::{Controller, Error, Wait};

fn init_err(mcfg: ModelConfig) -> (Hw, Controller, Error) {
    let mut hw = Hw::new(mcfg);
    let mut c = Controller::new(&mut hw, config()).expect("new");
    let e = c.init(&mut hw).expect_err("init must fail");
    assert_eq!(c.state(), State::Failed(e));
    (hw, c, e)
}

#[test]
fn full_bring_up() {
    let (hw, c) = running(ModelConfig::default(), config());
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.init_phase(), InitPhase::Done);
    assert_eq!(c.max_slots_enabled(), 16);
    assert!(hw.running());
    // BIOS released ownership, the OS owns it, SMIs are off and the pending
    // SMI event bit was cleared.
    assert_eq!(hw.legacy_sup() & (1 << 16), 0);
    assert_ne!(hw.legacy_sup() & (1 << 24), 0);
    assert_eq!(
        hw.legacy_ctl() & (1 | 1 << 4 | 1 << 13 | 1 << 14 | 1 << 15),
        0
    );
    assert_eq!(hw.legacy_ctl() >> 29, 0);
    assert_eq!(hw.stats.hc_resets, 1);
    assert!(c.is_usb3_port(3) && !c.is_usb3_port(1));
    hw.assert_clean();
}

#[test]
fn bring_up_variants() {
    for (name, mcfg) in [
        (
            "64-byte contexts",
            ModelConfig {
                csz: true,
                ..ModelConfig::default()
            },
        ),
        (
            "no scratchpads",
            ModelConfig {
                scratchpads: 0,
                ..ModelConfig::default()
            },
        ),
        (
            "many scratchpads",
            ModelConfig {
                scratchpads: 40,
                ..ModelConfig::default()
            },
        ),
        (
            "no legacy capability",
            ModelConfig {
                legacy: false,
                ..ModelConfig::default()
            },
        ),
        (
            "BIOS not owner",
            ModelConfig {
                bios_owned: false,
                ..ModelConfig::default()
            },
        ),
        (
            "left running by firmware",
            ModelConfig {
                initially_running: true,
                ..ModelConfig::default()
            },
        ),
        (
            "32-bit only",
            ModelConfig {
                ac64: false,
                ..ModelConfig::default()
            },
        ),
    ] {
        let (hw, c) = running(mcfg, config());
        assert_eq!(c.state(), State::Running, "{name}");
        hw.assert_clean();
    }
}

#[test]
fn every_wait_is_bounded() {
    let cases = [
        (
            ModelConfig {
                bios_release_us: None,
                ..ModelConfig::default()
            },
            Wait::BiosHandoff,
        ),
        (
            ModelConfig {
                initially_running: true,
                halt_us: None,
                ..ModelConfig::default()
            },
            Wait::Halt,
        ),
        (
            ModelConfig {
                reset_us: None,
                ..ModelConfig::default()
            },
            Wait::Reset,
        ),
        (
            ModelConfig {
                cnr_us: None,
                ..ModelConfig::default()
            },
            Wait::ControllerNotReady,
        ),
    ];
    for (mcfg, want) in cases {
        let (hw, _, e) = init_err(mcfg);
        assert_eq!(e, Error::Timeout(want));
        // Bounded by the configured timeout (at most 1 s) plus a little.
        assert!(hw.now < 1_200_000, "{want:?}: {}", hw.now);
    }
}

#[test]
fn forced_bios_takeover() {
    let mut hw = Hw::new(ModelConfig {
        bios_release_us: None,
        ..ModelConfig::default()
    });
    let cfg = hw_xhci::controller::Config {
        force_bios_takeover: true,
        ..config()
    };
    let mut c = Controller::new(&mut hw, cfg).unwrap();
    c.init(&mut hw).expect("takeover");
    assert_eq!(hw.legacy_sup() & (1 << 16), 0);
    assert_eq!(c.state(), State::Running);
    hw.assert_clean();
}

#[test]
fn host_system_error_fails_and_recovers() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    c.noop(&mut hw).unwrap();
    hw.trigger_hse();
    assert_eq!(c.noop(&mut hw), Err(Error::HostSystemError));
    assert_eq!(c.state(), State::Failed(Error::HostSystemError));
    assert_eq!(c.poll(&mut hw), Err(Error::HostSystemError));
    c.recover(&mut hw).expect("recover");
    assert_eq!(c.state(), State::Running);
    assert_eq!(c.stats().recoveries, 1);
    c.noop(&mut hw).unwrap();
    hw.assert_clean();
}

#[test]
fn host_controller_error_is_distinct() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.trigger_hce();
    assert_eq!(c.poll(&mut hw), Err(Error::HostControllerError));
    c.recover(&mut hw).unwrap();
    hw.assert_clean();
}

#[test]
fn shutdown_frees_all_memory_and_init_again() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    assert!(hw.allocated() > 0);
    c.shutdown(&mut hw).expect("shutdown");
    assert_eq!(c.state(), State::Uninit);
    assert_eq!(hw.allocated(), 0, "DMA memory leaked");
    assert!(!hw.running());
    c.init(&mut hw).expect("init after shutdown");
    c.noop(&mut hw).unwrap();
    hw.assert_clean();
}

#[test]
fn invalid_configuration_is_rejected_without_register_writes() {
    let mut hw = Hw::new(ModelConfig::default());
    for cfg in [
        hw_xhci::controller::Config {
            command_ring_trbs: 4,
            ..config()
        },
        hw_xhci::controller::Config {
            event_ring_trbs: 8,
            ..config()
        },
        hw_xhci::controller::Config {
            max_slots: 0,
            ..config()
        },
        hw_xhci::controller::Config {
            mmio_len: 0x100,
            ..config()
        },
    ] {
        assert!(Controller::new(&mut hw, cfg).is_err());
    }
    assert!(!hw.running());
    assert_eq!(hw.stats.hc_resets, 0);
}
