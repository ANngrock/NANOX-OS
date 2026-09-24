use crate::{
    build, image,
    record::{self, ProcessResult},
    Options, Result, Scenario,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, fs, path::Path};

const CPU: &str = "qemu64,+nx,-rdrand,-rdseed";
const RTC: &str = "2026-09-22T00:00:00";

pub fn machine() -> String {
    env::var("NANOX_QEMU_MACHINE").unwrap_or_else(|_| "pc-q35-9.2".into())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunResult {
    pub run_id: String,
    pub verdict: String,
    pub reason: String,
    pub expectation_met: bool,
}

fn contains(bytes: &[u8], marker: &[u8]) -> bool {
    bytes.windows(marker.len()).any(|part| part == marker)
}

/// A process status alone can never produce PASS. Error markers take precedence.
fn classify(serial: &[u8], process: &ProcessResult) -> (&'static str, &'static str) {
    if process.spawn_error.is_some() {
        return ("HARNESS_ERROR", "qemu_spawn_failed");
    }
    if contains(serial, b"NANOX:KERNEL:PANIC") || contains(serial, b"NANOX:LOADER:PANIC") {
        return ("PANIC", "guest_panic");
    }
    if process.timed_out {
        return ("TIMEOUT", "qemu_timeout");
    }
    if process.signal.is_some() {
        return ("HARNESS_ERROR", "qemu_signal");
    }
    if contains(serial, b"NANOX:LOADER:ERROR") {
        return ("LOADER_ERROR", "loader_rejected_input");
    }
    if contains(serial, b"NANOX:KERNEL:BOOTINFO_ERROR") {
        return ("FAIL", "bootinfo_invalid");
    }
    if contains(serial, b"NANOX:TEST:FAIL") {
        return ("FAIL", "kernel_test_failure");
    }
    if contains(serial, b"PANIC") || contains(serial, b"FAIL") {
        return ("FAIL", "guest_error_marker");
    }
    if process.exit_code == Some(33)
        && contains(serial, b"NANOX:LOADER:ENTER")
        && contains(serial, b"NANOX:LOADER:EXIT_BOOT_SERVICES")
        && contains(serial, b"NANOX:KERNEL:ENTER")
        && contains(serial, b"NANOX:KERNEL:BOOTINFO_VALIDATED")
        && contains(serial, b"NANOX:TEST:PASS")
    {
        return ("PASS", "markers_and_debug_exit");
    }
    ("HARNESS_ERROR", "unexpected_qemu_exit_or_missing_markers")
}

fn expected(scenario: Scenario, verdict: &str, process: &ProcessResult, serial: &[u8]) -> bool {
    let kernel = contains(serial, b"NANOX:KERNEL:ENTER");
    let pass = contains(serial, b"NANOX:TEST:PASS");
    match scenario {
        Scenario::Pass => verdict == "PASS",
        Scenario::MissingKernel | Scenario::TruncatedElf | Scenario::BadSegments => {
            verdict == "LOADER_ERROR" && process.exit_code == Some(35) && !kernel && !pass
        }
        Scenario::KernelFail => {
            verdict == "FAIL" && process.exit_code == Some(35) && kernel && !pass
        }
        Scenario::KernelPanic => {
            verdict == "PANIC"
                && process.exit_code == Some(35)
                && !process.timed_out
                && kernel
                && !pass
        }
        Scenario::KernelHang => {
            verdict == "TIMEOUT" && kernel && contains(serial, b"NANOX:TEST:HANG") && !pass
        }
    }
}

fn profile() -> Result<Value> {
    let machine = machine();
    if !machine.starts_with("pc-q35-")
        || !machine
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err("NANOX_QEMU_MACHINE must be a versioned pc-q35 name".into());
    }
    Ok(
        json!({"machine":machine,"cpu":CPU,"vcpu":1,"ram_mib":512,"accelerator":"tcg,thread=single","rtc_base":RTC,"network":"none","boot_controller":"q35 onboard AHCI, ide-hd bus=ide.0","serial":"COM1 polling","icount_shift":3,"firmware_mode":"pflash0 CODE readonly; pflash1 VARS fresh qcow2 overlay; synchronous pflash without blkreplay; disk blkreplay in record/replay"}),
    )
}

