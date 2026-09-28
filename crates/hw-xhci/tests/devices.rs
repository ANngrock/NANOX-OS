//! Commands, ports and devices against the model: command ring wrap and
//! abort, USB2/USB3 port reset, enumeration (full speed, EP0 re-evaluation,
//! SuperSpeed, BSR, 64-byte contexts), configuration and interrupt IN
//! transfers, stall recovery on both endpoints, disconnect with transfers in
//! flight (each reported exactly once), malformed descriptors, controller
//! recovery and memory accounting.

mod common;

use std::collections::HashMap;

use common::*;
use hw_xhci::controller::{Config, DeviceState, State};
use hw_xhci::descriptor::{find_boot_keyboard, SetupPacket};
use hw_xhci::{Completion, Controller, DescError, Error, Notification, Speed, TransferId, Wait};

/// Attaches the device on `port` and configures its boot keyboard interrupt
/// IN endpoint. Returns (slot, dci).
fn attach_keyboard(hw: &mut Hw, c: &mut Controller, port: u8) -> (u8, u8) {
    let dev = c.attach(hw, port).expect("attach");
    let mut buf = [0u8; 256];
    let n = c
        .read_configuration(hw, dev.slot, 0, &mut buf)
        .expect("configuration");
    let kb = find_boot_keyboard(&buf[..n])
        .expect("parse")
        .expect("boot keyboard");
    let dci = c
        .configure_interrupt_in(
            hw,
            dev.slot,
            kb.config_value,
            &kb.endpoint,
            kb.companion.as_ref(),
        )
        .expect("configure");
    (dev.slot, dci)
}

/// Polls until `want` transfer completions arrived; other notifications are
/// collected separately. Panics on duplicates or when polls run out.
fn wait_transfers(
    hw: &mut Hw,
    c: &mut Controller,
    want: usize,
) -> (HashMap<TransferId, Completion>, Vec<Notification>) {
    let mut got = HashMap::new();
    let mut other = Vec::new();
    for _ in 0..200_000 {
        match c.poll(hw).expect("poll") {
            Some(Notification::Transfer(t)) => {
                assert!(got.insert(t.id, t).is_none(), "{:?} reported twice", t.id);
            }
            Some(n) => other.push(n),
            None => {
                let _ = hw_xhci::Clock::now_us(hw);
            }
        }
        if got.len() == want {
            return (got, other);
        }
    }
    panic!("only {} of {want} transfers completed", got.len());
}

/// Drains notifications until `poll` returns `None` a few times in a row.
fn drain(hw: &mut Hw, c: &mut Controller) -> Vec<Notification> {
    let mut out = Vec::new();
    let mut idle = 0;
    while idle < 50 {
        match c.poll(hw).expect("poll") {
            Some(n) => {
                out.push(n);
                idle = 0;
            }
            None => {
                idle += 1;
                let _ = hw_xhci::Clock::now_us(hw);
            }
        }
    }
    out
}

#[test]
fn command_ring_wraps_through_link_trbs() {
    let cfg = Config {
        command_ring_trbs: 8,
        ..config()
    };
    let (mut hw, mut c) = running(ModelConfig::default(), cfg);
    for i in 0..100 {
        c.noop(&mut hw)
            .unwrap_or_else(|e| panic!("noop {i}: {e:?}"));
    }
    assert_eq!(
        hw.stats
            .commands
            .iter()
            .filter(|&&t| t == ty::NOOP_COMMAND)
            .count(),
        100
    );
    hw.assert_clean();
}

#[test]
fn event_ring_wraps() {
    let cfg = Config {
        event_ring_trbs: 16,
        ..config()
    };
    let (mut hw, mut c) = running(ModelConfig::default(), cfg);
    for _ in 0..200 {
        c.noop(&mut hw).unwrap();
    }
    assert!(hw.stats.events >= 200);
    hw.assert_clean();
}

#[test]
fn hung_command_is_aborted_and_the_ring_restarts() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    let mut first = true;
    hw.cmd_hook = Some(Box::new(move |_| {
        if std::mem::take(&mut first) {
            CmdAction::Hang
        } else {
            CmdAction::Normal
        }
    }));
    assert_eq!(c.noop(&mut hw), Err(Error::Timeout(Wait::Command)));
    assert_eq!(hw.stats.aborts, 1);
    assert_eq!(c.stats().command_aborts, 1);
    assert_eq!(c.state(), State::Running);
    for _ in 0..20 {
        c.noop(&mut hw).unwrap();
    }
    hw.assert_clean();
}

