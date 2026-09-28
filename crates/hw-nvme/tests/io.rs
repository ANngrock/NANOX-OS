//! I/O against the model: data integrity through every PRP shape, request
//! validation, ring wrap, queue exhaustion, abort, reset and abandon with
//! commands in flight (each reported exactly once), controller fatal status
//! during I/O, protocol-violating completions, shutdown, and a randomized
//! run with injected faults checked against a reference disk.

mod common;

use std::collections::{HashMap, HashSet};

use common::*;
use hw_nvme::status::generic;
use hw_nvme::{
    Completion, CompletionError, Controller, Error, Outcome, Phase, PrpError, Request, ResetReason,
    Status, TimeoutPhase,
};

const LBA: u64 = 512;

fn pattern(seed: u64, len: usize) -> Vec<u8> {
    Rng(seed | 1).bytes(len)
}

fn disk_range(m: &Model, lba: u64, len: usize) -> Vec<u8> {
    let start = (lba * LBA) as usize;
    m.disk[start..start + len].to_vec()
}

/// Polls until `poll` fails and returns the error; panics if it never does.
fn poll_until_error(m: &mut Model, c: &mut Controller, sink: &mut Vec<Completion>) -> Error {
    for _ in 0..2_000_000 {
        if let Err(e) = c.poll(m, &mut |x| sink.push(x)) {
            return e;
        }
    }
    panic!("poll never failed");
}

fn assert_each_once(reports: &[Completion], tags: &[u64]) -> HashMap<u64, Outcome> {
    let mut seen = HashMap::new();
    for r in reports {
        assert!(tags.contains(&r.tag), "unexpected tag {}", r.tag);
        assert!(
            seen.insert(r.tag, r.outcome).is_none(),
            "tag {} twice",
            r.tag
        );
    }
    assert_eq!(seen.len(), tags.len(), "missing reports: {seen:?}");
    seen
}

#[test]
fn data_round_trips_through_every_prp_shape() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    let max = c.max_transfer_bytes();
    // (offset in first page, length): one page, page-crossing with PRP2 as a
    // page, PRP lists, and the largest transfer. MDTS 2 MiB is at most 513
    // data pages, whose 512 list entries fill exactly one list page; the
    // chained case is `prp_list_chains_into_a_second_page`.
    let shapes = [
        (0, 512),
        (0, 4096),
        (512, 4096),
        (0, 8192),
        (1024, 16 * 1024),
        (0, 1 << 20),
        (4, 512 * 1023),
        (0, max),
    ];
    for (i, &(offset, len)) in shapes.iter().enumerate() {
        let buf = Buf::contiguous(DATA, offset, len);
        let data = pattern(i as u64 + 1, len as usize);
        let lba = 7 * i as u64;
        assert_eq!(
            write_blocks(&mut m, &mut c, &buf, lba, &data),
            OK,
            "write {i}"
        );
        assert_eq!(disk_range(&m, lba, len as usize), data, "disk {i}");
        buf.fill(&mut m, &vec![0x5A; len as usize]);
        let (o, back) = read_blocks(&mut m, &mut c, &buf, lba);
        assert_eq!(o, OK, "read {i}");
        assert_eq!(back, data, "read-back {i}");
    }
    assert_eq!(m.stats.max_prp_list_pages, 1, "{:?}", m.stats);
    assert_eq!(m.stats.max_transfer, u64::from(max));
    m.assert_clean();
}

#[test]
fn prp_list_chains_into_a_second_page() {
    // MDTS 4 MiB: a 3 MiB transfer has 768 data pages, i.e. 767 list
    // entries, more than one list page holds.
    let mcfg = ModelConfig {
        mdts: 10,
        ..ModelConfig::default()
    };
    let (mut m, mut c) = ready(mcfg, config());
    let len = 3 << 20;
    assert!(c.max_transfer_bytes() >= len, "{}", c.max_transfer_bytes());
    let buf = Buf::contiguous(DATA, 0, len);
    let data = pattern(11, len as usize);
    assert_eq!(write_blocks(&mut m, &mut c, &buf, 0, &data), OK);
    assert_eq!(disk_range(&m, 0, len as usize), data);
    assert_eq!(m.stats.max_prp_list_pages, 2, "{:?}", m.stats);
    buf.fill(&mut m, &vec![0; len as usize]);
    assert_eq!(read_blocks(&mut m, &mut c, &buf, 0).1, data);
    m.assert_clean();
}