fn snapshot_inputs(
    options: &Options,
    scenario: Scenario,
    dir: &Path,
    mode: &str,
    replay_from: Option<&Path>,
) -> Result<Value> {
    if let Some(original) = replay_from {
        let inputs = record::read_json(&original.join("inputs.json"))?;
        verify_saved(original, &inputs)?;
        for file in [
            "initial.img",
            "initial-code.fd",
            "initial-vars.fd",
            "BOOTX64.EFI",
            "KERNEL.ELF",
            "BOOT.CFG",
        ] {
            fs::copy(original.join(file), dir.join(file))?;
        }
        if mode == "replay-play" {
            fs::copy(original.join("replay.bin"), dir.join("replay.bin"))?;
        }
        return Ok(inputs);
    }
    let out = options.root.join("out");
    let source = record::source(&options.root)?;
    record::write_json(&dir.join("source-manifest.json"), &source)?;
    fs::copy(
        build::firmware("NANOX_OVMF_CODE")?,
        dir.join("initial-code.fd"),
    )?;
    fs::copy(
        build::firmware("NANOX_OVMF_VARS")?,
        dir.join("initial-vars.fd"),
    )?;
    fs::copy(out.join("BOOTX64.EFI"), dir.join("BOOTX64.EFI"))?;
    fs::copy(out.join("KERNEL.ELF"), dir.join("KERNEL.ELF"))?;
    image::create(
        options,
        scenario,
        &dir.join("initial.img"),
        &dir.join("image-work"),
        &dir.join("build.log"),
    )?;
    fs::copy(dir.join("image-work/BOOT.CFG"), dir.join("BOOT.CFG"))?;
    let mut hashes = serde_json::Map::new();
    for file in [
        "initial.img",
        "initial-code.fd",
        "initial-vars.fd",
        "BOOTX64.EFI",
        "KERNEL.ELF",
        "BOOT.CFG",
    ] {
        hashes.insert(file.into(), json!(record::hash_file(&dir.join(file))?));
    }
    Ok(
        json!({"schema_version":1,"scenario":scenario.name(),"source":source,"tools":build::tools(&options.root)?,"machine":profile()?,"files":hashes,"scenario_input_sha256":record::hash_file(&dir.join("BOOT.CFG"))?,"packaged_kernel_sha256":record::file_hash_or_null(&dir.join("image-work/KERNEL.ELF"))?}),
    )
}

fn verify_saved(dir: &Path, inputs: &Value) -> Result<()> {
    for file in [
        "initial.img",
        "initial-code.fd",
        "initial-vars.fd",
        "BOOTX64.EFI",
        "KERNEL.ELF",
        "BOOT.CFG",
    ] {
        if inputs["files"][file] != json!(record::hash_file(&dir.join(file))?) {
            return Err(format!("saved input hash mismatch: {file}").into());
        }
    }
    Ok(())
}

fn compatible(recorded: &Value, current: &Value) -> Result<()> {
    for key in [
        "source_manifest_sha256",
        "efi_sha256",
        "elf_sha256",
        "qemu",
        "qemu_sha256",
        "machine",
        "firmware_code_sha256",
        "firmware_vars_sha256",
        "flake_lock_sha256",
        "cargo_lock_sha256",
    ] {
        if recorded[key].is_null() || recorded[key] != current[key] {
            return Err(format!("replay input mismatch: {key}").into());
        }
    }
    Ok(())
}

fn recorded_compatibility(inputs: &Value) -> Value {
    json!({"source_manifest_sha256":inputs["source"]["manifest_sha256"],"efi_sha256":inputs["files"]["BOOTX64.EFI"],"elf_sha256":inputs["files"]["KERNEL.ELF"],"qemu":inputs["tools"]["qemu"],"qemu_sha256":inputs["tools"]["qemu_sha256"],"machine":inputs["machine"],"firmware_code_sha256":inputs["files"]["initial-code.fd"],"firmware_vars_sha256":inputs["files"]["initial-vars.fd"],"flake_lock_sha256":inputs["tools"]["flake_lock_sha256"],"cargo_lock_sha256":inputs["tools"]["cargo_lock_sha256"]})
}

fn verify_current(options: &Options, inputs: &Value) -> Result<()> {
    let source = record::source(&options.root)?;
    let tools = build::tools(&options.root)?;
    let current = json!({"source_manifest_sha256":source["manifest_sha256"],"efi_sha256":record::hash_file(&options.root.join("out/BOOTX64.EFI"))?,"elf_sha256":record::hash_file(&options.root.join("out/KERNEL.ELF"))?,"qemu":tools["qemu"],"qemu_sha256":tools["qemu_sha256"],"machine":profile()?,"firmware_code_sha256":record::hash_file(&build::firmware("NANOX_OVMF_CODE")?)?,"firmware_vars_sha256":record::hash_file(&build::firmware("NANOX_OVMF_VARS")?)?,"flake_lock_sha256":tools["flake_lock_sha256"],"cargo_lock_sha256":tools["cargo_lock_sha256"]});
    compatible(&recorded_compatibility(inputs), &current)
}

