//! Every sequence of operations on a four-descriptor table against a model:
//! descriptors map to description ids; an object is freed exactly when its
//! last descriptor goes.

use linux_compat::errno::{EBADF, EMFILE};
use linux_compat::fdtable::{FdTable, Kind, Ofd};

fn ofd(obj: u64) -> Ofd {
    Ofd {
        obj,
        kind: Kind::File,
        flags: 0,
        offset: 0,
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Open,
    Close(usize),
    Dup(usize),
    DupTo(usize, usize),
}

fn ops() -> Vec<Op> {
    let mut v = vec![Op::Open];
    for i in 0..4 {
        v.push(Op::Close(i));
        v.push(Op::Dup(i));
        for j in 0..4 {
            if i != j {
                v.push(Op::DupTo(i, j));
            }
        }
    }
    v
}

fn live(m: &[Option<u32>; 4], id: u32) -> usize {
    m.iter().flatten().filter(|x| **x == id).count()
}

fn apply(t: &mut FdTable<4>, model: &mut [Option<u32>; 4], next: &mut u32, op: Op, ctx: &str) {
    match op {
        Op::Open => {
            let want = model.iter().position(|s| s.is_none());
            let got = t.insert(ofd(u64::from(*next)), false, 0);
            match want {
                Some(fd) => {
                    assert_eq!(got, Ok(fd), "{ctx}");
                    model[fd] = Some(*next);
                    *next += 1;
                }
                None => assert_eq!(got, Err(EMFILE), "{ctx}"),
            }
        }
        Op::Close(fd) => {
            let got = t.close(fd);
            match model[fd] {
                None => assert_eq!(got, Err(EBADF), "{ctx}"),
                Some(id) => {
                    model[fd] = None;
                    let freed = (live(model, id) == 0).then_some(u64::from(id));
                    assert_eq!(got, Ok(freed), "{ctx}");
                }
            }
        }
        Op::Dup(fd) => {
            let got = t.dup(fd, 0, false);
            match (model[fd], model.iter().position(|s| s.is_none())) {
                (None, _) => assert_eq!(got, Err(EBADF), "{ctx}"),
                (Some(_), None) => assert_eq!(got, Err(EMFILE), "{ctx}"),
                (Some(id), Some(new)) => {
                    assert_eq!(got, Ok(new), "{ctx}");
                    model[new] = Some(id);
                }
            }
        }
        Op::DupTo(a, b) => {
            let got = t.dup_to(a, b, false);
            match model[a] {
                None => assert_eq!(got, Err(EBADF), "{ctx}"),
                Some(id) => {
                    let old = model[b];
                    model[b] = Some(id);
                    let freed = old.filter(|o| live(model, *o) == 0).map(u64::from);
                    assert_eq!(got, Ok(freed), "{ctx}");
                }
            }
        }
    }
}

#[test]
fn every_short_sequence_matches_the_model() {
    let all = ops();
    let depth = 4;
    let mut idx = vec![0usize; depth];
    let mut sequences = 0u64;
    loop {
        let mut t = FdTable::<4>::new();
        let mut model: [Option<u32>; 4] = [None; 4];
        let mut next = 100u32;
        for (step, &i) in idx.iter().enumerate() {
            let ctx = format!("{idx:?} step {step} {:?}", all[i]);
            apply(&mut t, &mut model, &mut next, all[i], &ctx);
            t.check().unwrap();
            for (fd, m) in model.iter().enumerate() {
                assert_eq!(
                    t.get(fd).ok().map(|o| o.obj),
                    m.map(u64::from),
                    "{ctx} fd {fd}"
                );
            }
        }
        sequences += 1;
        let mut k = depth;
        loop {
            if k == 0 {
                assert_eq!(sequences, (all.len() as u64).pow(depth as u32));
                return;
            }
            k -= 1;
            idx[k] += 1;
            if idx[k] < all.len() {
                break;
            }
            idx[k] = 0;
        }
    }
}
