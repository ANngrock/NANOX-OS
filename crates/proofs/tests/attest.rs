//! Attestation freshness, hashing and the registry itself.

use std::fs;
use std::path::{Path, PathBuf};

use proofs::attest::{read_records, write_records};
use proofs::{hash_files, props, Proof, Property, Record, Status};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nanox-proofs-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn always() -> Proof {
    Proof::held(7)
}

fn never() -> Proof {
    Proof::failed(3, "x = 3".to_string())
}

fn prop(run: fn() -> Proof) -> Property {
    Property {
        id: "t.one",
        version: 1,
        statement: "s",
        bound: "b",
        component: &["comp.rs"],
        checker: "check.rs",
        run,
    }
}

fn setup(name: &str) -> PathBuf {
    let d = scratch(name);
    fs::write(d.join("comp.rs"), "fn a() {}\n").unwrap();
    fs::write(d.join("check.rs"), "fn check() {}\n").unwrap();
    d
}

#[test]
fn hash_depends_on_bytes_path_and_ignores_crlf() {
    let d = scratch("hash");
    fs::write(d.join("a"), "one\ntwo\n").unwrap();
    fs::write(d.join("b"), "one\r\ntwo\r\n").unwrap();
    fs::write(d.join("c"), "one\ntwo \n").unwrap();
    let ha = hash_files(&d, &["a"]).unwrap();
    assert_eq!(ha.len(), 64);
    assert_ne!(
        ha,
        hash_files(&d, &["b"]).unwrap(),
        "the path is part of the hash"
    );
    fs::write(d.join("a2"), "one\ntwo\n").unwrap();
    // Same bytes under two names differ; CRLF and LF with the same name agree.
    assert_ne!(
        hash_files(&d, &["a"]).unwrap(),
        hash_files(&d, &["a2"]).unwrap()
    );
    let before = hash_files(&d, &["a"]).unwrap();
    fs::write(d.join("a"), "one\r\ntwo\r\n").unwrap();
    assert_eq!(hash_files(&d, &["a"]).unwrap(), before);
    fs::write(d.join("a"), "one\ntwo \n").unwrap();
    assert_ne!(hash_files(&d, &["a"]).unwrap(), before);
    assert!(hash_files(&d, &["missing"]).is_err());
    // Concatenation boundaries matter: (ab)(c) is not (a)(bc).
    fs::write(d.join("x"), "ab").unwrap();
    fs::write(d.join("y"), "c").unwrap();
    fs::write(d.join("p"), "a").unwrap();
    fs::write(d.join("q"), "bc").unwrap();
    assert_ne!(
        hash_files(&d, &["x", "y"]).unwrap(),
        hash_files(&d, &["p", "q"]).unwrap()
    );
}

#[test]
fn record_line_round_trips_and_rejects_garbage() {
    let r = Record {
        id: "a.b".into(),
        version: 3,
        component: "c".repeat(64),
        checker: "d".repeat(64),
        cases: 123_456_789,
        passed: true,
        bound: "all {x} <= 3".into(),
    };
    assert_eq!(Record::parse(&r.to_line()), Some(r.clone()));
    let f = Record { passed: false, ..r };
    assert_eq!(Record::parse(&f.to_line()), Some(f));
    for bad in [
        "",
        "a\tb",
        "a\t1\tc\td\t5\tMAYBE\tb",
        "a\tx\tc\td\t5\tPASS\tb",
        "a\t1\tc\td\t-5\tPASS\tb",
    ] {
        assert_eq!(Record::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn an_attestation_is_current_until_a_source_version_or_bound_changes() {
    let d = setup("fresh");
    let p = prop(always);
    let proof = (p.run)();
    let rec = p.record(&d, &proof).unwrap();
    assert!(rec.passed && rec.cases == 7);
    assert_eq!(p.status(&d, None).unwrap(), Status::Missing);
    assert_eq!(p.status(&d, Some(&rec)).unwrap(), Status::Current);

    fs::write(d.join("comp.rs"), "fn a() { 1; }\n").unwrap();
    assert_eq!(
        p.status(&d, Some(&rec)).unwrap(),
        Status::Stale("component sources")
    );
    fs::write(d.join("comp.rs"), "fn a() {}\n").unwrap();
    assert_eq!(
        p.status(&d, Some(&rec)).unwrap(),
        Status::Current,
        "restoring the source restores it"
    );

    fs::write(d.join("check.rs"), "fn check() { 1; }\n").unwrap();
    assert_eq!(
        p.status(&d, Some(&rec)).unwrap(),
        Status::Stale("checker sources")
    );
    fs::write(d.join("check.rs"), "fn check() {}\n").unwrap();

    let newer = Property {
        version: 2,
        ..prop(always)
    };
    assert_eq!(
        newer.status(&d, Some(&rec)).unwrap(),
        Status::Stale("version")
    );
    let wider = Property {
        bound: "bigger",
        ..prop(always)
    };
    assert_eq!(
        wider.status(&d, Some(&rec)).unwrap(),
        Status::Stale("bound")
    );
}

#[test]
fn a_counterexample_is_recorded_and_reported_as_failed() {
    let d = setup("fail");
    let p = prop(never);
    let proof = (p.run)();
    assert_eq!(proof.failure.as_deref(), Some("x = 3"));
    let rec = p.record(&d, &proof).unwrap();
    assert!(!rec.passed);
    assert_eq!(p.status(&d, Some(&rec)).unwrap(), Status::Failed);
}

#[test]
fn attestation_files_round_trip() {
    let d = setup("file");
    let p = prop(always);
    let rec = p.record(&d, &(p.run)()).unwrap();
    let path = d.join("proofs.tsv");
    write_records(&path, std::slice::from_ref(&rec)).unwrap();
    assert_eq!(read_records(&path).unwrap(), vec![rec]);
    fs::write(&path, "garbage line without tabs\n").unwrap();
    assert!(read_records(&path).is_err());
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn registry_is_well_formed() {
    let all = props::all();
    assert!(all.len() >= 7, "{}", all.len());
    let root = workspace_root();
    for (i, p) in all.iter().enumerate() {
        assert!(
            all[i + 1..].iter().all(|q| q.id != p.id),
            "duplicate id {}",
            p.id
        );
        assert!(
            !p.bound.contains('\t') && !p.bound.contains('\n'),
            "{}",
            p.id
        );
        assert!(!p.statement.is_empty() && p.version >= 1);
        for f in p.component.iter().chain([&p.checker]) {
            assert!(root.join(f).is_file(), "{}: no such file {f}", p.id);
        }
    }
}

/// The checked-in attestations must match these sources. This fails the
/// moment a component or checker changes without the attestations being
/// regenerated (cargo run --release -p proofs -- prove --out ...).
#[test]
fn checked_in_attestations_are_current() {
    let root = workspace_root();
    let recs = read_records(&root.join("docs/research/proofs.tsv")).expect("attestation file");
    for p in props::all() {
        let rec = recs.iter().find(|r| r.id == p.id);
        assert_eq!(p.status(&root, rec).unwrap(), Status::Current, "{}", p.id);
    }
    for r in &recs {
        assert!(
            props::all().iter().any(|p| p.id == r.id),
            "orphan attestation {}",
            r.id
        );
    }
}