fn qemu_args(options: &Options, dir: &Path, mode: &str) -> Result<Vec<String>> {
    let replay = mode == "replay-record" || mode == "replay-play";
    let mut args = vec![
        "-machine".into(),
        format!("{},pflash0=code,pflash1=vars", machine()),
        "-accel".into(),
        "tcg,thread=single".into(),
        "-cpu".into(),
        CPU.into(),
        "-smp".into(),
        "1".into(),
        "-m".into(),
        "512M".into(),
        "-nodefaults".into(),
        "-display".into(),
        "none".into(),
        "-monitor".into(),
        "none".into(),
        "-net".into(),
        "none".into(),
        "-rtc".into(),
        format!("base={RTC},clock=vm"),
        "-no-reboot".into(),
        "-serial".into(),
        format!("file:{}", record::string_path(&dir.join("serial.bin"))?),
        "-device".into(),
        "isa-debug-exit,iobase=0xf4,iosize=4".into(),
    ];
    for (name, file, readonly) in [
        ("code", "initial-code.fd", true),
        ("vars", "vars.qcow2", false),
        ("disk", "disk.qcow2", false),
    ] {
        if !readonly {
            let initial = if name == "vars" {
                "initial-vars.fd"
            } else {
                "initial.img"
            };
            record::logged(
                &options.root,
                "qemu-img",
                &[
                    "create".into(),
                    "-f".into(),
                    "qcow2".into(),
                    "-F".into(),
                    "raw".into(),
                    "-b".into(),
                    record::string_path(&dir.join(initial))?,
                    record::string_path(&dir.join(file))?,
                ],
                &dir.join("build.log"),
                options.build_timeout,
                &[],
            )?;
        }
        // pflash realizes/loads before VM execution; filtering that synchronous
        // path deadlocks QEMU 9.2.4 before replay starts. Its exact initial and
        // final bytes are checked separately (docs/M0-REPLAY.md). AHCI disk
        // asynchronous completions use the supported blkreplay filter.
        let filter = replay && name == "disk";
        let node = if filter {
            format!("{name}-base")
        } else {
            name.to_owned()
        };
        args.push("-blockdev".into());
        args.push(serde_json::to_string(&json!({"driver":if readonly {"raw"} else {"qcow2"},"node-name":node,"read-only":readonly,"file":{"driver":"file","filename":record::string_path(&dir.join(file))?}}))?);
        if filter {
            args.push("-blockdev".into());
            args.push(serde_json::to_string(
                &json!({"driver":"blkreplay","node-name":name,"image":node,"read-only":readonly}),
            )?);
        }
    }
    args.extend(["-device".into(), "ide-hd,drive=disk,bus=ide.0".into()]);
    if replay {
        args.extend([
            "-icount".into(),
            format!(
                "shift=3,rr={},rrfile={}",
                if mode == "replay-record" {
                    "record"
                } else {
                    "replay"
                },
                record::string_path(&dir.join("replay.bin"))?
            ),
        ]);
    }
    if mode == "debug" {
        args.extend(["-S".into(), "-gdb".into(), "tcp:127.0.0.1:1234".into()]);
    }
    Ok(args)
}