#[test]
fn abort_that_never_finishes_fails_the_controller() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.cmd_hook = Some(Box::new(|_| CmdAction::HangForever));
    assert_eq!(c.noop(&mut hw), Err(Error::Timeout(Wait::CommandAbort)));
    assert_eq!(c.state(), State::Failed(Error::Timeout(Wait::CommandAbort)));
    hw.cmd_hook = None;
    c.recover(&mut hw).unwrap();
    c.noop(&mut hw).unwrap();
    hw.assert_clean();
}

#[test]
fn port_reset_usb2_and_usb3() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    assert_eq!(c.reset_port(&mut hw, 1), Err(Error::PortNotConnected));
    assert_eq!(c.reset_port(&mut hw, 0), Err(Error::InvalidPort));
    assert_eq!(c.reset_port(&mut hw, 5), Err(Error::InvalidPort));
    hw.connect(1, UsbDevice::keyboard());
    let st = c.reset_port(&mut hw, 1).expect("USB2 reset");
    assert!(st.enabled() && st.connected());
    assert_eq!(st.speed(), Some(Speed::Full));
    assert_eq!(hw.stats.port_resets, 1);
    // USB3 trains on its own: no reset needed.
    hw.connect(3, UsbDevice::superspeed());
    let st = c.reset_port(&mut hw, 3).expect("USB3 enable");
    assert!(st.enabled());
    assert_eq!(st.speed(), Some(Speed::Super));
    assert_eq!(hw.stats.warm_resets, 0);
    // SS.Inactive needs a warm reset.
    hw.usb3_inactive(3);
    let st = c.reset_port(&mut hw, 3).expect("warm reset");
    assert!(st.enabled());
    assert_eq!(hw.stats.warm_resets, 1);
    // Change bits were cleared by the driver and reported, not left set.
    assert_eq!(hw.portsc(1) & 0x00FE_0000, 0, "{:#x}", hw.portsc(1));
    hw.assert_clean();
}

#[test]
fn enumeration_variants() {
    struct Case {
        name: &'static str,
        port: u8,
        dev: UsbDevice,
        mcfg: ModelConfig,
        bsr: bool,
        mps: u16,
        evaluate: bool,
    }
    let cases = [
        Case {
            name: "full speed",
            port: 1,
            dev: UsbDevice::keyboard(),
            mcfg: ModelConfig::default(),
            bsr: false,
            mps: 8,
            evaluate: false,
        },
        Case {
            name: "EP0 64 bytes",
            port: 2,
            dev: UsbDevice::full_speed_mps64(),
            mcfg: ModelConfig::default(),
            bsr: false,
            mps: 64,
            evaluate: true,
        },
        Case {
            name: "SuperSpeed",
            port: 3,
            dev: UsbDevice::superspeed(),
            mcfg: ModelConfig::default(),
            bsr: false,
            mps: 512,
            evaluate: false,
        },
        Case {
            name: "BSR",
            port: 1,
            dev: UsbDevice::full_speed_mps64(),
            mcfg: ModelConfig::default(),
            bsr: true,
            mps: 64,
            evaluate: false,
        },
        Case {
            name: "64-byte contexts",
            port: 2,
            dev: UsbDevice::full_speed_mps64(),
            mcfg: ModelConfig {
                csz: true,
                ..ModelConfig::default()
            },
            bsr: false,
            mps: 64,
            evaluate: true,
        },
    ];
    for k in cases {
        let cfg = Config {
            address_with_bsr: k.bsr,
            ..config()
        };
        let (mut hw, mut c) = running(k.mcfg, cfg);
        let want_desc = k.dev.device_desc.clone();
        hw.connect(k.port, k.dev);
        let d = c
            .attach(&mut hw, k.port)
            .unwrap_or_else(|e| panic!("{}: {e:?}", k.name));
        assert_eq!(d.ep0_max_packet, k.mps, "{}", k.name);
        assert_eq!(
            d.descriptor.vendor,
            u16::from_le_bytes([want_desc[8], want_desc[9]]),
            "{}",
            k.name
        );
        assert_eq!(
            hw.device(k.port).unwrap().address,
            d.slot,
            "{}: device got its address",
            k.name
        );
        let evals = hw
            .stats
            .commands
            .iter()
            .filter(|&&t| t == ty::EVALUATE_CONTEXT)
            .count();
        assert_eq!(evals > 0, k.evaluate, "{}", k.name);
        let addrs = hw
            .stats
            .commands
            .iter()
            .filter(|&&t| t == ty::ADDRESS_DEVICE)
            .count();
        assert_eq!(addrs, if k.bsr { 2 } else { 1 }, "{}", k.name);
        assert_eq!(
            c.device_info(d.slot).unwrap().state,
            DeviceState::Addressed,
            "{}",
            k.name
        );
        hw.assert_clean();
    }
}

