//! The descriptor table against a model, and POSIX duplication semantics.

use linux_compat::errno::{EBADF, EMFILE};
use linux_compat::fdtable::{FdTable, Kind, Obj, Ofd};

fn ofd(obj: Obj) -> Ofd {
    Ofd {
        obj,
        kind: Kind::File,
        flags: 0,
        offset: 0,
    }
}

#[test]
fn the_lowest_free_number_is_always_used() {
    let mut t = FdTable::<8>::new();
    assert_eq!(t.insert(ofd(10), false, 0), Ok(0));
    assert_eq!(t.insert(ofd(11), false, 0), Ok(1));
    assert_eq!(t.insert(ofd(12), false, 0), Ok(2));
    assert_eq!(t.close(1), Ok(Some(11)));
    assert_eq!(t.insert(ofd(13), false, 0), Ok(1), "the hole is reused");
    assert_eq!(t.insert(ofd(14), false, 5), Ok(5), "a minimum is honoured");
    assert_eq!(t.insert(ofd(15), false, 5), Ok(6));
    t.check().unwrap();
}

#[test]
fn duplicates_share_the_offset_and_the_object_closes_with_the_last() {
    let mut t = FdTable::<8>::new();
    let a = t.insert(ofd(7), true, 0).unwrap();
    let b = t.dup(a, 0, false).unwrap();
    assert_ne!(a, b);
    t.get_mut(a).unwrap().offset = 123;
    assert_eq!(t.get(b).unwrap().offset, 123, "one open file, one offset");
    assert_eq!(t.cloexec(a), Ok(true));
    assert_eq!(t.cloexec(b), Ok(false), "dup clears close-on-exec");
    assert_eq!(t.close(a), Ok(None), "another descriptor still holds it");
    assert_eq!(
        t.close(b),
        Ok(Some(7)),
        "the last close releases the object"
    );
    assert_eq!(t.close(b), Err(EBADF));
    t.check().unwrap();
}

#[test]
fn dup_to_replaces_and_reports_what_it_freed() {
    let mut t = FdTable::<8>::new();
    let a = t.insert(ofd(1), false, 0).unwrap();
    let b = t.insert(ofd(2), false, 0).unwrap();
    assert_eq!(
        t.dup_to(a, b, false),
        Ok(Some(2)),
        "object 2 lost its only descriptor"
    );
    assert_eq!(t.get(b).unwrap().obj, 1);
    assert_eq!(
        t.dup_to(a, a, false),
        Ok(None),
        "dup2 onto itself does nothing"
    );
    assert_eq!(t.dup_to(a, 5, true), Ok(None));
    assert_eq!(t.cloexec(5), Ok(true));
    assert_eq!(t.dup_to(a, 99, false), Err(EBADF), "beyond the table");
    assert_eq!(t.dup_to(6, 3, false), Err(EBADF), "the source must be open");
    t.check().unwrap();
}

#[test]
fn close_on_exec_closes_exactly_the_marked_descriptors() {
    let mut t = FdTable::<8>::new();
    let a = t.insert(ofd(1), true, 0).unwrap();
    let _b = t.insert(ofd(2), false, 0).unwrap();
    let c = t.insert(ofd(3), true, 0).unwrap();
    let d = t.dup(c, 0, false).unwrap();
    let mut freed = Vec::new();
    t.close_on_exec(|o| freed.push(o));
    assert_eq!(
        freed,
        [1],
        "object 3 survives through its unmarked duplicate"
    );
    assert!(t.get(a).is_err() && t.get(c).is_err());
    assert_eq!(t.get(d).unwrap().obj, 3);
    t.check().unwrap();
}

#[test]
fn a_full_table_says_so() {
    let mut t = FdTable::<3>::new();
    for o in 0..3 {
        t.insert(ofd(o), false, 0).unwrap();
    }
    assert_eq!(t.insert(ofd(9), false, 0), Err(EMFILE));
    assert_eq!(t.dup(0, 0, false), Err(EMFILE));
    assert_eq!(t.open_count(), 3);
    assert_eq!(t.get(7).err(), Some(EBADF));
}
