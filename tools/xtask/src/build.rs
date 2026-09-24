use crate::{
    image,
    record::{self, logged},
    Options, Result, Scenario,
};
use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub const LOADER_TARGET: &str = "x86_64-unknown-uefi";
pub const KERNEL_TARGET: &str = "x86_64-unknown-none";
pub fn tools(root: &Path) -> Result<Value> {
    let qemu_path = record::capture(root, "which", &["qemu-system-x86_64"]).ok();
    let qemu_hash = qemu_path
        .as_ref()
        .and_then(|p| record::hash_file(Path::new(p)).ok());
    Ok(
        json!({"rustc":record::version(root,"rustc",&["-vV"]),"cargo":record::version(root,"cargo",&["--version"]),"qemu":record::version(root,"qemu-system-x86_64",&["--version"]),"qemu_path":qemu_path,"qemu_sha256":qemu_hash,"qemu_img":record::version(root,"qemu-img",&["--version"]),"sgdisk":record::version(root,"sgdisk",&["--version"]),"mtools":record::version(root,"mcopy",&["-V"]),"targets":[LOADER_TARGET,KERNEL_TARGET],"profile":"release","guest_dependencies":["boot-protocol (workspace)","core (Rust sysroot)","compiler_builtins (Rust sysroot)"],"flake_lock_sha256":record::file_hash_or_null(&root.join("flake.lock"))?,"cargo_lock_sha256":record::file_hash_or_null(&root.join("Cargo.lock"))?,"rust_toolchain_sha256":record::file_hash_or_null(&root.join("rust-toolchain.toml"))?}),
    )
}