#[test]
fn keyboard_reports_arrive_and_short_packets_are_measured() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.connect(1, UsbDevice::keyboard());
    let (slot, dci) = attach_keyboard(&mut hw, &mut c, 1);
    assert_eq!(dci, 3, "EP 0x81 is DCI 3");
    assert_eq!(
        hw.device(1).unwrap().configuration,
        1,
        "SET_CONFIGURATION sent"
    );
    assert_eq!(c.device_info(slot).unwrap().state, DeviceState::Configured);
    let buf = USER_BASE;
    let id = c.submit_interrupt_in(&mut hw, slot, dci, buf, 8).unwrap();
    // Nothing arrives until the device has a report (NAK).
    for _ in 0..100 {
        assert!(!matches!(
            c.poll(&mut hw).unwrap(),
            Some(Notification::Transfer(_))
        ));
    }
    hw.push_report(1, &[0, 0, 4, 0, 0, 0, 0, 0]);
    let (got, _) = wait_transfers(&mut hw, &mut c, 1);
    assert_eq!(got[&id].result, Ok(8));
    let mut b = [0u8; 8];
    hw.mem_read(buf, &mut b);
    assert_eq!(b, [0, 0, 4, 0, 0, 0, 0, 0]);
    // Short report.
    let id = c.submit_interrupt_in(&mut hw, slot, dci, buf, 8).unwrap();
    hw.push_report(1, &[1, 2, 3]);
    let (got, _) = wait_transfers(&mut hw, &mut c, 1);
    assert_eq!(got[&id].result, Ok(3));
    hw.assert_clean();
}

#[test]
fn transfer_ring_wraps() {
    let cfg = Config {
        transfer_ring_trbs: 8,
        ..config()
    };
    let (mut hw, mut c) = running(ModelConfig::default(), cfg);
    hw.connect(1, UsbDevice::keyboard());
    let (slot, dci) = attach_keyboard(&mut hw, &mut c, 1);
    for i in 0..60u8 {
        let buf = USER_BASE + 64 * u64::from(i % 4);
        let id = c.submit_interrupt_in(&mut hw, slot, dci, buf, 8).unwrap();
        hw.push_report(1, &[i; 8]);
        let (got, _) = wait_transfers(&mut hw, &mut c, 1);
        assert_eq!(got[&id].result, Ok(8), "transfer {i}");
        let mut b = [0u8; 8];
        hw.mem_read(buf, &mut b);
        assert_eq!(b, [i; 8]);
    }
    // Control transfers on EP0 wrap its ring as well.
    for _ in 0..20 {
        let mut d = [0u8; 18];
        assert_eq!(
            c.control_in(&mut hw, slot, SetupPacket::get_descriptor(1, 0, 18), &mut d),
            Ok(18)
        );
    }
    hw.assert_clean();
}