#[test]
fn scattered_pages_keep_their_order() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    // Twelve pages in reverse physical order, with a gap between each.
    let buf = Buf {
        pages: (0..12u64).rev().map(|i| DATA + i * 2 * PAGE).collect(),
        offset: 256,
        len: 11 * 4096,
    };
    let data = pattern(99, buf.len as usize);
    assert_eq!(write_blocks(&mut m, &mut c, &buf, 100, &data), OK);
    assert_eq!(disk_range(&m, 100, data.len()), data);
    m.assert_clean();
}

#[test]
fn invalid_requests_are_rejected_before_submission() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    let doorbells = m.stats.doorbells;
    let max = c.max_transfer_bytes();
    let nsze = c.namespace().unwrap().nsze;
    let big = Buf::contiguous(DATA, 0, max + 512);
    let one = Buf::contiguous(DATA, 0, 512);
    let odd = Buf::contiguous(DATA, 2, 512);
    let cases: Vec<(Request<'_>, Error)> = vec![
        (
            Request::Write {
                lba: 0,
                blocks: (max + 512) / 512,
                buffer: big.data(),
            },
            Error::TransferTooLarge,
        ),
        (
            Request::Read {
                lba: 0,
                blocks: 0,
                buffer: one.data(),
            },
            Error::LbaRange,
        ),
        (
            Request::Read {
                lba: 0,
                blocks: 65537,
                buffer: one.data(),
            },
            Error::LbaRange,
        ),
        (
            Request::Read {
                lba: nsze,
                blocks: 1,
                buffer: one.data(),
            },
            Error::LbaRange,
        ),
        (
            Request::Read {
                lba: u64::MAX,
                blocks: 1,
                buffer: one.data(),
            },
            Error::LbaRange,
        ),
        (
            Request::Read {
                lba: 0,
                blocks: 2,
                buffer: one.data(),
            },
            Error::Buffer(PrpError::Length),
        ),
        (
            Request::Write {
                lba: 0,
                blocks: 1,
                buffer: odd.data(),
            },
            Error::Buffer(PrpError::OffsetAlignment),
        ),
    ];
    for (i, (req, want)) in cases.into_iter().enumerate() {
        assert_eq!(c.submit(&mut m, i as u64, req), Err(want), "case {i}");
    }
    assert_eq!(m.stats.doorbells, doorbells, "nothing may reach the device");
    assert_eq!(c.outstanding(), 0);
    let mut none = Vec::new();
    c.poll(&mut m, &mut |x| none.push(x)).unwrap();
    assert!(none.is_empty(), "rejected tags must never be reported");
    m.assert_clean();
}

#[test]
fn rings_wrap_many_times() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    let slots = config().io_slots();
    let mut rng = Rng(7);
    let mut reference = vec![0u8; m.disk.len()];
    let mut tag = 0u64;
    for round in 0..40 {
        let n = 1 + rng.below(slots as u64) as usize;
        let mut bufs = Vec::new();
        for k in 0..n {
            let lba = rng.below(16000);
            let blocks = 1 + rng.below(8) as u32;
            let buf = Buf::contiguous(DATA + k as u64 * 64 * 1024, 0, blocks * 512);
            let data = rng.bytes(buf.len as usize);
            buf.fill(&mut m, &data);
            let start = (lba * LBA) as usize;
            reference[start..start + data.len()].copy_from_slice(&data);
            bufs.push((tag, lba, blocks, buf));
            tag += 1;
        }
        // Distinct LBAs are not guaranteed; submit in order, the model
        // executes in order, so the last write wins as in `reference`.
        for (t, lba, blocks, buf) in &bufs {
            c.submit(
                &mut m,
                *t,
                Request::Write {
                    lba: *lba,
                    blocks: *blocks,
                    buffer: buf.data(),
                },
            )
            .unwrap_or_else(|e| panic!("round {round}: {e:?}"));
        }
        let tags: Vec<u64> = bufs.iter().map(|b| b.0).collect();
        for (t, o) in wait_all(&mut m, &mut c, &tags, 100_000) {
            assert_eq!(o, OK, "tag {t}");
        }
    }
    assert!(tag > 3 * 32, "the 32-entry rings must wrap several times");
    assert_eq!(m.disk, reference);
    m.assert_clean();
}

