//! The startup blob: round trip, every rejection, truncation and damage.

use nanox_runtime::start::{
    encode, StartError, StartInfo, HANDLE_ENTRY, HEADER_LEN, MAX_HANDLES, MAX_TOTAL, NAME_LEN,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn sample() -> Vec<u8> {
    let mut buf = vec![0u8; 512];
    let n = encode(
        &[b"prog".as_slice(), b"--flag", b"", b"x y"],
        &[
            ("authority", 0x0000_0001_0000_0007),
            ("parent", 9),
            ("log", u64::MAX),
        ],
        &mut buf,
    )
    .unwrap();
    buf.truncate(n);
    buf
}

fn set32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn a_blob_round_trips() {
    let b = sample();
    let s = StartInfo::parse(&b).unwrap();
    assert_eq!(s.arg_count(), 4);
    let args: Vec<&[u8]> = s.args().collect();
    assert_eq!(args, [b"prog".as_slice(), b"--flag", b"", b"x y"]);
    let hs: Vec<(&str, u64)> = s.handles().collect();
    assert_eq!(
        hs,
        [
            ("authority", 0x0000_0001_0000_0007),
            ("parent", 9),
            ("log", u64::MAX)
        ]
    );
    assert_eq!(s.handle("parent"), Some(9));
    assert_eq!(s.handle("nope"), None);
    assert_eq!(s.handle("paren"), None, "names match whole");
    // Extra bytes after the blob are ignored.
    let mut longer = b.clone();
    longer.extend_from_slice(&[0xAA; 100]);
    assert_eq!(StartInfo::parse(&longer).unwrap().arg_count(), 4);
    // From a raw pointer.
    let again = unsafe { StartInfo::from_raw(b.as_ptr(), b.len()) }.unwrap();
    assert_eq!(again.handle("log"), Some(u64::MAX));
}

#[test]
fn empty_and_minimal_blobs_are_fine() {
    let mut buf = [0u8; 64];
    let n = encode(&[], &[], &mut buf).unwrap();
    assert_eq!(n, HEADER_LEN);
    let s = StartInfo::parse(&buf[..n]).unwrap();
    assert_eq!((s.arg_count(), s.handles().count()), (0, 0));
    let n = encode(&[b""], &[], &mut buf).unwrap();
    assert_eq!(
        StartInfo::parse(&buf[..n]).unwrap().arg_count(),
        1,
        "one empty argument"
    );
}

#[test]
fn each_malformation_is_refused_with_its_own_error() {
    let ok = sample();
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = ok.clone();
        f(&mut b);
        StartInfo::parse(&b).err()
    };
    assert_eq!(
        StartInfo::parse(&ok[..10]).err(),
        Some(StartError::TooShort)
    );
    assert_eq!(with(&|b| b[0] = b'X'), Some(StartError::Magic));
    assert_eq!(with(&|b| b[4] = 2), Some(StartError::Version));
    assert_eq!(with(&|b| b[6] = 40), Some(StartError::HeaderLen));
    assert_eq!(
        with(&|b| {
            let n = b.len() as u32 + 1;
            set32(b, 8, n);
        }),
        Some(StartError::TotalLen)
    );
    assert_eq!(with(&|b| set32(b, 8, 31)), Some(StartError::TotalLen));
    assert_eq!(
        with(&|b| set32(b, 8, MAX_TOTAL as u32 + 1)),
        Some(StartError::TotalLen)
    );
    assert_eq!(with(&|b| set32(b, 12, 1)), Some(StartError::Flags));
    assert_eq!(
        with(&|b| set32(b, 16, 8)),
        Some(StartError::Bounds),
        "arguments inside the header"
    );
    assert_eq!(with(&|b| set32(b, 20, 10_000)), Some(StartError::Bounds));
    assert_eq!(
        with(&|b| set32(b, 16, u32::MAX)),
        Some(StartError::Bounds),
        "offset wraps"
    );
    assert_eq!(
        with(&|b| set32(b, 24, u32::MAX - 10)),
        Some(StartError::Bounds)
    );
    assert_eq!(
        with(&|b| set32(b, 28, MAX_HANDLES as u32 + 1)),
        Some(StartError::TooManyHandles)
    );
    assert_eq!(
        with(&|b| set32(b, 28, u32::MAX)),
        Some(StartError::TooManyHandles)
    );
    // The handle table over the arguments.
    assert_eq!(
        with(&|b| set32(b, 24, HEADER_LEN as u32)),
        Some(StartError::Overlap)
    );
    // The last argument loses its terminator.
    let args_len = u32::from_le_bytes(ok[20..24].try_into().unwrap()) as usize;
    assert_eq!(
        with(&|b| b[HEADER_LEN + args_len - 1] = b'z'),
        Some(StartError::ArgsUnterminated)
    );
}

