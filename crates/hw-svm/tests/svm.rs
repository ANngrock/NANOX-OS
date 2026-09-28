//! hw-svm against the scripted processor: capability decoding, VMRUN
//! consistency checks, permission maps, nested paging (including the stale
//! TLB negative control) and the VMM loop with every verdict.

mod common;

use common::*;
use hw_svm::caps::CpuidMsr;
use hw_svm::perm::{IoPermissionMap, MsrPermissionMap, IOPM_BYTES, MSRPM_BYTES};
use hw_svm::vmcb::{attr, bits, ctl, misc2, save, StateError};
use hw_svm::vmm::{Vcpu, Verdict, VENDOR};
use hw_svm::{Error, FrameAlloc, Npt, NptPerms, PhysMem, SvmCaps, SvmUnavailable, PAGE_SIZE};

// ---- capabilities -----------------------------------------------------------

fn zen3() -> CpuidMsr {
    CpuidMsr {
        max_ext_leaf: 0x8000_0021,
        ext_ecx: 1 << 2,
        svm_eax: 1,
        svm_ebx: 0x8000,
        svm_edx: 0x101B_BCFF,
        vm_cr: 0,
    }
}

#[test]
fn capabilities() {
    let c = SvmCaps::decode(zen3()).unwrap();
    assert!(c.nested_paging && c.nrips && c.flush_by_asid && c.decode_assists);
    assert_eq!(c.asids, 0x8000);
    let cases = [
        (
            CpuidMsr {
                ext_ecx: 0,
                ..zen3()
            },
            SvmUnavailable::NotSupported,
        ),
        (
            CpuidMsr {
                max_ext_leaf: 0x8000_0008,
                ..zen3()
            },
            SvmUnavailable::NoSvmLeaf,
        ),
        (
            CpuidMsr {
                vm_cr: 1 << 4,
                ..zen3()
            },
            SvmUnavailable::DisabledByFirmware { locked: false },
        ),
        (
            CpuidMsr {
                vm_cr: 1 << 4 | 1 << 3,
                ..zen3()
            },
            SvmUnavailable::DisabledByFirmware { locked: true },
        ),
        (
            CpuidMsr {
                svm_edx: 0x101B_BCFE,
                ..zen3()
            },
            SvmUnavailable::NoNestedPaging,
        ),
        (
            CpuidMsr {
                svm_ebx: 1,
                ..zen3()
            },
            SvmUnavailable::NoAsids,
        ),
    ];
    for (raw, want) in cases {
        assert_eq!(SvmCaps::decode(raw), Err(want));
    }
}

// ---- VMCB checks -----------------------------------------------------------

#[test]
fn prepared_long_mode_guest_passes_the_checks() {
    let mut rig = Rig::new(&[]);
    let mut serial = [0u8; 16];
    let v = Vcpu::new(rig.cfg, &mut serial);
    let mut vmcb = rig.vmcb();
    v.prepare(&mut vmcb);
    assert_eq!(vmcb.check(), Ok(()));
    assert_eq!(vmcb.segment(save::CS).attrib & attr::L, attr::L);
}

