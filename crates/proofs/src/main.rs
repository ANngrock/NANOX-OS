//! proofs: run the bounded exhaustive checks and keep their attestations.
//!
//!   proofs list
//!   proofs prove  [--root DIR] [--out FILE] [ID...]   run, print, optionally write attestations
//!   proofs check  [--root DIR] [--file FILE]          are the attestations current for these sources?
//!   proofs verify [--root DIR] [--file FILE]          re-run everything and compare with the file

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use proofs::attest::{read_records, write_records};
use proofs::props;
use proofs::{Property, Record, Status};

struct Args {
    root: PathBuf,
    file: PathBuf,
    out: Option<PathBuf>,
    ids: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        root: PathBuf::from("."),
        file: PathBuf::from("docs/research/proofs.tsv"),
        out: None,
        ids: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match x.as_str() {
            "--root" => a.root = PathBuf::from(value("--root")?),
            "--file" => a.file = PathBuf::from(value("--file")?),
            "--out" => a.out = Some(PathBuf::from(value("--out")?)),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.ids.push(s.to_string()),
        }
    }
    Ok(a)
}

fn run_all(a: &Args, props: &[Property]) -> Result<(Vec<Record>, bool), String> {
    let mut records = Vec::new();
    let mut all_held = true;
    for p in props {
        if !a.ids.is_empty() && !a.ids.iter().any(|i| i == p.id) {
            continue;
        }
        let t = Instant::now();
        let proof = (p.run)();
        let rec = p.record(&a.root, &proof).map_err(|e| e.to_string())?;
        println!(
            "{:<8} {:<40} {:>14} cases {:>8.2}s",
            if rec.passed { "PASS" } else { "FAIL" },
            p.id,
            proof.cases,
            t.elapsed().as_secs_f64()
        );
        if let Some(why) = &proof.failure {
            println!("         counterexample: {why}");
            all_held = false;
        }
        records.push(rec);
    }
    Ok((records, all_held))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first() else {
        eprintln!("usage: proofs list | prove | check | verify");
        return ExitCode::from(2);
    };
    let a = match parse(&argv[1..]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let props = props::all();
    match cmd.as_str() {
        "list" => {
            for p in &props {
                println!(
                    "{} (version {})\n  {}\n  bound: {}\n",
                    p.id, p.version, p.statement, p.bound
                );
            }
            ExitCode::SUCCESS
        }
        "prove" => match run_all(&a, &props) {
            Ok((records, held)) => {
                if let Some(out) = &a.out {
                    if let Err(e) = write_records(out, &records) {
                        eprintln!("{}: {e}", out.display());
                        return ExitCode::from(2);
                    }
                    println!("wrote {} ({} records)", out.display(), records.len());
                }
                if held {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        },
        "check" => check(&a, &props),
        "verify" => verify(&a, &props),
        other => {
            eprintln!("unknown command {other}");
            ExitCode::from(2)
        }
    }
}

fn load(a: &Args) -> Result<Vec<Record>, ExitCode> {
    read_records(&a.root.join(&a.file)).map_err(|e| {
        eprintln!("{}: {e}", a.file.display());
        ExitCode::from(2)
    })
}

fn check(a: &Args, props: &[Property]) -> ExitCode {
    let recs = match load(a) {
        Ok(r) => r,
        Err(c) => return c,
    };
    let mut bad = 0;
    for p in props {
        let rec = recs.iter().find(|r| r.id == p.id);
        let st = match p.status(&a.root, rec) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {e}", p.id);
                return ExitCode::from(2);
            }
        };
        let text = match &st {
            Status::Current => "current".to_string(),
            Status::Stale(why) => format!("STALE ({why} changed)"),
            Status::Missing => "MISSING".to_string(),
            Status::Failed => "FAILED (attested counterexample)".to_string(),
        };
        println!("{:<40} {text}", p.id);
        bad += usize::from(st != Status::Current);
    }
    for r in &recs {
        if !props.iter().any(|p| p.id == r.id) {
            println!("{:<40} ORPHAN (no such property)", r.id);
            bad += 1;
        }
    }
    if bad == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn verify(a: &Args, props: &[Property]) -> ExitCode {
    let recs = match load(a) {
        Ok(r) => r,
        Err(c) => return c,
    };
    let (fresh, held) = match run_all(a, props) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let mut differ = 0;
    for f in &fresh {
        match recs.iter().find(|r| r.id == f.id) {
            Some(r) if r == f => {}
            Some(r) => {
                println!(
                    "DIFFERS {}\n  file : {}\n  fresh: {}",
                    f.id,
                    r.to_line(),
                    f.to_line()
                );
                differ += 1;
            }
            None => {
                println!("MISSING {}", f.id);
                differ += 1;
            }
        }
    }
    if differ == 0 && held {
        println!("verified: {} attestations reproduced", fresh.len());
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