#[test]
fn interrupt_stall_is_recovered() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.connect(1, UsbDevice::keyboard());
    let (slot, dci) = attach_keyboard(&mut hw, &mut c, 1);
    hw.device_mut(1).unwrap().stall_interrupt = true;
    let id = c
        .submit_interrupt_in(&mut hw, slot, dci, USER_BASE, 8)
        .unwrap();
    let (got, _) = wait_transfers(&mut hw, &mut c, 1);
    assert_eq!(got[&id].result, Err(Error::Stall));
    // Deferred recovery runs on the next polls.
    drain(&mut hw, &mut c);
    assert_eq!(c.stats().endpoint_recoveries, 1);
    assert_eq!(
        hw.device(1).unwrap().halt_cleared,
        1,
        "CLEAR_FEATURE(ENDPOINT_HALT)"
    );
    // Reset Endpoint + Set TR Dequeue leave it Stopped (3) until the next
    // doorbell; it must no longer be Halted (2).
    assert_eq!(hw.ep_state(slot, dci), 3, "endpoint recovered");
    let id = c
        .submit_interrupt_in(&mut hw, slot, dci, USER_BASE, 8)
        .unwrap();
    hw.push_report(1, &[9; 8]);
    let (got, _) = wait_transfers(&mut hw, &mut c, 1);
    assert_eq!(got[&id].result, Ok(8));
    hw.assert_clean();
}

#[test]
fn control_stall_is_recovered() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.connect(1, UsbDevice::keyboard());
    let d = c.attach(&mut hw, 1).unwrap();
    let mut buf = [0u8; 64];
    // String descriptors are not implemented by the model device: STALL.
    assert_eq!(
        c.control_in(
            &mut hw,
            d.slot,
            SetupPacket::get_descriptor(3, 0, 64),
            &mut buf
        ),
        Err(Error::Stall)
    );
    assert_eq!(c.stats().endpoint_recoveries, 1);
    let mut dd = [0u8; 18];
    assert_eq!(
        c.control_in(
            &mut hw,
            d.slot,
            SetupPacket::get_descriptor(1, 0, 18),
            &mut dd
        ),
        Ok(18)
    );
    hw.assert_clean();
}

#[test]
fn disconnect_with_transfers_in_flight() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.connect(1, UsbDevice::keyboard());
    hw.connect(2, UsbDevice::keyboard());
    let (slot1, dci1) = attach_keyboard(&mut hw, &mut c, 1);
    let (slot2, dci2) = attach_keyboard(&mut hw, &mut c, 2);
    let before = hw.allocated();
    let ids: Vec<TransferId> = (0..3)
        .map(|i| {
            c.submit_interrupt_in(&mut hw, slot1, dci1, USER_BASE + 64 * i, 8)
                .unwrap()
        })
        .collect();
    let keep = c
        .submit_interrupt_in(&mut hw, slot2, dci2, USER_BASE + 0x1000, 8)
        .unwrap();
    hw.disconnect(1);
    let (got, other) = wait_transfers(&mut hw, &mut c, 3);
    for id in &ids {
        assert_eq!(got[id].result, Err(Error::Disconnected), "{id:?}");
    }
    let mut notes = other;
    notes.extend(drain(&mut hw, &mut c));
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::DeviceDetached { slot, port: 1 } if *slot == slot1)),
        "{notes:?}"
    );
    assert!(!hw.slot_enabled(slot1), "Disable Slot issued");
    assert!(c.device_info(slot1).is_none());
    assert!(hw.allocated() < before, "slot memory freed");
    // The other device is unaffected.
    hw.push_report(2, &[7; 8]);
    let (got, _) = wait_transfers(&mut hw, &mut c, 1);
    assert_eq!(got[&keep].result, Ok(8));
    assert_eq!(
        c.submit_interrupt_in(&mut hw, slot1, dci1, USER_BASE, 8),
        Err(Error::InvalidSlot)
    );
    // Reconnect and enumerate again.
    hw.connect(1, UsbDevice::keyboard());
    let (slot, _) = attach_keyboard(&mut hw, &mut c, 1);
    assert!(c.device_info(slot).is_some());
    hw.assert_clean();
}

/// A corruption applied to a model device.
type DeviceEdit = Box<dyn Fn(&mut UsbDevice)>;

