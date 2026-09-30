//! Properties, attestation records and their freshness.

use std::fmt::Write as _;
use std::path::Path;
use std::{fs, io};

use crate::sha256::{hex, Sha256};

/// The outcome of running a property.
pub struct Proof {
    /// How many cases were examined.
    pub cases: u64,
    /// `None`: the property held on every case; otherwise a counterexample.
    pub failure: Option<String>,
}

impl Proof {
    pub fn held(cases: u64) -> Self {
        Self {
            cases,
            failure: None,
        }
    }
    pub fn failed(cases: u64, why: String) -> Self {
        Self {
            cases,
            failure: Some(why),
        }
    }
}

/// A property of a component, with the bound it is checked within.
pub struct Property {
    pub id: &'static str,
    /// Bumped by hand when the statement or the bound changes.
    pub version: u32,
    pub statement: &'static str,
    pub bound: &'static str,
    /// Source files (repository-relative) of the component it is about.
    pub component: &'static [&'static str],
    /// The file that holds the checker.
    pub checker: &'static str,
    pub run: fn() -> Proof,
}

/// SHA-256 over the files, each contributing its path, its length and its
/// bytes with CRLF turned into LF (so a Windows checkout hashes the same).
pub fn hash_files(root: &Path, files: &[&str]) -> io::Result<String> {
    let mut h = Sha256::new();
    for f in files {
        let raw =
            fs::read(root.join(f)).map_err(|e| io::Error::new(e.kind(), format!("{f}: {e}")))?;
        let mut bytes = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'\r' && raw.get(i + 1) == Some(&b'\n') {
                i += 1;
                continue;
            }
            bytes.push(raw[i]);
            i += 1;
        }
        h.update(f.as_bytes());
        h.update(&[0]);
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(hex(&h.finish()))
}

/// One line of the attestation file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub version: u32,
    pub component: String,
    pub checker: String,
    pub cases: u64,
    pub passed: bool,
    pub bound: String,
}

const HEADER: &str = "# NANOX proof attestations. Regenerate: cargo run --release -p proofs -- prove --out docs/research/proofs.tsv\n\
# id <TAB> version <TAB> component sha256 <TAB> checker sha256 <TAB> cases <TAB> result <TAB> bound\n";

impl Record {
    pub fn to_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.id,
            self.version,
            self.component,
            self.checker,
            self.cases,
            if self.passed { "PASS" } else { "FAIL" },
            self.bound
        )
    }

    pub fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            return None;
        }
        Some(Self {
            id: f[0].to_string(),
            version: f[1].parse().ok()?,
            component: f[2].to_string(),
            checker: f[3].to_string(),
            cases: f[4].parse().ok()?,
            passed: match f[5] {
                "PASS" => true,
                "FAIL" => false,
                _ => return None,
            },
            bound: f[6].to_string(),
        })
    }
}

pub fn write_records(path: &Path, records: &[Record]) -> io::Result<()> {
    let mut s = String::from(HEADER);
    for r in records {
        let _ = writeln!(s, "{}", r.to_line());
    }
    fs::write(path, s)
}

pub fn read_records(path: &Path) -> io::Result<Vec<Record>> {
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.push(Record::parse(line).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {}: bad record", n + 1),
            )
        })?);
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// Attested on exactly these sources, statement and bound, and it held.
    Current,
    /// Sources, checker, version or bound differ from the attestation.
    Stale(&'static str),
    /// No attestation for this property.
    Missing,
    /// Attested, current, but the recorded result is a failure.
    Failed,
}

impl Property {
    /// What running this property now gives, as a record.
    pub fn record(&self, root: &Path, proof: &Proof) -> io::Result<Record> {
        Ok(Record {
            id: self.id.to_string(),
            version: self.version,
            component: hash_files(root, self.component)?,
            checker: hash_files(root, &[self.checker])?,
            cases: proof.cases,
            passed: proof.failure.is_none(),
            bound: self.bound.to_string(),
        })
    }

    /// Freshness of an attestation against the sources in `root`.
    pub fn status(&self, root: &Path, rec: Option<&Record>) -> io::Result<Status> {
        let Some(r) = rec else {
            return Ok(Status::Missing);
        };
        if r.version != self.version {
            return Ok(Status::Stale("version"));
        }
        if r.bound != self.bound {
            return Ok(Status::Stale("bound"));
        }
        if r.component != hash_files(root, self.component)? {
            return Ok(Status::Stale("component sources"));
        }
        if r.checker != hash_files(root, &[self.checker])? {
            return Ok(Status::Stale("checker sources"));
        }
        Ok(if r.passed {
            Status::Current
        } else {
            Status::Failed
        })
    }
}