#[test]
fn handle_table_entries_are_validated() {
    let ok = sample();
    let h_off = u32::from_le_bytes(ok[24..28].try_into().unwrap()) as usize;
    let entry = |i: usize| h_off + i * HANDLE_ENTRY;
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut b = ok.clone();
        f(&mut b);
        StartInfo::parse(&b).err()
    };
    assert_eq!(
        with(&|b| b[entry(0)] = 0),
        Some(StartError::BadName),
        "empty name"
    );
    assert_eq!(
        with(&|b| b[entry(0)..entry(0) + NAME_LEN].fill(0)),
        Some(StartError::BadName),
        "a name of only padding"
    );
    assert_eq!(
        with(&|b| b[entry(0)] = b' '),
        Some(StartError::BadName),
        "space"
    );
    assert_eq!(
        with(&|b| b[entry(0)] = 0x80),
        Some(StartError::BadName),
        "not ASCII"
    );
    assert_eq!(
        with(&|b| b[entry(0) + 2] = 7),
        Some(StartError::BadName),
        "control character"
    );
    assert_eq!(
        with(&|b| {
            b[entry(1) + 6] = 0;
            b[entry(1) + 7] = b'q';
        }),
        Some(StartError::BadName),
        "text after the padding began"
    );
    assert_eq!(
        with(&|b| {
            let name = b[entry(0)..entry(0) + NAME_LEN].to_vec();
            b[entry(2)..entry(2) + NAME_LEN].copy_from_slice(&name);
        }),
        Some(StartError::DuplicateName)
    );
    assert_eq!(
        with(&|b| b[entry(1) + NAME_LEN..entry(1) + HANDLE_ENTRY].fill(0)),
        Some(StartError::ZeroHandle)
    );
    // A name using all 16 bytes is fine.
    let mut buf = [0u8; 128];
    let n = encode(&[], &[("0123456789abcdef", 1)], &mut buf).unwrap();
    assert_eq!(
        StartInfo::parse(&buf[..n])
            .unwrap()
            .handle("0123456789abcdef"),
        Some(1)
    );
    // Too long a name is refused by the encoder.
    assert_eq!(
        encode(&[], &[("0123456789abcdefX", 1)], &mut buf),
        Err(StartError::BadName)
    );
}

#[test]
fn every_truncation_is_refused_and_damage_never_panics() {
    let ok = sample();
    for len in 0..ok.len() {
        assert!(
            StartInfo::parse(&ok[..len]).is_err(),
            "prefix of {len} bytes"
        );
    }
    let mut rng = Rng(0xFACE);
    let (mut accepted, mut rejected) = (0, 0);
    for _ in 0..200_000 {
        let mut b = ok.clone();
        for _ in 0..1 + rng.next() % 4 {
            let at = (rng.next() % b.len() as u64) as usize;
            b[at] = rng.next() as u8;
        }
        match StartInfo::parse(&b) {
            Ok(s) => {
                // Whatever was accepted can be read completely.
                let _ = (s.args().count(), s.handles().count(), s.handle("authority"));
                accepted += 1;
            }
            Err(_) => rejected += 1,
        }
    }
    assert!(accepted > 1000 && rejected > 1000, "{accepted} {rejected}");
}

#[test]
fn the_encoder_reports_what_does_not_fit() {
    let mut small = [0u8; 40];
    assert_eq!(
        encode(&[b"abcdefghijklmnopq"], &[], &mut small),
        Err(StartError::NoSpace)
    );
    let many: Vec<(String, u64)> = (0..=MAX_HANDLES).map(|i| (format!("h{i}"), 1)).collect();
    let refs: Vec<(&str, u64)> = many.iter().map(|(n, h)| (n.as_str(), *h)).collect();
    let mut big = vec![0u8; 8192];
    assert_eq!(
        encode(&[], &refs, &mut big),
        Err(StartError::TooManyHandles)
    );
    assert_eq!(
        encode(&[], &refs[..MAX_HANDLES], &mut big).map(|n| n > 0),
        Ok(true)
    );
    let huge = vec![b'a'; MAX_TOTAL];
    assert_eq!(
        encode(&[huge.as_slice()], &[], &mut vec![0u8; MAX_TOTAL + 64]),
        Err(StartError::TotalLen)
    );
}
