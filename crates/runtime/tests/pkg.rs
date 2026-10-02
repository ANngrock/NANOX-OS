//! The package reader and the hash under it. The fixture is written by
//! tools/native/nxpk.py (and checked against it by that tool's tests), so a
//! disagreement between the two implementations shows up on both sides.

use nanox_runtime::pkg::{Archive, Kind, PkgError};
use nanox_runtime::sha256::{sha256, Sha256};

const FIXTURE: &[u8] = include_bytes!("data/sample.nxpk");
const ARCHIVE_DIGEST: &str = "5c6e65a294a74851b37eb7c2a90c28769dd5645eab75c0982488fab11a88b518";

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

// ---------------------------------------------------------------- SHA-256

#[test]
fn published_vectors() {
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    assert_eq!(
        hex(&sha256(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu")),
        "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
    );
}

#[test]
fn a_million_a() {
    let mut h = Sha256::new();
    for _ in 0..1000 {
        h.update(&[b'a'; 1000]);
    }
    assert_eq!(
        hex(&h.finalize()),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn padding_boundaries_agree_with_hashlib() {
    for (n, want) in [
        (
            0,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            1,
            "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
        ),
        (
            55,
            "463eb28e72f82e0a96c0a4cc53690c571281131f672aa229e0d45ae59b598b59",
        ),
        (
            56,
            "da2ae4d6b36748f2a318f23e7ab1dfdf45acdc9d049bd80e59de82a60895f562",
        ),
        (
            57,
            "2fe741af801cc238602ac0ec6a7b0c3a8a87c7fc7d7f02a3fe03d1c12eac4d8f",
        ),
        (
            63,
            "29af2686fd53374a36b0846694cc342177e428d1647515f078784d69cdb9e488",
        ),
        (
            64,
            "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108",
        ),
        (
            65,
            "4bfd2c8b6f1eec7a2afeb48b934ee4b2694182027e6d0fc075074f2fabb31781",
        ),
        (
            119,
            "da18797ed7c3a777f0847f429724a2d8cd5138e6ed2895c3fa1a6d39d18f7ec6",
        ),
        (
            120,
            "f52b23db1fbb6ded89ef42a23ce0c8922c45f25c50b568a93bf1c075420bbb7c",
        ),
        (
            127,
            "92ca0fa6651ee2f97b884b7246a562fa71250fedefe5ebf270d31c546bfea976",
        ),
        (
            128,
            "471fb943aa23c511f6f72f8d1652d9c880cfa392ad80503120547703e56a2be5",
        ),
        (
            129,
            "5099c6a56203f9687f7d33f4bfdf576d31dc91f6b695ecea38b2770c87631135",
        ),
        (
            1000,
            "4e4c294b331f7a2099a379bec34b9f9fc03dc46ab465d998f4d683da53487e6d",
        ),
    ] {
        assert_eq!(hex(&sha256(&pattern(n))), want, "length {n}");
    }
}

#[test]
fn streaming_in_any_pieces_gives_the_same_digest() {
    let data = pattern(300);
    let want = sha256(&data);
    for cut in 0..=data.len() {
        let mut h = Sha256::new();
        h.update(&data[..cut]);
        h.update(&data[cut..]);
        assert_eq!(h.finalize(), want, "split at {cut}");
    }
    for step in [1, 3, 7, 63, 64, 65, 100] {
        let mut h = Sha256::new();
        for piece in data.chunks(step) {
            h.update(piece);
        }
        assert_eq!(h.finalize(), want, "pieces of {step}");
    }
    let mut h = Sha256::default();
    h.update(b"");
    h.update(&data);
    assert_eq!(h.finalize(), want, "empty updates change nothing");
}

// ----------------------------------------------------------------- archive

#[test]
fn the_fixture_parses_and_verifies() {
    let a = Archive::parse(FIXTURE).unwrap();
    assert_eq!(a.len(), 3);
    assert!(!a.is_empty());
    a.verify().unwrap();
    assert_eq!(hex(&a.digest()), ARCHIVE_DIGEST);
    let got: Vec<_> = a
        .entries()
        .map(|e| (e.name, e.mode, e.data.len(), hex(&e.sha256)))
        .collect();
    assert_eq!(
        got,
        [
            (
                "alpha.bin",
                0o755,
                96,
                "8050f9c626460e59ecf977244a4d7ec8fb57be3bca148d3a74c59f6a6235aee0".to_string()
            ),
            (
                "hello",
                0o755,
                40,
                "5620dcd4c0ab4736a35122b1973e2d5d9726c378b3893234c66ff78d071b0e87".to_string()
            ),
            (
                "zeta",
                0o644,
                0,
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()
            ),
        ]
    );
    let hello = a.find("hello").unwrap();
    assert_eq!(hello.kind, Kind::Program);
    assert_eq!(hello.data, (1..=40u8).collect::<Vec<_>>());
    let alpha = a.find("alpha.bin").unwrap();
    assert_eq!(
        alpha.data,
        (0..96).map(|i| ((i * 7) % 256) as u8).collect::<Vec<_>>()
    );
    assert!(a.find("nope").is_none());
    assert!(
        a.find("hell").is_none() && a.find("hello ").is_none(),
        "names match exactly"
    );
}

#[test]
fn an_empty_archive_is_valid() {
    let mut blob = b"NXPK\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
    blob.extend_from_slice(&sha256(&blob));
    let a = Archive::parse(&blob).unwrap();
    assert!(a.is_empty());
    assert_eq!(a.entries().count(), 0);
    a.verify().unwrap();
}

#[test]
fn every_single_byte_change_is_refused() {
    for i in 0..FIXTURE.len() {
        let mut bad = FIXTURE.to_vec();
        bad[i] ^= 1;
        let refused = match Archive::parse(&bad) {
            Err(_) => true,
            Ok(a) => a.verify().is_err(),
        };
        assert!(refused, "byte {i}");
    }
}

#[test]
fn every_truncation_and_extension_is_refused() {
    for n in 0..FIXTURE.len() {
        assert!(Archive::parse(&FIXTURE[..n]).is_err(), "cut at {n}");
    }
    for extra in [&b"\0"[..], &[0u8; 16], b"junk"] {
        let mut longer = FIXTURE.to_vec();
        longer.extend_from_slice(extra);
        let refused = match Archive::parse(&longer) {
            Err(_) => true,
            Ok(a) => a.verify().is_err(),
        };
        assert!(refused);
    }
}

fn retrailer(mut body: Vec<u8>) -> Vec<u8> {
    body.truncate(body.len() - 32);
    let d = sha256(&body);
    body.extend_from_slice(&d);
    body
}

fn patched(at: usize, new: &[u8]) -> Vec<u8> {
    let mut b = FIXTURE.to_vec();
    b[at..at + new.len()].copy_from_slice(new);
    retrailer(b)
}

/// Where the first directory entry's fields are (the name "alpha.bin" has 9 bytes).
const E0: usize = 16;
const NAME0: usize = E0 + 1;
const KIND0: usize = NAME0 + 9;
const MODE0: usize = KIND0 + 1;
const SIZE0: usize = MODE0 + 2;
const OFFSET0: usize = SIZE0 + 8;

#[test]
fn structural_faults_are_named_even_with_a_valid_trailer() {
    let cases: [(&str, Vec<u8>, PkgError); 14] = [
        ("magic", patched(0, b"NXPX"), PkgError::BadMagic),
        ("version", patched(4, &[2, 0]), PkgError::UnknownVersion),
        ("flags", patched(6, &[1, 0]), PkgError::UnknownFlags),
        (
            "too many entries",
            patched(8, &[1, 1, 0, 0]),
            PkgError::TooManyEntries,
        ),
        (
            "directory beyond the archive",
            patched(12, &[0xff, 0xff, 0, 0]),
            PkgError::DirectoryOutOfRange,
        ),
        (
            "fewer entries than the directory holds",
            patched(8, &[2, 0, 0, 0]),
            PkgError::DirectoryOutOfRange,
        ),
        (
            "more entries than the directory holds",
            patched(8, &[4, 0, 0, 0]),
            PkgError::DirectoryOutOfRange,
        ),
        ("name with a space", patched(NAME0, b" "), PkgError::BadName),
        (
            "names out of order",
            patched(NAME0, b"z"),
            PkgError::NamesNotSorted,
        ),
        ("unknown kind", patched(KIND0, &[2]), PkgError::UnknownKind),
        (
            "mode out of range",
            patched(MODE0, &[0xff, 0xff]),
            PkgError::BadMode,
        ),
        (
            "data offset moved",
            patched(OFFSET0, &[0x20]),
            PkgError::NotCanonical,
        ),
        (
            "size larger than the archive",
            patched(SIZE0 + 1, &[0xff]),
            PkgError::DataOutOfRange,
        ),
        (
            "size one byte short: the last byte would be padding",
            patched(SIZE0, &[95]),
            PkgError::PaddingNotZero,
        ),
    ];
    for (what, bytes, want) in cases {
        assert_eq!(Archive::parse(&bytes).unwrap_err(), want, "{what}");
    }
}

#[test]
fn padding_must_be_zero() {
    // The byte after the 40-byte "hello" blob is padding.
    let a = Archive::parse(FIXTURE).unwrap();
    let hello = a.find("hello").unwrap();
    let at = hello.data.as_ptr() as usize - FIXTURE.as_ptr() as usize + hello.data.len();
    assert_eq!(
        Archive::parse(&patched(at, &[1])).unwrap_err(),
        PkgError::PaddingNotZero
    );
    // and the padding between the directory and the first blob
    let dir_end = 16 + (1 + 9 + 51) + (1 + 5 + 51) + (1 + 4 + 51);
    assert_eq!(
        dir_end % 16,
        14,
        "the fixture has directory padding to test"
    );
    assert_eq!(
        Archive::parse(&patched(dir_end, &[1])).unwrap_err(),
        PkgError::PaddingNotZero
    );
}

#[test]
fn a_hash_mismatch_with_a_valid_structure_is_found_by_verify() {
    // Change a data byte and recompute only the trailer: parse accepts, verify names the program hash.
    let a = Archive::parse(FIXTURE).unwrap();
    let alpha = a.find("alpha.bin").unwrap();
    let at = alpha.data.as_ptr() as usize - FIXTURE.as_ptr() as usize;
    let bad = patched(at + 3, &[0xee]);
    let b = Archive::parse(&bad).unwrap();
    assert_eq!(b.verify().unwrap_err(), PkgError::HashMismatch);
    // Change the trailer only: the whole-archive hash fails.
    let mut bad = FIXTURE.to_vec();
    let n = bad.len();
    bad[n - 1] ^= 0x80;
    assert_eq!(
        Archive::parse(&bad).unwrap().verify().unwrap_err(),
        PkgError::HashMismatch
    );
}

#[test]
fn trailing_bytes_between_the_last_entry_and_the_trailer_are_refused() {
    let mut body = FIXTURE[..FIXTURE.len() - 32].to_vec();
    body.extend_from_slice(&[0u8; 16]);
    let d = sha256(&body);
    body.extend_from_slice(&d);
    assert_eq!(Archive::parse(&body).unwrap_err(), PkgError::TrailingBytes);
}

#[test]
fn too_short_inputs() {
    assert_eq!(Archive::parse(b"").unwrap_err(), PkgError::TooShort);
    assert_eq!(
        Archive::parse(&FIXTURE[..47]).unwrap_err(),
        PkgError::TooShort
    );
}

// A second writer, independent of the Python one: if the two agree on the
// bytes of the fixture, the layout is the same in both.

fn align16(n: usize) -> usize {
    (n + 15) & !15
}

/// Writes entries in the order given (the caller sorts, or deliberately does not).
fn build(entries: &[(&str, u16, &[u8])]) -> Vec<u8> {
    let dir_len: usize = entries.iter().map(|(n, _, _)| 1 + n.len() + 51).sum();
    let mut dir = Vec::new();
    let mut blobs = Vec::new();
    let mut at = align16(16 + dir_len);
    for (name, mode, data) in entries {
        dir.push(name.len() as u8);
        dir.extend_from_slice(name.as_bytes());
        dir.push(1);
        dir.extend_from_slice(&mode.to_le_bytes());
        dir.extend_from_slice(&(data.len() as u64).to_le_bytes());
        dir.extend_from_slice(&(at as u64).to_le_bytes());
        dir.extend_from_slice(&sha256(data));
        blobs.extend_from_slice(data);
        let pad = align16(data.len()) - data.len();
        blobs.resize(blobs.len() + pad, 0);
        at += data.len() + pad;
    }
    let mut out = b"NXPK".to_vec();
    out.extend_from_slice(&[1, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&(dir_len as u32).to_le_bytes());
    out.extend_from_slice(&dir);
    out.resize(align16(out.len()), 0);
    out.extend_from_slice(&blobs);
    let d = sha256(&out);
    out.extend_from_slice(&d);
    out
}

#[test]
fn the_rust_writer_reproduces_the_python_fixture_byte_for_byte() {
    let alpha: Vec<u8> = (0..96).map(|i| ((i * 7) % 256) as u8).collect();
    let hello: Vec<u8> = (1..=40u8).collect();
    let blob = build(&[
        ("alpha.bin", 0o755, &alpha),
        ("hello", 0o755, &hello),
        ("zeta", 0o644, &[]),
    ]);
    assert_eq!(blob, FIXTURE);
}

#[test]
fn names_must_be_strictly_increasing() {
    let a = build(&[("a", 0o755, b"1"), ("a", 0o755, b"2")]);
    assert_eq!(
        Archive::parse(&a).unwrap_err(),
        PkgError::NamesNotSorted,
        "duplicate"
    );
    let b = build(&[("b", 0o755, b"1"), ("a", 0o755, b"2")]);
    assert_eq!(
        Archive::parse(&b).unwrap_err(),
        PkgError::NamesNotSorted,
        "descending"
    );
    let c = build(&[("a", 0o755, b"1"), ("b", 0o755, b"2")]);
    Archive::parse(&c).unwrap().verify().unwrap();
}

#[test]
fn name_rules() {
    let long = "n".repeat(64);
    let ok = build(&[(&long, 0o755, b"x")]);
    assert_eq!(
        Archive::parse(&ok).unwrap().entries().next().unwrap().name,
        long
    );
    let too_long = "n".repeat(65);
    assert_eq!(
        Archive::parse(&build(&[(&too_long, 0o755, b"x")])).unwrap_err(),
        PkgError::BadName
    );
    for bad in ["", "a b", "a/b", "\u{e4}", "a\0b"] {
        assert_eq!(
            Archive::parse(&build(&[(bad, 0o755, b"x")])).unwrap_err(),
            PkgError::BadName,
            "{bad:?}"
        );
    }
    for good in ["A", "a.b", "a_b", "a-b", "0", "..."] {
        Archive::parse(&build(&[(good, 0o755, b"x")])).unwrap();
    }
}

#[test]
fn the_entry_count_limit() {
    let names: Vec<String> = (0..257).map(|i| format!("n{i:03}")).collect();
    let all: Vec<(&str, u16, &[u8])> = names
        .iter()
        .map(|n| (n.as_str(), 0o755u16, &b"x"[..]))
        .collect();
    let full = build(&all[..256]);
    let a = Archive::parse(&full).unwrap();
    assert_eq!(a.len(), 256);
    a.verify().unwrap();
    assert_eq!(
        Archive::parse(&build(&all)).unwrap_err(),
        PkgError::TooManyEntries
    );
}

#[test]
fn blob_sizes_around_the_alignment() {
    for n in [0usize, 1, 15, 16, 17, 31, 32, 33] {
        let data = pattern(n);
        let blob = build(&[("a", 0o755, &data), ("b", 0o755, &data)]);
        let a = Archive::parse(&blob).unwrap();
        a.verify().unwrap();
        assert_eq!(a.find("b").unwrap().data, &data[..], "size {n}");
    }
}

#[test]
fn the_mode_limit_is_exact() {
    assert_eq!(
        Archive::parse(&patched(MODE0, &[0xff, 0x0f]))
            .unwrap()
            .len(),
        3,
        "0o7777 is the largest mode"
    );
    assert_eq!(
        Archive::parse(&patched(MODE0, &[0x00, 0x10])).unwrap_err(),
        PkgError::BadMode,
        "0o10000 is one too many"
    );
}

#[test]
fn a_directory_that_runs_into_the_trailer_is_refused_as_such() {
    // dir_len chosen so that the directory ends one byte into the trailer
    let body_end = FIXTURE.len() - 32;
    let dir_len = (body_end + 1 - 16) as u32;
    assert_eq!(
        Archive::parse(&patched(12, &dir_len.to_le_bytes())).unwrap_err(),
        PkgError::DirectoryOutOfRange
    );
}

#[test]
fn data_whose_padding_would_run_into_the_trailer_is_refused_as_such() {
    // Three stray bytes before the trailer make the end of the body unaligned; then the last
    // entry (empty, at the old end of the body) is given two bytes, whose padding would
    // reach past the body.
    let mut body = FIXTURE[..FIXTURE.len() - 32].to_vec();
    body.extend_from_slice(&[0, 0, 0]);
    let d = sha256(&body);
    body.extend_from_slice(&d);
    let zeta_size = 16 + (1 + 9 + 51) + (1 + 5 + 51) + 1 + 4 + 1 + 2;
    body[zeta_size] = 2;
    let body = retrailer(body);
    assert_eq!(Archive::parse(&body).unwrap_err(), PkgError::DataOutOfRange);
}

#[test]
fn bytes_after_the_directory_are_never_taken_for_entries() {
    // A 12-byte name makes the directory end on a 16-byte boundary, so the data follows it
    // immediately; the data below looks exactly like a directory entry.
    let mut fake = vec![1, b'x', 1];
    fake.extend_from_slice(&0o755u16.to_le_bytes());
    fake.extend_from_slice(&0u64.to_le_bytes());
    fake.extend_from_slice(&80u64.to_le_bytes());
    fake.extend_from_slice(&[0u8; 32]);
    let blob = build(&[("abcdefghijkl", 0o755, &fake)]);
    let a = Archive::parse(&blob).unwrap();
    a.verify().unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(
        a.entries().count(),
        1,
        "the iterator stops where the directory stops"
    );
    assert!(a.find("x").is_none());
}
