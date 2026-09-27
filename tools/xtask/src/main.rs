//! Host-only build and experiment controller. Guest crates never depend on this package.
mod build;
mod image;
mod record;
mod runner;

use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Pass,
    MissingKernel,
    TruncatedElf,
    BadSegments,
    KernelFail,
    KernelPanic,
    KernelHang,
}

impl Scenario {
    const ALL: [Self; 7] = [
        Self::Pass,
        Self::MissingKernel,
        Self::TruncatedElf,
        Self::BadSegments,
        Self::KernelFail,
        Self::KernelPanic,
        Self::KernelHang,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::MissingKernel => "missing-kernel",
            Self::TruncatedElf => "truncated-elf",
            Self::BadSegments => "bad-segments",
            Self::KernelFail => "kernel-fail",
            Self::KernelPanic => "kernel-panic",
            Self::KernelHang => "kernel-hang",
        }
    }
    fn epoch(self) -> u64 {
        match self {
            Self::KernelFail => u64::MAX,
            Self::KernelHang => u64::MAX - 1,
            Self::KernelPanic => u64::MAX - 2,
            _ => 20260922,
        }
    }
    fn parse(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|x| x.name() == s)
            .ok_or_else(|| format!("unknown scenario: {s}").into())
    }
}

#[derive(Clone)]
struct Options {
    root: PathBuf,
    timeout: Duration,
    build_timeout: Duration,
    no_build: bool,
    replay: bool,
    scenario: Scenario,
    run_id: Option<String>,
}
impl Options {
    fn parse() -> Result<(String, Self)> {
        let mut args = env::args().skip(1);
        let command = args.next().unwrap_or_else(|| "help".into());
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let mut options = Self {
            root,
            timeout: Duration::from_secs(30),
            build_timeout: Duration::from_secs(600),
            no_build: false,
            replay: false,
            scenario: Scenario::Pass,
            run_id: None,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--timeout" => {
                    options.timeout =
                        Duration::from_secs(args.next().ok_or("--timeout needs seconds")?.parse()?)
                }
                "--build-timeout" => {
                    options.build_timeout = Duration::from_secs(
                        args.next()
                            .ok_or("--build-timeout needs seconds")?
                            .parse()?,
                    )
                }
                "--no-build" => options.no_build = true,
                "--replay" => options.replay = true,
                "--scenario" => {
                    options.scenario =
                        Scenario::parse(&args.next().ok_or("--scenario needs a name")?)?
                }
                "--run" => {
                    let id = args.next().ok_or("--run needs an ID")?;
                    validate_run_id(&id)?;
                    options.run_id = Some(id);
                }
                _ => return Err(format!("unknown option: {arg}").into()),
            }
        }
        if options.timeout.is_zero() || options.build_timeout.is_zero() {
            return Err("timeouts must be positive".into());
        }
        Ok((command, options))
    }
}

fn validate_run_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 160
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("run ID must contain only ASCII letters, digits, '-' or '_'".into());
    }
    Ok(())
}

fn linux_build_root(root: &Path) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err("guest builds/run require the primary Linux filesystem checkout in WSL; use cargo test -p xtask for host-only checks".into());
    }
    if root.starts_with("/mnt") {
        return Err(
            "refusing a guest build on /mnt; use the primary Linux filesystem checkout".into(),
        );
    }
    Ok(())
}

fn run() -> Result<()> {
    let (command, options) = Options::parse()?;
    match command.as_str() {
        "doctor" => build::doctor(&options),
        "build" => {
            linux_build_root(&options.root)?;
            build::build(&options)?;
            Ok(())
        }
        "run" => {
            linux_build_root(&options.root)?;
            build::ensure(&options)?;
            let result = runner::execute(&options, options.scenario, "boot-test", None)?;
            println!("{}: {} ({})", result.run_id, result.verdict, result.reason);
            if result.expectation_met {
                Ok(())
            } else {
                Err("guest run did not satisfy its scenario; see record.json".into())
            }
        }
        "test" => {
            linux_build_root(&options.root)?;
            runner::test(&options)
        }
        "reproduce-build" => {
            linux_build_root(&options.root)?;
            build::reproduce(&options)
        }
        "debug" => {
            linux_build_root(&options.root)?;
            runner::debug(&options)
        }
        "replay" => {
            linux_build_root(&options.root)?;
            runner::replay_existing(&options)
        }
        "help" | "--help" | "-h" => {
            println!("cargo xtask doctor|build|run|test|reproduce-build|debug|replay\n  --timeout SEC         QEMU timeout (default 30; excludes build)\n  --build-timeout SEC   per-build timeout (default 600)\n  --no-build            require verified existing binaries\n  --scenario NAME       pass, missing-kernel, truncated-elf, bad-segments, kernel-fail, kernel-panic, kernel-hang\n  test --replay         real QEMU record/replay plus input-mismatch check\n  debug --run ID        replay original image/firmware inputs with GDB on 127.0.0.1:1234, paused\n  replay --run ID       verify and replay an existing replay-record run");
            Ok(())
        }
        _ => Err(format!("unknown command: {command}").into()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("NANOX harness: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_directory_escape() {
        for id in ["../a", "/tmp", "a/b", "a\\b", "", ".", "a b"] {
            assert!(validate_run_id(id).is_err());
        }
        assert!(validate_run_id("run-20260922_1").is_ok());
    }
    #[test]
    fn unique_fault_epochs() {
        assert_ne!(Scenario::KernelFail.epoch(), Scenario::KernelHang.epoch());
        assert_ne!(Scenario::KernelFail.epoch(), Scenario::KernelPanic.epoch());
    }
}
