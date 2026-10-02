//! The table of measured regions against the measurement and against the devices.

use vmm_devices::lapic;
use vmm_devices::legacy::Legacy;
use vmm_devices::map::*;
use vmm_devices::pic::Pic;
use vmm_devices::pit::Pit2;
use vmm_devices::rtc::Rtc;
use vmm_devices::uart::Uart;

const REPORT: &str = include_str!("../../../docs/research/linux-guest-surface.md");

/// (name, lowest, highest) of the rows of the first table of the report.
fn measured_rows() -> Vec<(String, u64, u64)> {
    let hex = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).unwrap();
    REPORT
        .lines()
        .skip_while(|l| !l.starts_with("### Device regions touched"))
        .skip(1)
        .skip_while(|l| !l.starts_with('|'))
        .skip(2)
        .take_while(|l| l.starts_with('|'))
        .map(|l| {
            let c: Vec<&str> = l.trim_matches('|').split('|').map(str::trim).collect();
            (c[0].to_string(), hex(c[4]), hex(c[5]))
        })
        .collect()
}

#[test]
fn the_table_is_the_measurement() {
    let rows = measured_rows();
    assert_eq!(rows.len(), 37, "the report lists 37 regions");
    let mut want: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let mut have: Vec<&str> = MEASURED.iter().map(|r| r.name).collect();
    want.sort_unstable();
    have.sort_unstable();
    assert_eq!(have, want, "same regions, none added, none missing");
    for (name, lo, hi) in rows {
        let r = MEASURED.iter().find(|r| r.name == name).unwrap();
        assert_eq!(
            r.ranges.iter().map(|x| x.0).min(),
            Some(lo),
            "{name}: lowest"
        );
        assert_eq!(
            r.ranges.iter().map(|x| x.1).max(),
            Some(hi),
            "{name}: highest"
        );
        assert!(r.ranges.iter().all(|(a, b)| a <= b), "{name}");
    }
}

fn claimed(module: &str, port: u64) -> bool {
    let p = port as u16;
    match module {
        "uart" => Uart::owns(p),
        "rtc" => Rtc::owns(p),
        "pic" => Pic::owns(p),
        "legacy" => Legacy::owns(p),
        "pit" => Pit2::new().read(p, 0).is_some(),
        m => panic!("unknown module {m}"),
    }
}

#[test]
fn every_done_region_is_claimed_by_its_module() {
    for r in MEASURED {
        let Status::Done(module) = r.status else {
            continue;
        };
        for &(lo, hi) in r.ranges {
            if module == "lapic" {
                assert!(
                    lo >= lapic::DEFAULT_BASE && hi < lapic::DEFAULT_BASE + 0x1000,
                    "{}",
                    r.name
                );
                continue;
            }
            for port in [lo, hi] {
                assert!(
                    claimed(module, port),
                    "{}: port {port:#x} not claimed by {module}",
                    r.name
                );
            }
        }
    }
}

#[test]
fn the_partial_region_says_what_is_missing_and_is_really_missing() {
    let partial: Vec<_> = MEASURED
        .iter()
        .filter(|r| matches!(r.status, Status::Partial(..)))
        .collect();
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].name, "pit");
    assert!(claimed("pit", 0x42) && claimed("pit", 0x43) && claimed("pit", 0x61));
    assert!(!claimed("pit", 0x40), "channel 0 is not modeled");
}

#[test]
fn pending_regions_name_a_planned_step() {
    let steps = ["display", "pci", "hpet", "ioapic", "acpi-pm", "i8042"];
    for r in MEASURED {
        if let Status::Pending(step) = r.status {
            assert!(steps.contains(&step), "{}: {step}", r.name);
        }
    }
    // and nothing claims to be done while a device for it does not exist
    for r in MEASURED {
        if let Status::Pending(_) = r.status {
            for &(lo, _) in r.ranges {
                for m in ["uart", "rtc", "pic", "legacy"] {
                    assert!(
                        !claimed(m, lo) || lo >= 0x1_0000,
                        "{} port {lo:#x} is already claimed by {m}",
                        r.name
                    );
                }
            }
        }
    }
}

#[test]
fn the_tally() {
    assert_eq!(tally(), (13, 1, 14, 9));
    let (a, b, c, d) = tally();
    assert_eq!(a + b + c + d, MEASURED.len());
}