#[test]
fn queue_full_is_reported_without_side_effects() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::Delay(5_000_000)
        } else {
            Action::Normal
        }
    });
    let slots = config().io_slots();
    let tags: Vec<u64> = (0..slots as u64).collect();
    for &t in &tags {
        c.submit(&mut m, t, Request::Flush).unwrap();
    }
    let doorbells = m.stats.doorbells;
    assert_eq!(c.submit(&mut m, 999, Request::Flush), Err(Error::QueueFull));
    assert_eq!(m.stats.doorbells, doorbells);
    for (_, o) in wait_all(&mut m, &mut c, &tags, 1_000_000) {
        assert_eq!(o, OK);
    }
    m.assert_clean();
}

#[test]
fn error_status_is_reported_and_writes_nothing() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    let status = Status::new(0, generic::LBA_OUT_OF_RANGE, false, true);
    m.set_hook(move |i| {
        if i.qid == 1 {
            Action::Status(status)
        } else {
            Action::Normal
        }
    });
    let buf = Buf::contiguous(DATA, 0, 4096);
    let before = disk_range(&m, 40, 4096);
    assert_eq!(
        write_blocks(&mut m, &mut c, &buf, 40, &pattern(3, 4096)),
        Outcome::Error(status)
    );
    assert_eq!(disk_range(&m, 40, 4096), before);
    assert_eq!(c.phase(), Phase::Ready);
    m.assert_clean();
}

#[test]
fn hung_command_is_aborted_and_reported_as_timed_out() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 && i.seq % 2 == 0 {
            Action::Hang
        } else {
            Action::Normal
        }
    });
    let tags: Vec<u64> = (0..6).collect();
    for &t in &tags {
        c.submit(&mut m, t, Request::Flush).unwrap();
    }
    let got = wait_all(&mut m, &mut c, &tags, 2_000_000);
    let timed_out = got.values().filter(|o| **o == Outcome::TimedOut).count();
    let ok = got.values().filter(|o| **o == OK).count();
    assert_eq!(timed_out + ok, tags.len(), "{got:?}");
    assert!(timed_out >= 1, "{got:?}");
    assert_eq!(m.stats.aborts_hit, timed_out as u64);
    assert_eq!(c.phase(), Phase::Ready);
    m.clear_hook();
    assert_eq!(run(&mut m, &mut c, 100, Request::Flush), OK);
    m.assert_clean();
}

#[test]
fn lost_completion_forces_reset_and_every_command_is_reported_once() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    // The first I/O command completes normally; the others are executed but
    // lose their completions, which an Abort cannot recover, so the driver
    // must reset with them still outstanding.
    let first = std::cell::Cell::new(true);
    m.set_hook(move |i| {
        if i.qid != 1 || first.replace(false) {
            Action::Normal
        } else {
            Action::Drop
        }
    });
    let tags: Vec<u64> = (10..20).collect();
    for &t in &tags {
        c.submit(&mut m, t, Request::Flush).unwrap();
    }
    let mut reports = Vec::new();
    let e = poll_until_error(&mut m, &mut c, &mut reports);
    assert_eq!(e, Error::NeedsReset(ResetReason::CommandTimeout));
    assert_eq!(c.phase(), Phase::NeedsReset(ResetReason::CommandTimeout));
    assert_eq!(
        c.submit(&mut m, 99, Request::Flush),
        Err(Error::NeedsReset(ResetReason::CommandTimeout))
    );
    assert_eq!(
        reports.len(),
        1,
        "only the completed command before the reset"
    );
    m.clear_hook();
    c.reset(&mut m, &mut |x| reports.push(x)).expect("reset");
    let got = assert_each_once(&reports, &tags);
    assert_eq!(got[&10], OK);
    assert!(
        tags[1..].iter().all(|t| got[t] == Outcome::Reset),
        "{got:?}"
    );
    assert!(
        m.stats.aborts_miss > 0,
        "the abort attempt must have missed"
    );
    assert_eq!(c.outstanding(), 0);
    // The controller works again and nothing more is reported.
    let buf = Buf::contiguous(DATA, 0, 4096);
    let data = pattern(5, 4096);
    assert_eq!(write_blocks(&mut m, &mut c, &buf, 3, &data), OK);
    assert_eq!(read_blocks(&mut m, &mut c, &buf, 3).1, data);
    m.assert_clean();
}