pub fn doctor(options: &Options) -> Result<()> {
    let mut failures = Vec::new();
    if let Err(error) = crate::linux_build_root(&options.root) {
        failures.push(error.to_string());
    }
    let versions = tools(&options.root)?;
    if !versions["qemu"]
        .as_str()
        .unwrap_or("")
        .contains("9.2.4 (nanox-replay-exit-v1)")
    {
        failures.push(
            "QEMU must be pinned 9.2.4 with nanox-replay-exit-v1; enter the Nix shell".into(),
        );
    }
    if !versions["rustc"]
        .as_str()
        .unwrap_or("")
        .starts_with("rustc 1.90.0 ")
    {
        failures.push("Rust must be the pinned 1.90.0 toolchain".into());
    }
    let sysroot = record::capture(&options.root, "rustc", &["--print", "sysroot"])?;
    match env::var("NANOX_PINNED_RUST") {
        Ok(pinned) if sysroot == pinned => (),
        _ => failures.push("rustc sysroot is not NANOX_PINNED_RUST; enter the Nix shell".into()),
    }
    for key in ["rustc", "cargo", "qemu", "qemu_img", "sgdisk", "mtools"] {
        if versions[key]
            .as_str()
            .unwrap_or("")
            .starts_with("UNAVAILABLE")
        {
            failures.push(format!("missing tool {key}"));
        }
    }
    let mut libs = serde_json::Map::new();
    for target in [LOADER_TARGET, KERNEL_TARGET] {
        let dir = record::capture(
            &options.root,
            "rustc",
            &["--print", "target-libdir", "--target", target],
        );
        match dir {
            Ok(dir) => {
                let found = fs::read_dir(&dir)
                    .map(|entries| {
                        entries
                            .filter_map(|e| e.ok())
                            .any(|e| e.file_name().to_string_lossy().starts_with("libcore-"))
                    })
                    .unwrap_or(false);
                if !found {
                    failures.push(format!("target libcore missing: {target}: {dir}"));
                }
                libs.insert(target.into(), json!({"path":dir,"libcore_present":found}));
            }
            Err(error) => failures.push(error.to_string()),
        }
    }
    for variable in ["NANOX_OVMF_CODE", "NANOX_OVMF_VARS"] {
        match env::var(variable) {
            Ok(path) if Path::new(&path).is_file() => (),
            _ => failures.push(format!(
                "{variable} must name the pinned firmware file from nix develop"
            )),
        }
    }
    for file in ["flake.lock", "Cargo.lock", "rust-toolchain.toml"] {
        if !options.root.join(file).is_file() {
            failures.push(format!("missing {file}"));
        }
    }
    let machine = crate::runner::machine();
    match record::capture(&options.root, "qemu-system-x86_64", &["-machine", "help"]) {
        Ok(list)
            if list
                .lines()
                .any(|line| line.split_whitespace().next() == Some(machine.as_str())) =>
        {
            ()
        }
        Ok(_) => failures.push(format!(
            "QEMU does not provide the pinned machine {machine}"
        )),
        Err(error) => failures.push(error.to_string()),
    }
    // mkfs.fat and mmd have no portable successful version switch; check command resolution.
    if cfg!(unix) {
        for program in ["mkfs.fat", "mmd", "touch", "kill"] {
            if record::capture(&options.root, "which", &[program]).is_err() {
                failures.push(format!("missing tool {program}"));
            }
        }
    }
    let report = json!({"schema_version":1,"checkout":options.root,"tools":versions,"target_libraries":libs,"machine":machine,"failures":failures});
    fs::create_dir_all(options.root.join("out"))?;
    record::write_json(&options.root.join("out/doctor.json"), &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if failures.is_empty() {
        Ok(())
    } else {
        Err("doctor found missing or incompatible prerequisites; see out/doctor.json".into())
    }
}

fn compile(options: &Options, target_dir: &Path, destination: &Path, log: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    let environment = [
        ("CARGO_TARGET_DIR", record::string_path(target_dir)?),
        ("SOURCE_DATE_EPOCH", "1790035200".to_owned()),
    ];
    for (package, target, binary, output) in [
        ("nanox-uefi", LOADER_TARGET, "nanox-uefi.efi", "BOOTX64.EFI"),
        ("nanox-kernel", KERNEL_TARGET, "nanox-kernel", "KERNEL.ELF"),
    ] {
        let remap = format!(
            "--remap-path-prefix={}=/nanox",
            record::string_path(&options.root)?
        );
        // Cargo merges CLI rustflags arrays with the per-target config arrays,
        // retaining the kernel script / PE timestamp flags and host isolation.
        let config = format!(
            "target.{target}.rustflags=[{}]",
            serde_json::to_string(&remap)?
        );
        let mut args = [
            "build",
            "--locked",
            "--release",
            "--package",
            package,
            "--target",
            target,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        args.extend(["--config".into(), config]);
        logged(
            &options.root,
            "cargo",
            &args,
            log,
            options.build_timeout,
            &environment,
        )?;
        fs::copy(
            target_dir.join(target).join("release").join(binary),
            destination.join(output),
        )?;
    }
    Ok(())
}

pub fn build(options: &Options) -> Result<()> {
    let out = options.root.join("out");
    fs::create_dir_all(&out)?;
    let log = out.join("build.log");
    fs::write(&log, b"")?;
    let source_before = record::source(&options.root)?;
    compile(options, &options.root.join("target"), &out, &log)?;
    let source_after = record::source(&options.root)?;
    if source_before["manifest_sha256"] != source_after["manifest_sha256"] {
        return Err("source changed while building; repeat after edits stop".into());
    }
    image::create(
        options,
        Scenario::Pass,
        &out.join("nanox.img"),
        &out.join("image-work"),
        &log,
    )?;
    let stamp = json!({"schema_version":1,"source":source_after,"tools":tools(&options.root)?,"efi_sha256":record::hash_file(&out.join("BOOTX64.EFI"))?,"elf_sha256":record::hash_file(&out.join("KERNEL.ELF"))?,"image_sha256":record::hash_file(&out.join("nanox.img"))?});
    record::write_json(&out.join("build-inputs.json"), &stamp)?;
    println!("Built {}", out.join("nanox.img").display());
    Ok(())
}

pub fn ensure(options: &Options) -> Result<()> {
    if !options.no_build {
        return build(options);
    }
    let stamp = record::read_json(&options.root.join("out/build-inputs.json"))?;
    let current = record::source(&options.root)?;
    if current["manifest_sha256"] != stamp["source"]["manifest_sha256"] {
        return Err("source manifest differs from existing build; omit --no-build".into());
    }
    for (file, key) in [("BOOTX64.EFI", "efi_sha256"), ("KERNEL.ELF", "elf_sha256")] {
        if stamp[key] != json!(record::hash_file(&options.root.join("out").join(file))?) {
            return Err(format!("built artifact changed: {file}").into());
        }
    }
    Ok(())
}

pub fn reproduce(options: &Options) -> Result<()> {
    let (id, dir) = record::new_run(&options.root, "reproduce-build")?;
    let source_before = record::source(&options.root)?;
    let mut builds = Vec::new();
    for name in ["a", "b"] {
        let destination = dir.join(name);
        fs::create_dir_all(&destination)?;
        let target = destination.join("target");
        // Both target directories are freshly created and never shared. No user files are deleted.
        compile(
            options,
            &target,
            &destination,
            &destination.join("build.log"),
        )?;
        builds.push(json!({"name":name,"efi_sha256":record::hash_file(&destination.join("BOOTX64.EFI"))?,"elf_sha256":record::hash_file(&destination.join("KERNEL.ELF"))?}));
    }
    let unchanged =
        source_before["manifest_sha256"] == record::source(&options.root)?["manifest_sha256"];
    let equal = unchanged
        && builds[0]["efi_sha256"] == builds[1]["efi_sha256"]
        && builds[0]["elf_sha256"] == builds[1]["elf_sha256"];
    record::write_json(
        &dir.join("record.json"),
        &json!({"schema_version":1,"run_id":id,"mode":"reproduce-build","source":source_before,"tools":tools(&options.root)?,"builds":builds,"source_unchanged":unchanged,"binaries_equal":equal,"disk_image_equality_tested":false}),
    )?;
    println!("reproduce-build {id}: binaries_equal={equal}");
    if equal {
        Ok(())
    } else {
        Err("repeat-build artifact mismatch or concurrent source edit".into())
    }
}

pub fn firmware(variable: &str) -> Result<PathBuf> {
    let path =
        PathBuf::from(env::var(variable).map_err(|_| {
            format!("{variable} is not set; enter the pinned nix develop environment")
        })?);
    if !path.is_absolute() || !path.is_file() {
        return Err(format!("{variable} is not an absolute regular firmware file").into());
    }
    Ok(path)
}
