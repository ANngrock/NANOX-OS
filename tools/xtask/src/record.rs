use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn hash_file(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut state = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        state.update(&buffer[..n]);
    }
    Ok(format!("{:x}", state.finalize()))
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(path, bytes)?;
    Ok(())
}
pub fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn string_path(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "non-UTF8 paths are unsupported in RunRecord".into())
}
pub fn capture(root: &Path, program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .env("LC_ALL", "C")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let bytes = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}
pub fn version(root: &Path, program: &str, args: &[&str]) -> String {
    capture(root, program, args).unwrap_or_else(|e| format!("UNAVAILABLE: {e}"))
}

pub fn source(root: &Path) -> Result<Value> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err("git ls-files failed".into());
    }
    let mut files = BTreeMap::<String, Value>::new();
    for bytes in output.stdout.split(|b| *b == 0).filter(|x| !x.is_empty()) {
        let name = std::str::from_utf8(bytes)?.replace('\\', "/");
        // The experiment fingerprint covers every possible input in this M0
        // workspace, including untracked sources. Editorial status updates and
        // local agent settings must not invalidate an already recorded boot.
        let input = ["boot/", "kernel/", "crates/", "tools/", "tests/", ".cargo/"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            || matches!(
                name.as_str(),
                "Cargo.toml"
                    | "Cargo.lock"
                    | "flake.nix"
                    | "flake.lock"
                    | "rust-toolchain.toml"
                    | "docs/specs/machine-profile.toml"
            );
        if !input {
            continue;
        }
        let path = root.join(&name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&path)?;
                files.insert(name, json!({"symlink": string_path(&link)?, "sha256":hash_bytes(string_path(&link)?.as_bytes())}));
            }
            Ok(meta) if meta.is_file() => {
                files.insert(name, json!({"sha256":hash_file(&path)?,"bytes":meta.len()}));
            }
            Ok(_) => {
                return Err(format!("source input is not a regular file or symlink: {name}").into())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                files.insert(name, json!({"deleted":true}));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let status = capture(
        root,
        "git",
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    let patch = Command::new("git")
        .args(["diff", "--binary", "HEAD", "--", "."])
        .current_dir(root)
        .output()?;
    if !patch.status.success() {
        return Err("git diff failed".into());
    }
    let manifest = serde_json::to_vec(&files)?;
    Ok(
        json!({"commit":capture(root,"git",&["rev-parse","HEAD"])?,"branch":capture(root,"git",&["rev-parse","--abbrev-ref","HEAD"])?,"dirty":!status.is_empty(),"manifest_sha256":hash_bytes(&manifest),"patch_sha256":hash_bytes(&patch.stdout),"files":files}),
    )
}

pub fn new_run(root: &Path, label: &str) -> Result<(String, PathBuf)> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let id = format!("{now}-{}-{label}", std::process::id());
    let path = root.join("out/runs").join(&id);
    fs::create_dir_all(&path)?;
    Ok((id, path))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
    pub elapsed_ms: u128,
    pub signal: Option<i32>,
}

/// Only the process group created by this invocation is terminated on a timeout.
pub fn process(
    root: &Path,
    program: &str,
    args: &[String],
    stdout: &Path,
    stderr: &Path,
    timeout: Option<Duration>,
    environment: &[(&str, String)],
) -> Result<ProcessResult> {
    let output = OpenOptions::new().create(true).append(true).open(stdout)?;
    let error = OpenOptions::new().create(true).append(true).open(stderr)?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(error)
        .env("LC_ALL", "C")
        .env("TZ", "UTC");
    for (key, value) in environment {
        command.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if timeout.is_some() {
            command.process_group(0);
        }
    }
    let start = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(ProcessResult {
                exit_code: None,
                timed_out: false,
                spawn_error: Some(error.to_string()),
                elapsed_ms: start.elapsed().as_millis(),
                signal: None,
            })
        }
    };
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            timed_out = true;
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .status();
            }
            // This fallback can address only the child held by this process, never unrelated QEMU.
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    Ok(ProcessResult {
        exit_code: status.code(),
        timed_out,
        spawn_error: None,
        elapsed_ms: start.elapsed().as_millis(),
        signal,
    })
}

pub fn logged(
    root: &Path,
    program: &str,
    args: &[String],
    log: &Path,
    timeout: Duration,
    environment: &[(&str, String)],
) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(
        file,
        "\nARGV {}",
        serde_json::to_string(
            &std::iter::once(program.to_owned())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
        )?
    )?;
    drop(file);
    let result = process(root, program, args, log, log, Some(timeout), environment)?;
    if result.spawn_error.is_some() || result.timed_out || result.exit_code != Some(0) {
        return Err(format!("{program} failed: {result:?}; log {}", log.display()).into());
    }
    Ok(())
}

pub fn file_hash_or_null(path: &Path) -> Result<Value> {
    if path.is_file() {
        Ok(json!(hash_file(path)?))
    } else {
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[cfg(unix)]
    #[test]
    fn timeout_is_distinct_from_signal_and_spawn_failure() {
        let base = std::env::temp_dir().join(format!("nanox-harness-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let r = process(
            &base,
            "sleep",
            &["2".into()],
            &base.join("out"),
            &base.join("err"),
            Some(Duration::from_millis(30)),
            &[],
        )
        .unwrap();
        assert!(r.timed_out);
        assert_ne!(r.exit_code, Some(0));
        let r = process(
            &base,
            "nanox-nonexistent-command",
            &[],
            &base.join("out"),
            &base.join("err"),
            Some(Duration::from_secs(1)),
            &[],
        )
        .unwrap();
        assert!(r.spawn_error.is_some());
        assert!(!r.timed_out);
        fs::remove_file(base.join("out")).unwrap();
        fs::remove_file(base.join("err")).unwrap();
        fs::remove_dir(base).unwrap();
    }
}