#[test]
fn controller_fatal_during_io_is_recovered() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::Delay(1_000_000_000)
        } else {
            Action::Normal
        }
    });
    let tags: Vec<u64> = (0..5).collect();
    for &t in &tags {
        c.submit(&mut m, t, Request::Flush).unwrap();
    }
    m.trigger_fatal();
    let mut reports = Vec::new();
    assert_eq!(
        c.poll(&mut m, &mut |x| reports.push(x)),
        Err(Error::NeedsReset(ResetReason::ControllerFatal))
    );
    m.clear_hook();
    c.reset(&mut m, &mut |x| reports.push(x)).expect("reset");
    let got = assert_each_once(&reports, &tags);
    assert!(got.values().all(|o| *o == Outcome::Reset), "{got:?}");
    assert_eq!(run(&mut m, &mut c, 50, Request::Flush), OK);
    m.assert_clean();
}

#[test]
fn persistent_fatal_fails_the_reset_but_keeps_reports_exact() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::Delay(1_000_000_000)
        } else {
            Action::Normal
        }
    });
    for t in 0..3 {
        c.submit(&mut m, t, Request::Flush).unwrap();
    }
    m.faults.cfs_persistent = true;
    m.trigger_fatal();
    let mut reports = Vec::new();
    assert!(c.poll(&mut m, &mut |x| reports.push(x)).is_err());
    let e = c.reset(&mut m, &mut |x| reports.push(x)).unwrap_err();
    assert!(
        matches!(e, Error::ControllerFatal | Error::Timeout(_)),
        "{e:?}"
    );
    assert_eq!(c.phase(), Phase::Failed);
    // Disabling succeeded, so the commands were reported as reset once.
    let got = assert_each_once(&reports, &[0, 1, 2]);
    assert!(got.values().all(|o| *o == Outcome::Reset), "{got:?}");
    assert_eq!(c.outstanding(), 0);
    assert_eq!(c.submit(&mut m, 9, Request::Flush), Err(Error::NotReady));
}

#[test]
fn abandon_reports_everything_once() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::Hang
        } else {
            Action::Normal
        }
    });
    let tags: Vec<u64> = (0..4).collect();
    for &t in &tags {
        c.submit(&mut m, t, Request::Flush).unwrap();
    }
    let mut reports = Vec::new();
    c.abandon(&mut |x| reports.push(x));
    let got = assert_each_once(&reports, &tags);
    assert!(got.values().all(|o| *o == Outcome::Abandoned), "{got:?}");
    assert_eq!(c.phase(), Phase::Failed);
    let mut again = Vec::new();
    c.abandon(&mut |x| again.push(x));
    assert!(again.is_empty(), "nothing may be reported twice");
    m.clear_hook();
    c.reset(&mut m, &mut |x| again.push(x))
        .expect("reset after abandon");
    assert!(again.is_empty());
    assert_eq!(run(&mut m, &mut c, 7, Request::Flush), OK);
}

#[test]
fn protocol_violating_completions_are_detected() {
    // Unknown CID.
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::WrongCid(0x7777)
        } else {
            Action::Normal
        }
    });
    c.submit(&mut m, 1, Request::Flush).unwrap();
    let mut r = Vec::new();
    let e = poll_until_error(&mut m, &mut c, &mut r);
    assert_eq!(
        e,
        Error::InvalidCompletion(CompletionError::UnknownCid(0x7777))
    );
    assert!(r.is_empty());

    // Duplicate completion: reported once, the copy is rejected.
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::Duplicate
        } else {
            Action::Normal
        }
    });
    c.submit(&mut m, 2, Request::Flush).unwrap();
    let mut r = Vec::new();
    let e = poll_until_error(&mut m, &mut c, &mut r);
    assert!(
        matches!(
            e,
            Error::InvalidCompletion(
                CompletionError::DuplicateCid(_) | CompletionError::UnknownCid(_)
            )
        ),
        "{e:?}"
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].tag, 2);
    let mut more = Vec::new();
    for _ in 0..1000 {
        c.poll(&mut m, &mut |x| more.push(x)).unwrap();
    }
    assert!(more.is_empty());

    // SQ head outside the submitted range.
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::WrongSqHead(30)
        } else {
            Action::Normal
        }
    });
    c.submit(&mut m, 3, Request::Flush).unwrap();
    let mut r = Vec::new();
    let e = poll_until_error(&mut m, &mut c, &mut r);
    assert_eq!(e, Error::InvalidCompletion(CompletionError::SqHead(30)));
}