#[test]
fn malformed_descriptors_release_the_slot() {
    let cases: Vec<(&str, DeviceEdit, Error)> = vec![
        (
            "bLength 0",
            Box::new(|d| d.device_desc[0] = 0),
            Error::Descriptor(DescError::BadLength),
        ),
        (
            "short device descriptor",
            Box::new(|d| d.device_desc.truncate(12)),
            Error::Descriptor(DescError::TooShort),
        ),
        (
            "wrong type",
            Box::new(|d| d.device_desc[1] = 2),
            Error::Descriptor(DescError::WrongType),
        ),
    ];
    for (name, edit, want) in cases {
        let (mut hw, mut c) = running(ModelConfig::default(), config());
        let mut dev = UsbDevice::keyboard();
        edit(&mut dev);
        hw.connect(1, dev);
        let baseline = {
            c.reset_port(&mut hw, 1).unwrap();
            hw.allocated()
        };
        let e = c.enumerate(&mut hw, 1).expect_err(name);
        assert_eq!(e, want, "{name}");
        assert_eq!(hw.allocated(), baseline, "{name}: memory freed");
        assert!(
            (1..=16).all(|s| !hw.slot_enabled(s)),
            "{name}: slot disabled"
        );
        hw.assert_clean();
    }
    // Configuration whose wTotalLength exceeds what the device returns.
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    let mut dev = UsbDevice::keyboard();
    dev.config_desc[2] = 200;
    hw.connect(1, dev);
    let d = c.attach(&mut hw, 1).unwrap();
    let mut buf = [0u8; 256];
    assert!(c.read_configuration(&mut hw, d.slot, 0, &mut buf).is_err());
    let mut small = [0u8; 8];
    assert_eq!(
        c.read_configuration(&mut hw, d.slot, 0, &mut small),
        Err(Error::BufferTooSmall)
    );
    hw.assert_clean();
}

#[test]
fn recover_reports_every_transfer_once_and_forgets_devices() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.connect(1, UsbDevice::keyboard());
    let (slot, dci) = attach_keyboard(&mut hw, &mut c, 1);
    let ids: Vec<TransferId> = (0..4)
        .map(|i| {
            c.submit_interrupt_in(&mut hw, slot, dci, USER_BASE + 64 * i, 8)
                .unwrap()
        })
        .collect();
    hw.trigger_hse();
    // The poll that sees HSE fails; the cancelled transfers are delivered
    // by the following polls, then the error repeats.
    let mut got = HashMap::new();
    let mut errors = 0;
    for _ in 0..100 {
        match c.poll(&mut hw) {
            Ok(Some(Notification::Transfer(t))) => assert!(got.insert(t.id, t.result).is_none()),
            Ok(_) => {}
            Err(e) => {
                assert_eq!(e, Error::HostSystemError);
                errors += 1;
                if got.len() == ids.len() {
                    break;
                }
            }
        }
    }
    assert!(errors >= 2, "error before and after the completions");
    assert_eq!(got.len(), ids.len(), "{got:?}");
    assert!(
        got.values().all(|r| *r == Err(Error::HostSystemError)),
        "{got:?}"
    );
    c.recover(&mut hw).unwrap();
    assert!(c.device_info(slot).is_none());
    assert_eq!(c.pending_transfers(), 0);
    // After recovery the still-connected device can be enumerated again.
    let (slot, dci) = attach_keyboard(&mut hw, &mut c, 1);
    let id = c
        .submit_interrupt_in(&mut hw, slot, dci, USER_BASE, 8)
        .unwrap();
    hw.push_report(1, &[5; 8]);
    let (r, _) = wait_transfers(&mut hw, &mut c, 1);
    assert_eq!(r[&id].result, Ok(8));
    c.shutdown(&mut hw).unwrap();
    assert_eq!(hw.allocated(), 0, "DMA memory leaked");
    hw.assert_clean();
}