pub fn execute(
    options: &Options,
    scenario: Scenario,
    mode: &str,
    replay_from: Option<&Path>,
) -> Result<RunResult> {
    let (run_id, dir) = record::new_run(&options.root, &format!("{}-{mode}", scenario.name()))?;
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    if options.root.join("out/build.log").is_file() {
        fs::copy(options.root.join("out/build.log"), dir.join("build.log"))?;
    } else {
        fs::write(dir.join("build.log"), b"")?;
    }
    for file in ["serial.bin", "qemu-stderr.txt", "qemu-stdout.txt"] {
        fs::write(dir.join(file), b"")?;
    }
    // Save a record before setup too; a packaging/firmware error remains a harness failure.
    let mut record = json!({"schema_version":1,"run_id":run_id,"scenario":scenario.name(),"mode":mode,"started_unix_ms":started,"result":{"verdict":"HARNESS_ERROR","reason":"setup_incomplete"}});
    record::write_json(&dir.join("record.json"), &record)?;
    let setup = (|| -> Result<(Value, Vec<String>)> {
        let inputs = snapshot_inputs(options, scenario, &dir, mode, replay_from)?;
        record::write_json(&dir.join("inputs.json"), &inputs)?;
        if let Some(original) = replay_from {
            if original.join("source-manifest.json").is_file() {
                fs::copy(
                    original.join("source-manifest.json"),
                    dir.join("source-manifest.json"),
                )?;
            }
        }
        let args = qemu_args(options, &dir, mode)?;
        Ok((inputs, args))
    })();
    let (inputs, args) = match setup {
        Ok(value) => value,
        Err(error) => {
            record["result"] = json!({"verdict":"HARNESS_ERROR","reason":"setup_failed","detail":error.to_string()});
            record::write_json(&dir.join("record.json"), &record)?;
            return Err(format!("run {run_id} setup failed: {error}").into());
        }
    };
    if mode == "debug" {
        println!(
            "GDB: file {}\ntarget remote 127.0.0.1:1234\nset breakpoint auto-hw off\nbreak kernel_main\ncontinue",
            dir.join("KERNEL.ELF").display()
        );
    }
    record["source"] = inputs["source"].clone();
    record["tools"] = inputs["tools"].clone();
    record["machine"] = inputs["machine"].clone();
    record["inputs"] = inputs["files"].clone();
    record["invocation"] = json!({"argv":std::iter::once("qemu-system-x86_64".to_owned()).chain(args.iter().cloned()).collect::<Vec<_>>(),"environment":{"LC_ALL":"C","TZ":"UTC","NANOX_QEMU_MACHINE":machine()},"timeout_seconds":if mode=="debug" {Value::Null} else {json!(options.timeout.as_secs())},"build_timeout_seconds":options.build_timeout.as_secs()});
    record::write_json(&dir.join("record.json"), &record)?;
    let process = record::process(
        &options.root,
        "qemu-system-x86_64",
        &args,
        &dir.join("qemu-stdout.txt"),
        &dir.join("qemu-stderr.txt"),
        if mode == "debug" {
            None
        } else {
            Some(options.timeout)
        },
        &[],
    )?;
    let serial = fs::read(dir.join("serial.bin"))?;
    fs::write(
        dir.join("serial.txt"),
        String::from_utf8_lossy(&serial).as_bytes(),
    )?;
    let (verdict, reason) = classify(&serial, &process);
    let mut result = RunResult {
        run_id,
        verdict: verdict.into(),
        reason: reason.into(),
        expectation_met: expected(scenario, verdict, &process, &serial),
    };
    if let Err(error) = verify_saved(&dir, &inputs) {
        result.verdict = "HARNESS_ERROR".into();
        result.reason = error.to_string();
        result.expectation_met = false;
    }
    record["outputs"] = json!({"serial_sha256":record::hash_bytes(&serial),"serial_path":"serial.bin","serial_text_path":"serial.txt","qemu_stderr_path":"qemu-stderr.txt","qemu_stderr_sha256":record::hash_file(&dir.join("qemu-stderr.txt"))?,"qemu_stdout_path":"qemu-stdout.txt","build_log_path":"build.log","replay_sha256":record::file_hash_or_null(&dir.join("replay.bin"))?});
    record["result"] = json!({"verdict":result.verdict,"reason":result.reason,"expectation_met":result.expectation_met,"process":process});
    if let Some(original) = replay_from {
        record["replay_from"] = json!(original.file_name().and_then(|s| s.to_str()));
    }
    record::write_json(&dir.join("record.json"), &record)?;
    Ok(result)
}