#[test]
fn shutdown_paths() {
    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.set_hook(|i| {
        if i.qid == 1 {
            Action::Delay(100_000_000)
        } else {
            Action::Normal
        }
    });
    c.submit(&mut m, 1, Request::Flush).unwrap();
    assert_eq!(c.shutdown(&mut m), Err(Error::Busy));
    wait_all(&mut m, &mut c, &[1], 1_000_000);
    m.clear_hook();
    c.shutdown(&mut m).expect("shutdown");
    assert_eq!(c.phase(), Phase::ShutDown);
    assert_eq!(m.csts() & (3 << 2), 2 << 2, "CSTS.SHST complete");
    assert_eq!(c.submit(&mut m, 2, Request::Flush), Err(Error::NotReady));
    m.assert_clean();

    let (mut m, mut c) = ready(ModelConfig::default(), config());
    m.faults.shutdown_never = true;
    assert_eq!(
        c.shutdown(&mut m),
        Err(Error::Timeout(TimeoutPhase::Shutdown))
    );
    assert_eq!(c.phase(), Phase::Failed);
    // The wait is bounded by shutdown_timeout_ns (100 ms) plus a little.
    assert!(m.now < 1_000_000_000, "{}", m.now);
}

/// Per-command fault plan derived from the command sequence number, so the
/// hook needs no shared state.
fn planned_action(seed: u64, info: &CmdInfo) -> Action {
    if info.qid != 1 {
        return Action::Normal;
    }
    let roll = Rng(seed ^ info.seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1).below(1000);
    match roll {
        0..=9 => Action::Hang,
        10..=14 => Action::Drop,
        15..=29 => Action::Status(Status::new(0, generic::INTERNAL_ERROR, false, false)),
        30..=32 => Action::Fatal,
        33..=199 => Action::Delay(roll * 100_000),
        _ => Action::Normal,
    }
}