#[test]
fn request_validation() {
    let (mut hw, mut c) = running(ModelConfig::default(), config());
    hw.connect(1, UsbDevice::keyboard());
    let (slot, dci) = attach_keyboard(&mut hw, &mut c, 1);
    let doorbells = hw.stats.doorbells;
    assert!(c
        .submit_interrupt_in(&mut hw, slot, dci, USER_BASE, 0)
        .is_err());
    assert!(
        c.submit_interrupt_in(&mut hw, slot, dci, USER_BASE + 0xFFFC, 8)
            .is_err(),
        "64 KiB crossing"
    );
    assert_eq!(
        c.submit_interrupt_in(&mut hw, slot, 2, USER_BASE, 8),
        Err(Error::InvalidEndpoint)
    );
    assert_eq!(
        c.submit_interrupt_in(&mut hw, slot, 5, USER_BASE, 8),
        Err(Error::InvalidEndpoint)
    );
    assert_eq!(
        c.submit_interrupt_in(&mut hw, 9, dci, USER_BASE, 8),
        Err(Error::InvalidSlot)
    );
    let mut b = [0u8; 4];
    assert_eq!(
        c.control_in(&mut hw, slot, SetupPacket::get_descriptor(1, 0, 18), &mut b),
        Err(Error::BufferTooSmall)
    );
    assert!(c
        .control_out(&mut hw, slot, SetupPacket::get_descriptor(1, 0, 18), &[])
        .is_err());
    assert_eq!(
        hw.stats.doorbells, doorbells,
        "rejected requests reach no doorbell"
    );
    hw.assert_clean();
}

#[test]
fn randomized_traffic_with_stalls_and_hot_plug() {
    for seed in [1u64, 2, 3] {
        let (mut hw, mut c) = running(ModelConfig::default(), config());
        let mut rng = Rng(seed * 7919 + 1);
        let mut attached: [Option<(u8, u8)>; 2] = [None, None];
        let mut outstanding: HashMap<TransferId, (usize, Vec<u8>)> = HashMap::new();
        let mut reported = 0usize;
        let mut submitted = 0usize;
        for step in 0..1500 {
            let p = rng.below(2) as usize;
            let port = p as u8 + 1;
            match rng.below(100) {
                0..=2 => {
                    if attached[p].is_some() {
                        hw.disconnect(port);
                        attached[p] = None;
                    } else if hw.device(port).is_none() {
                        hw.connect(port, UsbDevice::keyboard());
                        attached[p] = Some(attach_keyboard(&mut hw, &mut c, port));
                    }
                }
                3..=5 => {
                    if let Some(d) = hw.device_mut(port) {
                        d.stall_interrupt = true;
                    }
                }
                6..=40 => {
                    if let Some((slot, dci)) = attached[p] {
                        let buf = USER_BASE + 0x100 * (step % 128) as u64;
                        match c.submit_interrupt_in(&mut hw, slot, dci, buf, 8) {
                            Ok(id) => {
                                submitted += 1;
                                outstanding.insert(id, (p, Vec::new()));
                            }
                            Err(
                                Error::EndpointHalted | Error::InvalidSlot | Error::Disconnected,
                            ) => {}
                            Err(e) => panic!("seed {seed} step {step}: {e:?}"),
                        }
                    }
                }
                41..=70 => {
                    let r = [rng.below(256) as u8; 8];
                    hw.push_report(port, &r);
                }
                _ => {}
            }
            while let Some(n) = c.poll(&mut hw).expect("poll") {
                if let Notification::Transfer(t) = n {
                    assert!(
                        outstanding.remove(&t.id).is_some(),
                        "seed {seed}: {:?} unknown or twice",
                        t.id
                    );
                    reported += 1;
                    assert!(
                        matches!(
                            t.result,
                            Ok(8) | Err(Error::Stall) | Err(Error::Disconnected)
                        ),
                        "seed {seed}: {:?}",
                        t.result
                    );
                }
            }
        }
        // Unplug everything: every outstanding transfer must be reported.
        hw.disconnect(1);
        hw.disconnect(2);
        for _ in 0..10_000 {
            match c.poll(&mut hw).expect("poll") {
                Some(Notification::Transfer(t)) => {
                    assert!(outstanding.remove(&t.id).is_some());
                    reported += 1;
                }
                Some(_) => {}
                None => {
                    if outstanding.is_empty() {
                        break;
                    }
                    let _ = hw_xhci::Clock::now_us(&mut hw);
                }
            }
        }
        assert!(
            outstanding.is_empty(),
            "seed {seed}: {} never reported",
            outstanding.len()
        );
        assert_eq!(reported, submitted);
        assert!(submitted > 100, "seed {seed}: only {submitted} transfers");
        c.shutdown(&mut hw).unwrap();
        assert_eq!(hw.allocated(), 0, "seed {seed}: leak");
        hw.assert_clean();
    }
}