fn replay_pair(options: &Options, record_id: &str) -> Result<()> {
    crate::validate_run_id(record_id)?;
    let original = options.root.join("out/runs").join(record_id);
    let original_record = record::read_json(&original.join("record.json"))?;
    if original_record["mode"] != "replay-record"
        || original_record["result"]["expectation_met"] != true
    {
        return Err("replay requires a replay-record run with its expected result".into());
    }
    let inputs = record::read_json(&original.join("inputs.json"))?;
    let scenario = Scenario::parse(
        inputs["scenario"]
            .as_str()
            .ok_or("missing recorded scenario")?,
    )?;
    if !matches!(scenario, Scenario::Pass | Scenario::KernelFail) {
        return Err("replay supports the PASS and kernel-fail M0 scenarios".into());
    }
    verify_saved(&original, &inputs)?;
    verify_current(options, &inputs)?;
    if original_record["outputs"]["replay_sha256"]
        != json!(record::hash_file(&original.join("replay.bin"))?)
    {
        return Err("replay trace hash mismatch".into());
    }
    let result = execute(options, scenario, "replay-play", Some(&original))?;
    let replay = options.root.join("out/runs").join(&result.run_id);
    let equal = fs::read(original.join("serial.bin"))? == fs::read(replay.join("serial.bin"))?;
    let same_verdict =
        result.verdict == original_record["result"]["verdict"].as_str().unwrap_or("");
    let mut final_blocks = serde_json::Map::new();
    let mut blocks_equal = true;
    for name in ["vars", "disk"] {
        let mut hashes = Vec::new();
        for dir in [&original, &replay] {
            let output = dir.join(format!("final-{name}.raw"));
            record::logged(
                &options.root,
                "qemu-img",
                &[
                    "convert".into(),
                    "-O".into(),
                    "raw".into(),
                    record::string_path(&dir.join(format!("{name}.qcow2")))?,
                    record::string_path(&output)?,
                ],
                &dir.join("build.log"),
                options.build_timeout,
                &[],
            )?;
            hashes.push(record::hash_file(&output)?);
        }
        blocks_equal &= hashes[0] == hashes[1];
        final_blocks.insert(name.into(),json!({"record_sha256":hashes[0],"replay_sha256":hashes[1],"equal":hashes[0]==hashes[1]}));
    }
    record::write_json(
        &replay.join("replay-comparison.json"),
        &json!({"record_run":record_id,"replay_run":result.run_id,"raw_serial_equal":equal,"guest_verdict_equal":same_verdict,"final_blocks":final_blocks,"final_blocks_equal":blocks_equal,"passed":equal && same_verdict && blocks_equal && result.expectation_met}),
    )?;
    // Corrupt a real saved ELF in a separate input set; never touch the build's
    // output or the successful record. The production preflight reads it and
    // rejects it before any QEMU process can be started.
    let mismatch = replay.join("mismatched-inputs");
    fs::create_dir_all(&mismatch)?;
    for file in [
        "initial.img",
        "initial-code.fd",
        "initial-vars.fd",
        "BOOTX64.EFI",
        "BOOT.CFG",
    ] {
        fs::hard_link(original.join(file), mismatch.join(file))?;
    }
    let mut altered = fs::read(original.join("KERNEL.ELF"))?;
    if altered.is_empty() {
        return Err("recorded ELF is empty".into());
    }
    altered[0] ^= 1;
    fs::write(mismatch.join("KERNEL.ELF"), &altered)?;
    let rejection = verify_saved(&mismatch, &inputs)
        .err()
        .map(|e| e.to_string());
    let rejected = rejection.as_deref() == Some("saved input hash mismatch: KERNEL.ELF");
    record::write_json(
        &replay.join("replay-rejection.json"),
        &json!({"case":"different-elf-hash","altered_elf_sha256":record::hash_bytes(&altered),"qemu_started":false,"rejected":rejected,"reason":rejection}),
    )?;
    println!("replay {}: raw_serial_equal={equal}, guest_verdict_equal={same_verdict}, incompatible_elf_rejected={rejected}",result.run_id);
    if equal && same_verdict && blocks_equal && result.expectation_met && rejected {
        Ok(())
    } else {
        Err("record/replay comparison failed; inspect replay-comparison.json and raw logs".into())
    }
}

pub fn replay_existing(options: &Options) -> Result<()> {
    replay_pair(
        options,
        options
            .run_id
            .as_deref()
            .ok_or("replay requires --run ID")?,
    )
}
pub fn debug(options: &Options) -> Result<()> {
    let id = options.run_id.as_deref().ok_or("debug requires --run ID")?;
    let original = options.root.join("out/runs").join(id);
    let inputs = record::read_json(&original.join("inputs.json"))?;
    verify_saved(&original, &inputs)?;
    verify_current(options, &inputs)?;
    let scenario = Scenario::parse(
        inputs["scenario"]
            .as_str()
            .ok_or("recorded scenario is missing")?,
    )?;
    let result = execute(options, scenario, "debug", Some(&original))?;
    println!("debug run {}: {}", result.run_id, result.verdict);
    Ok(())
}