#[test]
fn randomized_io_with_faults_matches_reference_disk() {
    for seed in [1u64, 2, 3] {
        let (mut m, mut c) = ready(ModelConfig::default(), config());
        m.set_hook(move |i| planned_action(seed, i));
        let slots = config().io_slots();
        let blocks_total = m.disk.len() / LBA as usize;
        let mut rng = Rng(seed * 31 + 7);
        let mut reference = m.disk.clone();
        let mut known = vec![true; blocks_total];
        // tag -> (is_write, lba, data or expected-length, buffer slot)
        let mut inflight: HashMap<u64, (bool, u64, Vec<u8>, usize)> = HashMap::new();
        let mut free_slots: Vec<usize> = (0..slots).collect();
        let mut reported: HashSet<u64> = HashSet::new();
        let (mut next_tag, mut resets, mut ok_reads) = (0u64, 0, 0);

        let settle = |t: u64,
                      o: Outcome,
                      m: &mut Model,
                      inflight: &mut HashMap<u64, (bool, u64, Vec<u8>, usize)>,
                      free: &mut Vec<usize>,
                      reference: &mut Vec<u8>,
                      known: &mut Vec<bool>,
                      reported: &mut HashSet<u64>,
                      ok_reads: &mut usize| {
            assert!(reported.insert(t), "tag {t} reported twice");
            let (is_write, lba, data, slot) = inflight.remove(&t).expect("unknown tag");
            free.push(slot);
            let blocks = data.len() / LBA as usize;
            let range = lba as usize..lba as usize + blocks;
            if is_write {
                if o == OK {
                    let s = (lba * LBA) as usize;
                    reference[s..s + data.len()].copy_from_slice(&data);
                    known[range].iter_mut().for_each(|k| *k = true);
                } else {
                    // The write may or may not have reached the media.
                    known[range].iter_mut().for_each(|k| *k = false);
                }
            } else if o == OK {
                let buf = Buf::contiguous(DATA + slot as u64 * 64 * 1024, 0, data.len() as u32);
                let got = buf.read_back(m);
                for b in 0..blocks {
                    if known[lba as usize + b] {
                        let s = ((lba as usize + b) * LBA as usize, b * LBA as usize);
                        assert_eq!(
                            &got[s.1..s.1 + LBA as usize],
                            &reference[s.0..s.0 + LBA as usize],
                            "seed {seed} tag {t} block {}",
                            lba as usize + b
                        );
                    }
                }
                *ok_reads += 1;
            }
        };

        for _ in 0..600 {
            // Submit a few requests while slots are free.
            for _ in 0..rng.below(4) {
                let Some(slot) = free_slots.pop() else { break };
                let blocks = 1 + rng.below(16) as u32;
                let lba = rng.below(blocks_total as u64 - u64::from(blocks));
                let is_write = rng.chance(500);
                let buf = Buf::contiguous(DATA + slot as u64 * 64 * 1024, 0, blocks * 512);
                let data = if is_write {
                    rng.bytes(buf.len as usize)
                } else {
                    vec![0; buf.len as usize]
                };
                if is_write {
                    buf.fill(&mut m, &data);
                }
                let req = if is_write {
                    Request::Write {
                        lba,
                        blocks,
                        buffer: buf.data(),
                    }
                } else {
                    Request::Read {
                        lba,
                        blocks,
                        buffer: buf.data(),
                    }
                };
                match c.submit(&mut m, next_tag, req) {
                    Ok(()) => {
                        inflight.insert(next_tag, (is_write, lba, data, slot));
                        next_tag += 1;
                    }
                    Err(Error::NeedsReset(_)) => free_slots.push(slot),
                    Err(e) => panic!("seed {seed}: submit {e:?}"),
                }
            }
            let mut batch = Vec::new();
            match c.poll(&mut m, &mut |x| batch.push(x)) {
                Ok(_) | Err(Error::InvalidCompletion(_)) => {}
                Err(Error::NeedsReset(_)) => {
                    resets += 1;
                    c.reset(&mut m, &mut |x| batch.push(x)).expect("reset");
                }
                Err(e) => panic!("seed {seed}: poll {e:?}"),
            }
            for x in batch {
                settle(
                    x.tag,
                    x.outcome,
                    &mut m,
                    &mut inflight,
                    &mut free_slots,
                    &mut reference,
                    &mut known,
                    &mut reported,
                    &mut ok_reads,
                );
            }
        }
        // Drain: stop injecting faults and wait for everything left.
        m.clear_hook();
        for _ in 0..3_000_000 {
            if inflight.is_empty() {
                break;
            }
            let mut batch = Vec::new();
            match c.poll(&mut m, &mut |x| batch.push(x)) {
                Ok(_) | Err(Error::InvalidCompletion(_)) => {}
                Err(Error::NeedsReset(_)) => {
                    resets += 1;
                    c.reset(&mut m, &mut |x| batch.push(x)).expect("reset");
                }
                Err(e) => panic!("seed {seed}: drain {e:?}"),
            }
            for x in batch {
                settle(
                    x.tag,
                    x.outcome,
                    &mut m,
                    &mut inflight,
                    &mut free_slots,
                    &mut reference,
                    &mut known,
                    &mut reported,
                    &mut ok_reads,
                );
            }
        }
        assert!(
            inflight.is_empty(),
            "seed {seed}: {} never reported",
            inflight.len()
        );
        assert_eq!(reported.len() as u64, next_tag, "seed {seed}");
        assert!(ok_reads > 50, "seed {seed}: only {ok_reads} verified reads");
        assert!(
            resets > 0 || m.stats.aborts_hit > 0,
            "seed {seed}: no recovery exercised"
        );
        // Every block the driver claims is known must match the media.
        for (b, k) in known.iter().enumerate() {
            if *k {
                let s = b * LBA as usize;
                assert_eq!(
                    m.disk[s..s + LBA as usize],
                    reference[s..s + LBA as usize],
                    "seed {seed} block {b}"
                );
            }
        }
        m.assert_clean();
    }
}