#[test]
fn every_consistency_check_fires() {
    type Edit = fn(&mut hw_svm::Vmcb<'_>);
    let cases: [(Edit, StateError); 17] = [
        (
            |v| v.write_u64(save::EFER, v.read_u64(save::EFER) & !bits::EFER_SVME),
            StateError::EferSvmeClear,
        ),
        (
            |v| v.write_u64(save::EFER, v.read_u64(save::EFER) | 1 << 20),
            StateError::EferReserved,
        ),
        (
            |v| v.write_u64(save::CR0, v.read_u64(save::CR0) | bits::CR0_NW),
            StateError::Cr0CdNw,
        ),
        (
            |v| v.write_u64(save::CR0, v.read_u64(save::CR0) | 1 << 40),
            StateError::Cr0High,
        ),
        (|v| v.write_u64(save::CR3, 1 << 60), StateError::Cr3Reserved),
        (
            |v| v.write_u64(save::CR4, v.read_u64(save::CR4) | 1 << 40),
            StateError::Cr4Reserved,
        ),
        (|v| v.write_u64(save::DR6, 1 << 40), StateError::Dr6High),
        (|v| v.write_u64(save::DR7, 1 << 40), StateError::Dr7High),
        (
            |v| v.write_u64(save::CR4, v.read_u64(save::CR4) & !bits::CR4_PAE),
            StateError::LongModeWithoutPae,
        ),
        (
            |v| v.write_u64(save::CR0, v.read_u64(save::CR0) & !bits::CR0_PE),
            StateError::LongModeWithoutPe,
        ),
        (
            |v| {
                let mut cs = v.segment(save::CS);
                cs.attrib |= attr::DB;
                v.set_segment(save::CS, cs);
            },
            StateError::CsLongAndDefault32,
        ),
        (
            |v| {
                v.write_u32(
                    ctl::INTERCEPT_MISC2,
                    v.read_u32(ctl::INTERCEPT_MISC2) & !misc2::VMRUN,
                )
            },
            StateError::VmrunNotIntercepted,
        ),
        (|v| v.write_u32(ctl::ASID, 0), StateError::AsidZero),
        (
            |v| v.write_u64(ctl::MSRPM_BASE, 0x1234),
            StateError::PermissionMapAddress,
        ),
        (
            |v| v.set_event_inj(1 << 31 | 5 << 8 | 13),
            StateError::EventInjection,
        ),
        (
            |v| v.write_u64(ctl::NP_ENABLE, 0),
            StateError::NestedPagingDisabled,
        ),
        (|v| v.write_u64(ctl::N_CR3, 0x1001), StateError::NestedCr3),
    ];
    for (edit, want) in cases {
        let mut rig = Rig::new(&[Step::Hlt]);
        let mut serial = [0u8; 16];
        let mut v = Vcpu::new(rig.cfg, &mut serial);
        {
            let mut vmcb = rig.vmcb();
            v.prepare(&mut vmcb);
            edit(&mut vmcb);
            assert_eq!(vmcb.check(), Err(Error::InvalidState(want)));
        }
        // The VMM refuses to enter: no VMRUN happens at all.
        let mut vmcb = hw_svm::Vmcb::wrap(&mut rig.page);
        let out = v.run(&mut rig.cpu, &mut rig.clock, &mut vmcb, &mut rig.gprs);
        assert_eq!(out.verdict, Verdict::Invalid(want));
        assert_eq!(rig.cpu.vmruns, 0);
    }
}

// ---- permission maps --------------------------------------------------------

#[test]
fn permission_map_layout() {
    assert_eq!(MsrPermissionMap::bit(0), Some(0));
    assert_eq!(MsrPermissionMap::bit(0x1FFF), Some(2 * 0x1FFF));
    assert_eq!(MsrPermissionMap::bit(0xC000_0080), Some(0x800 * 8 + 0x100));
    assert_eq!(MsrPermissionMap::bit(0xC001_0114), Some(0x1000 * 8 + 0x228));
    assert_eq!(MsrPermissionMap::bit(0x4000_0000), None);
    let mut m = [0u8; MSRPM_BYTES];
    let mut map = MsrPermissionMap::intercept_all(&mut m);
    assert!(map.allow(0xC000_0100, true, false));
    assert!(!map.intercepts(0xC000_0100, false));
    assert!(map.intercepts(0xC000_0100, true));
    assert!(!map.allow(0x4000_0000, true, true), "outside the ranges");
    assert!(map.intercepts(0x4000_0000, false));

    let mut io = [0u8; IOPM_BYTES];
    let mut iomap = IoPermissionMap::intercept_all(&mut io);
    iomap.allow(0x60);
    iomap.allow(0x61);
    assert!(!iomap.intercepts(0x60, 2));
    assert!(
        iomap.intercepts(0x61, 2),
        "a wide access touching an intercepted port"
    );
    assert!(
        iomap.intercepts(0xFFFF, 4),
        "accesses past FFFFh use the spill bits"
    );
}

// ---- nested paging ------------------------------------------------------------

#[test]
fn nested_mapping_rules() {
    let mut mem = Mem::new();
    let mut fr = Frames::new();
    let mut npt = Npt::new(&mut mem, &mut fr, 48).unwrap();
    let host = 0x4000_0000;
    // A range crossing 2 MiB and 1 GiB boundaries.
    let gpa = (1 << 30) - 3 * PAGE_SIZE;
    npt.map(&mut mem, &mut fr, gpa, host, 6 * PAGE_SIZE, NptPerms::RW)
        .unwrap();
    for i in 0..6 {
        let (h, p) = npt
            .translate(&mut mem, gpa + i * PAGE_SIZE + 5)
            .unwrap()
            .unwrap();
        assert_eq!(h, host + i * PAGE_SIZE + 5);
        assert!(p.write && !p.exec);
    }
    // PML4 + PDPT + 2 PD + 2 PT.
    assert_eq!(npt.table_frames(), 6);
    let tables = fr.allocated();
    assert_eq!(
        npt.map(
            &mut mem,
            &mut fr,
            gpa + PAGE_SIZE,
            host,
            PAGE_SIZE,
            NptPerms::RO
        ),
        Err(Error::AlreadyMapped)
    );
    for (g, h, l) in [
        (1, host, PAGE_SIZE),
        (0, 3, PAGE_SIZE),
        (0, host, 0),
        (1 << 48, host, PAGE_SIZE),
        (0, 1 << 52, PAGE_SIZE),
    ] {
        assert!(
            npt.map(&mut mem, &mut fr, g, h, l, NptPerms::RO).is_err(),
            "{g:#x} {h:#x} {l}"
        );
    }
    assert_eq!(fr.allocated(), tables, "rejected maps allocate nothing");
    assert_eq!(npt.unmap(&mut mem, 0, PAGE_SIZE), Err(Error::NotMapped));
    let t = npt.unmap(&mut mem, gpa, 6 * PAGE_SIZE).unwrap();
    assert_eq!(t.pages(), 6);
    assert_eq!(npt.translate(&mut mem, gpa).unwrap(), None);
    npt.destroy(&mut mem, &mut fr);
    assert_eq!(fr.allocated(), 0, "every table frame freed");
    assert!(fr.violations.is_empty() && mem.violations.is_empty());
}

#[test]
fn out_of_frames_has_no_partial_effect() {
    for budget in 0..4 {
        let mut mem = Mem::new();
        let mut fr = Frames::new();
        let mut npt = Npt::new(&mut mem, &mut fr, 48).unwrap();
        let root = npt.root();
        let mut before = [0u8; 4096];
        mem.read_bytes(root, &mut before);
        fr.fail_after = Some(budget);
        // Needs PDPT, PD and PT: 3 frames.
        let r = npt.map(
            &mut mem,
            &mut fr,
            0x40_0000_0000,
            0x1000_0000,
            PAGE_SIZE,
            NptPerms::RW,
        );
        if budget < 3 {
            assert_eq!(r, Err(Error::OutOfFrames));
            let mut after = [0u8; 4096];
            mem.read_bytes(root, &mut after);
            assert_eq!(before, after, "root untouched");
            assert_eq!(fr.allocated(), 1, "budget {budget}: frames returned");
            assert_eq!(npt.mapped_pages(), 0);
        } else {
            r.unwrap();
            assert_eq!(fr.allocated(), 4);
        }
        assert!(fr.violations.is_empty());
    }
}

#[test]
fn corrupt_entries_are_detected() {
    let mut mem = Mem::new();
    let mut fr = Frames::new();
    let mut npt = Npt::new(&mut mem, &mut fr, 48).unwrap();
    npt.map(&mut mem, &mut fr, 0, 0x1000_0000, PAGE_SIZE, NptPerms::RW)
        .unwrap();
    // A large-page bit where this crate never writes one.
    let pml4e = mem.read_u64(npt.root());
    mem.write_u64(npt.root(), pml4e | 1 << 7);
    assert_eq!(npt.translate(&mut mem, 0), Err(Error::Corrupt));
}

// ---- the VMM --------------------------------------------------------------

fn serial_out(s: &str) -> Vec<Step> {
    s.bytes()
        .map(|b| Step::Out {
            port: 0x3F8,
            size: 1,
            value: u32::from(b),
        })
        .collect()
}

#[test]
fn candidate_passes_like_under_qemu() {
    let mut script = vec![Step::In {
        port: 0x3FD,
        size: 1,
        expect: 0x60,
    }];
    script.extend(serial_out("NANOX PASS\n"));
    script.push(Step::Out {
        port: 0xF4,
        size: 4,
        value: 0x10,
    });
    let mut rig = Rig::new(&script);
    let mut serial = [0u8; 64];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(
        out.verdict,
        Verdict::DebugExit {
            value: 0x10,
            status: 33
        },
        "M0 PASS status"
    );
    assert_eq!(v.serial(), b"NANOX PASS\n");
    assert_eq!(rig.cpu.tlb_controls[0], 1, "first entry flushes the TLB");
    assert!(rig.cpu.tlb_controls[1..].iter().all(|&t| t == 0));
    rig.cpu.assert_clean();
}

#[test]
fn fail_status_matches_the_harness() {
    let mut rig = Rig::new(&[Step::Out {
        port: 0xF4,
        size: 1,
        value: 0x11,
    }]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(
        run(&mut rig, &mut v).verdict,
        Verdict::DebugExit {
            value: 0x11,
            status: 35
        }
    );
    rig.cpu.assert_clean();
}

#[test]
fn port_reads_and_serial_registers() {
    let mut rig = Rig::new(&[
        Step::In {
            port: 0x3F9,
            size: 1,
            expect: 0,
        },
        Step::In {
            port: 0x60,
            size: 1,
            expect: 0xFF,
        },
        Step::In {
            port: 0x1234,
            size: 2,
            expect: 0xFFFF,
        },
        Step::In {
            port: 0xCFC,
            size: 4,
            expect: 0xFFFF_FFFF,
        },
        Step::Out {
            port: 0x80,
            size: 1,
            value: 0x42,
        },
        Step::Hlt,
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(out.verdict, Verdict::Halted);
    assert!(v.serial().is_empty());
    rig.cpu.assert_clean();
}

#[test]
fn cpuid_policy() {
    let mut rig = Rig::new(&[
        Step::Cpuid { leaf: 1, sub: 0 },
        Step::Cpuid {
            leaf: 0x4000_0000,
            sub: 0,
        },
        Step::Cpuid {
            leaf: 0x8000_0001,
            sub: 0,
        },
        Step::Cpuid {
            leaf: 0x8000_000A,
            sub: 0,
        },
        Step::Cpuid { leaf: 0x11, sub: 0 },
        Step::Cpuid { leaf: 0, sub: 0 },
        Step::Hlt,
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    run(&mut rig, &mut v);
    let r = &rig.cpu.cpuid_results;
    assert_eq!(r.len(), 6);
    assert_ne!(r[0][2] & 1 << 31, 0, "hypervisor present");
    assert_eq!(
        r[0][2] & (1 << 5 | 1 << 21 | 1 << 24 | 1 << 3),
        0,
        "VMX, x2APIC, TSC deadline, MONITOR hidden"
    );
    let vendor: Vec<u8> = r[1][1..].iter().flat_map(|w| w.to_le_bytes()).collect();
    assert_eq!(&vendor[..], VENDOR);
    assert_eq!(r[2][2] & 1 << 2, 0, "no nested SVM");
    assert_eq!(r[3], [0; 4]);
    assert_eq!(r[4], [0; 4], "leaf above the host maximum");
    assert_eq!(r[5][0], 0x10, "basic leaves unchanged");
    rig.cpu.assert_clean();
}

#[test]
fn msr_policy_passes_state_msrs_and_faults_the_rest() {
    let mut rig = Rig::new(&[
        Step::Wrmsr {
            msr: 0xC000_0100,
            value: 0xFFFF_8000_1234_0000,
        },
        Step::Rdmsr { msr: 0xC000_0100 },
        Step::Rdmsr { msr: 0x1B }, // APIC base: not virtualized
        Step::Wrmsr {
            msr: 0xC001_0114,
            value: 0,
        }, // VM_CR
        Step::Rdmsr { msr: 0x4000_0000 }, // outside the map
        Step::Out {
            port: 0xF4,
            size: 1,
            value: 0x10,
        },
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(
        out.verdict,
        Verdict::DebugExit {
            value: 0x10,
            status: 33
        }
    );
    assert_eq!(out.msr_faults, 3);
    assert_eq!(
        rig.cpu.rdmsr_results,
        vec![0xFFFF_8000_1234_0000],
        "FS.base did not exit"
    );
    assert_eq!(rig.cpu.injected.len(), 3);
    assert!(rig
        .cpu
        .injected
        .iter()
        .all(|&e| e & 0xFF == 13 && (e >> 8) & 7 == 3 && e & 1 << 11 != 0));
    rig.cpu.assert_clean();
}

#[test]
fn svm_instructions_from_the_guest_get_ud() {
    let mut rig = Rig::new(&[Step::Vmmcall, Step::Hlt]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(out.verdict, Verdict::Halted);
    assert_eq!(out.ud_injected, 1);
    assert_eq!(rig.cpu.injected[0] & 0xFF, 6);
    assert_eq!(rig.cpu.injected[0] & 1 << 11, 0, "#UD has no error code");
    rig.cpu.assert_clean();
}

#[test]
fn guest_memory_and_nested_faults() {
    let mut rig = Rig::new(&[
        Step::Load {
            gpa: 3 * PAGE_SIZE + 7,
        },
        Step::Store {
            gpa: 5 * PAGE_SIZE,
            byte: 0x77,
        },
        Step::Load { gpa: 5 * PAGE_SIZE },
        Step::Load {
            gpa: RAM_PAGES * PAGE_SIZE,
        },
    ]);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(rig.cpu.loads, vec![3, 0x77]);
    match out.verdict {
        Verdict::NestedPageFault { gpa, error } => {
            assert_eq!(gpa, RAM_PAGES * PAGE_SIZE);
            assert_eq!(error & 1, 0, "not present");
            assert_ne!(error & 1 << 32, 0);
        }
        v => panic!("{v:?}"),
    }
    let mut b = [0u8];
    rig.cpu.mem.read_bytes(rig.ram[5], &mut b);
    assert_eq!(b[0], 0x77, "store reached host memory");
    rig.cpu.assert_clean();
}

#[test]
fn read_only_mapping_faults_on_write() {
    let mut rig = Rig::new(&[Step::Store {
        gpa: 0x40_0000,
        byte: 1,
    }]);
    let f = rig.cpu.frames.alloc_frame().unwrap();
    rig.npt
        .map(
            &mut rig.cpu.mem,
            &mut rig.cpu.frames,
            0x40_0000,
            f,
            PAGE_SIZE,
            NptPerms::RO,
        )
        .unwrap();
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    match run(&mut rig, &mut v).verdict {
        Verdict::NestedPageFault { gpa, error } => {
            assert_eq!(gpa, 0x40_0000);
            assert_eq!(error & 3, 3, "present, write");
        }
        v => panic!("{v:?}"),
    }
    rig.cpu.assert_clean();
}

/// Unmapping must flush the guest TLB before the next entry; without the
/// flush the guest keeps using the old translation (negative control).
#[test]
fn unmap_flushes_the_guest_tlb() {
    for flush in [true, false] {
        let mut rig = Rig::new(&[
            Step::Load { gpa: 2 * PAGE_SIZE },
            Step::Out {
                port: 0xF4,
                size: 1,
                value: 0x10,
            },
            Step::Load { gpa: 2 * PAGE_SIZE },
            Step::Out {
                port: 0xF4,
                size: 1,
                value: 0x10,
            },
        ]);
        let mut serial = [0u8; 4];
        let mut v = Vcpu::new(rig.cfg, &mut serial);
        assert!(matches!(
            run(&mut rig, &mut v).verdict,
            Verdict::DebugExit { .. }
        ));
        let token = rig
            .npt
            .unmap(&mut rig.cpu.mem, 2 * PAGE_SIZE, PAGE_SIZE)
            .unwrap();
        if flush {
            v.note_unmap(token);
        } else {
            let _ = token;
        }
        let out = run(&mut rig, &mut v);
        if flush {
            assert!(
                matches!(out.verdict, Verdict::NestedPageFault { gpa, .. } if gpa == 2 * PAGE_SIZE),
                "{out:?}"
            );
            assert_eq!(rig.cpu.loads, vec![2]);
        } else {
            assert!(matches!(out.verdict, Verdict::DebugExit { .. }));
            assert_eq!(rig.cpu.loads, vec![2, 2], "stale translation used");
        }
        rig.cpu.assert_clean();
    }
}

#[test]
fn shutdown_halt_and_unsupported_io() {
    for (script, want) in [
        (vec![Step::TripleFault], Verdict::Shutdown),
        (vec![Step::Hlt], Verdict::Halted),
        (
            vec![Step::OutString { port: 0x3F8 }],
            Verdict::UnsupportedIo { port: 0x3F8 },
        ),
    ] {
        let mut rig = Rig::new(&script);
        let mut serial = [0u8; 4];
        let mut v = Vcpu::new(rig.cfg, &mut serial);
        assert_eq!(run(&mut rig, &mut v).verdict, want);
        rig.cpu.assert_clean();
    }
}

#[test]
fn spinning_guest_is_stopped_by_budgets() {
    // Time budget: every exit costs clock ticks.
    let mut rig = Rig::new(&[Step::Tick, Step::Tick, Step::SpinForever]);
    rig.cfg.max_time_us = 1_000;
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(out.verdict, Verdict::Timeout);
    assert!(out.exits >= 3);
    rig.cpu.assert_clean();
    // Exit budget.
    let mut rig = Rig::new(&[Step::SpinForever]);
    rig.cfg.max_exits = 50;
    rig.cfg.max_time_us = u64::MAX;
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(out.verdict, Verdict::ExitBudget);
    assert_eq!(out.exits, 50);
}

#[test]
fn serial_overflow_is_reported_not_fatal() {
    let mut script = serial_out("0123456789");
    script.push(Step::Out {
        port: 0xF4,
        size: 1,
        value: 0x10,
    });
    let mut rig = Rig::new(&script);
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    let out = run(&mut rig, &mut v);
    assert_eq!(
        out.verdict,
        Verdict::DebugExit {
            value: 0x10,
            status: 33
        }
    );
    assert!(out.serial_truncated);
    assert_eq!(v.serial(), b"0123");
    rig.cpu.assert_clean();
}

#[test]
fn without_nrips_fixed_instruction_lengths_are_used() {
    let mut rig = Rig::new(&[Step::Cpuid { leaf: 0, sub: 0 }, Step::Hlt]);
    rig.cfg.nrips = false;
    let mut serial = [0u8; 4];
    let mut v = Vcpu::new(rig.cfg, &mut serial);
    assert_eq!(run(&mut rig, &mut v).verdict, Verdict::Halted);
    rig.cpu.assert_clean();
}