pub fn test(options: &Options) -> Result<()> {
    let (suite_id, suite_dir) = record::new_run(&options.root, "suite")?;
    record::logged(
        &options.root,
        "cargo",
        &[
            "test".into(),
            "--locked".into(),
            "--package".into(),
            "boot-protocol".into(),
            "--package".into(),
            "xtask".into(),
        ],
        &suite_dir.join("host-tests.log"),
        options.build_timeout,
        &[],
    )?;
    build::ensure(options)?;
    let mut results = Vec::new();
    for scenario in Scenario::ALL {
        match execute(options,scenario,"boot-test",None) {
            Ok(result) => { println!("{}: {} expected={}",scenario.name(),result.verdict,result.expectation_met); results.push(json!(result)); },
            Err(error) => results.push(json!({"scenario":scenario.name(),"verdict":"HARNESS_ERROR","reason":error.to_string(),"expectation_met":false})),
        }
        record::write_json(
            &suite_dir.join("suite.json"),
            &json!({"schema_version":1,"suite_id":suite_id,"results":results,"replay_requested":options.replay}),
        )?;
    }
    let mut replay_error = None;
    if options.replay {
        let replay = (|| -> Result<()> {
            for scenario in [Scenario::Pass, Scenario::KernelFail] {
                let run = execute(options, scenario, "replay-record", None)?;
                if !run.expectation_met {
                    return Err(format!("record failed: {} {}", run.run_id, run.reason).into());
                }
                replay_pair(options, &run.run_id)?;
            }
            Ok(())
        })();
        if let Err(error) = replay {
            replay_error = Some(error.to_string());
        }
    }
    let passed = results.iter().all(|r| r["expectation_met"] == true) && replay_error.is_none();
    record::write_json(
        &suite_dir.join("suite.json"),
        &json!({"schema_version":1,"suite_id":suite_id,"results":results,"replay_requested":options.replay,"replay_error":replay_error,"passed":passed}),
    )?;
    println!("suite {suite_id}: passed={passed}");
    if passed {
        Ok(())
    } else {
        Err(
            "M0 scenario suite failed; inspect out/runs suite.json and individual record.json"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn process(exit: i32) -> ProcessResult {
        ProcessResult {
            exit_code: Some(exit),
            timed_out: false,
            spawn_error: None,
            elapsed_ms: 1,
            signal: None,
        }
    }
    const PASS: &[u8] = b"NANOX:LOADER:ENTER\nNANOX:LOADER:EXIT_BOOT_SERVICES\nNANOX:KERNEL:ENTER\nNANOX:KERNEL:BOOTINFO_VALIDATED\nNANOX:TEST:PASS\n";
    #[test]
    fn exit_without_evidence_is_never_pass() {
        for code in [0, 33, 35] {
            assert_ne!(classify(b"", &process(code)).0, "PASS");
        }
        assert_eq!(classify(PASS, &process(33)).0, "PASS");
        assert_ne!(classify(PASS, &process(0)).0, "PASS");
    }
    #[test]
    fn errors_override_success_markers() {
        let mut serial = PASS.to_vec();
        serial.extend_from_slice(b"NANOX:TEST:FAIL");
        assert_eq!(classify(&serial, &process(33)).0, "FAIL");
        let mut p = process(33);
        p.timed_out = true;
        assert_eq!(classify(PASS, &p).0, "TIMEOUT");
    }
    #[test]
    fn negative_case_requires_no_kernel_execution() {
        let good = b"NANOX:LOADER:ERROR:file_missing\n";
        assert!(expected(
            Scenario::MissingKernel,
            "LOADER_ERROR",
            &process(35),
            good
        ));
        let bad = b"NANOX:LOADER:ERROR:file_missing\nNANOX:KERNEL:ENTER\n";
        assert!(!expected(
            Scenario::MissingKernel,
            "LOADER_ERROR",
            &process(35),
            bad
        ));
    }
    #[test]
    fn replay_gate_rejects_changed_elf_and_missing_provenance() {
        let base = json!({"source_manifest_sha256":"s","efi_sha256":"e","elf_sha256":"k","qemu":"q","qemu_sha256":"qh","machine":"m","firmware_code_sha256":"c","firmware_vars_sha256":"v","flake_lock_sha256":"f","cargo_lock_sha256":"l"});
        assert!(compatible(&base, &base).is_ok());
        let mut changed = base.clone();
        changed["elf_sha256"] = json!("changed");
        assert!(compatible(&base, &changed).is_err());
        assert!(compatible(&json!({}), &json!({})).is_err());
    }
}
