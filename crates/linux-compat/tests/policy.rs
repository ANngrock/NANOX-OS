//! The system-call policy: complete, disjoint, and covering every call a
//! real toolchain build makes.

use linux_compat::errno::{EAFNOSUPPORT, ENOSYS, EPERM};
use linux_compat::policy::{disposition, refusal, Disp, DENIED, HANDLED, PROBED, STUBBED};
use linux_compat::table::*;

const MEASURED: &str = include_str!("../../../docs/research/native-surface/build-syscalls.tsv");

/// strace shows these, but a program never issues them: the kernel restarts
/// an interrupted call by itself.
const NOT_ISSUED: &[&str] = &["restart_syscall"];

fn number(name: &str) -> Option<u32> {
    TABLE.iter().find(|(_, n)| *n == name).map(|(nr, _)| *nr)
}

#[test]
fn the_table_is_the_kernel_table() {
    assert!(
        TABLE.windows(2).all(|w| w[0].0 < w[1].0),
        "ascending, no duplicates"
    );
    let mut names: Vec<&str> = TABLE.iter().map(|(_, n)| *n).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), TABLE.len(), "names are unique");
    assert_eq!((SYS_READ, SYS_WRITE, SYS_OPEN, SYS_MMAP), (0, 1, 2, 9));
    assert_eq!(
        (SYS_EXIT_GROUP, SYS_OPENAT, SYS_CLONE3, SYS_STATX),
        (231, 257, 435, 332)
    );
    assert!(TABLE.len() > 330 && MAX_NR >= 450);
}

#[test]
fn the_sets_are_disjoint_and_the_constants_are_real_system_calls() {
    for nr in HANDLED {
        assert!(
            !STUBBED.iter().any(|(n, _)| n == nr),
            "{nr} handled and stubbed"
        );
        assert!(!DENIED.contains(nr), "{nr} handled and denied");
        assert!(TABLE.iter().any(|(n, _)| n == nr));
    }
    for (nr, _) in STUBBED {
        assert!(!DENIED.contains(nr), "{nr} stubbed and denied");
        assert!(TABLE.iter().any(|(n, _)| n == nr));
    }
    let mut seen = Vec::new();
    for nr in HANDLED
        .iter()
        .chain(DENIED)
        .chain(STUBBED.iter().map(|(n, _)| n))
    {
        assert!(!seen.contains(nr), "{nr} listed twice");
        seen.push(*nr);
    }
}

#[test]
fn every_call_of_a_real_toolchain_build_is_handled_or_stubbed() {
    let mut checked = 0;
    for line in MEASURED
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let name = line.split('\t').next().unwrap();
        let nr = number(name).unwrap_or_else(|| panic!("{name} is not in the table"));
        if NOT_ISSUED.contains(&name) {
            continue;
        }
        assert!(
            matches!(disposition(nr), Disp::Handled | Disp::Stub(_)) || PROBED.contains(&nr),
            "{name} ({nr}) from a real build is {:?}",
            disposition(nr)
        );
        checked += 1;
    }
    assert!(checked >= 60, "{checked}");
}

#[test]
fn dangerous_calls_are_refused_and_unknown_ones_are_missing() {
    for nr in [
        SYS_PTRACE,
        SYS_MOUNT,
        SYS_REBOOT,
        SYS_SETUID,
        SYS_CHROOT,
        SYS_UNSHARE,
        SYS_BPF,
        SYS_INIT_MODULE,
        SYS_KEXEC_LOAD,
    ] {
        assert_eq!(disposition(nr), Disp::Deny(EPERM), "{nr}");
        assert_eq!(refusal(disposition(nr)), EPERM);
    }
    assert_eq!(
        disposition(SYS_SOCKET),
        Disp::Deny(EAFNOSUPPORT),
        "no network by default"
    );
    assert_eq!(disposition(SYS_EPOLL_CREATE1), Disp::Missing);
    assert_eq!(refusal(Disp::Missing), ENOSYS);
    assert_eq!(disposition(10_000), Disp::Missing);
    assert_eq!(disposition(u32::MAX), Disp::Missing);
}

#[test]
fn every_number_has_a_disposition_and_the_counts_are_recorded() {
    let (mut h, mut s, mut d, mut m) = (0, 0, 0, 0);
    for nr in 0..=MAX_NR + 100 {
        match disposition(nr) {
            Disp::Handled => h += 1,
            Disp::Stub(_) => s += 1,
            Disp::Deny(_) => d += 1,
            Disp::Missing => m += 1,
        }
    }
    assert_eq!(h, HANDLED.len());
    assert_eq!(s, STUBBED.len());
    assert_eq!(d, DENIED.len() + 1, "plus socket");
    assert_eq!(h + s + d + m, (MAX_NR + 101) as usize);
    assert!(m > 150, "most of the 375 calls stay unavailable: {m}");
}
