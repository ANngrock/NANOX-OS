use crate::{
    record::{self, logged},
    Options, Result, Scenario,
};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

const DISK_SIZE: u64 = 96 * 1024 * 1024;
const ESP_OFFSET: u64 = 1024 * 1024;
const ESP_SIZE: u64 = 94 * 1024 * 1024;

fn allocate_file(path: &Path, size: u64) -> Result<File> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(format!("refusing non-regular image output {}", path.display()).into());
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    file.set_len(size)?;
    Ok(file)
}

pub fn create(
    options: &Options,
    scenario: Scenario,
    disk_path: &Path,
    work: &Path,
    log: &Path,
) -> Result<()> {
    fs::create_dir_all(work)?;
    let esp_path = work.join("esp.fat");
    allocate_file(disk_path, DISK_SIZE)?;
    allocate_file(&esp_path, ESP_SIZE)?;
    let args = vec![
        "--clear".into(),
        "--disk-guid=4e414e4f-582d-4000-8000-000000000001".into(),
        "--new=1:2048:194559".into(),
        "--typecode=1:ef00".into(),
        "--partition-guid=1:4e414e4f-582d-4000-8000-000000000002".into(),
        "--change-name=1:NANOX-ESP".into(),
        record::string_path(disk_path)?,
    ];
    logged(
        &options.root,
        "sgdisk",
        &args,
        log,
        options.build_timeout,
        &[],
    )?;
    logged(
        &options.root,
        "sgdisk",
        &["--verify".into(), record::string_path(disk_path)?],
        log,
        options.build_timeout,
        &[],
    )?;
    logged(
        &options.root,
        "mkfs.fat",
        &[
            "--invariant",
            "-F",
            "32",
            "-s",
            "1",
            "-S",
            "512",
            "-i",
            "4E414E4F",
            "-n",
            "NANOX",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([record::string_path(&esp_path)?])
        .collect::<Vec<_>>(),
        log,
        options.build_timeout,
        &[],
    )?;
    for dir in ["::/EFI", "::/EFI/BOOT", "::/NANOX"] {
        logged(
            &options.root,
            "mmd",
            &["-i".into(), record::string_path(&esp_path)?, dir.into()],
            log,
            options.build_timeout,
            &[],
        )?;
    }
    let loader = options.root.join("out/BOOTX64.EFI");
    copy_fat(options, &esp_path, &loader, "::/EFI/BOOT/BOOTX64.EFI", log)?;
    let mut config = Vec::from(*b"NXCFG001");
    config.extend_from_slice(&1_u64.to_le_bytes());
    config.extend_from_slice(&scenario.epoch().to_le_bytes());
    let config_path = work.join("BOOT.CFG");
    fs::write(&config_path, &config)?;
    copy_fat(options, &esp_path, &config_path, "::/NANOX/BOOT.CFG", log)?;
    if scenario != Scenario::MissingKernel {
        let mut kernel = fs::read(options.root.join("out/KERNEL.ELF"))?;
        mutate_kernel(&mut kernel, scenario)?;
        let kernel_path = work.join("KERNEL.ELF");
        fs::write(&kernel_path, kernel)?;
        copy_fat(options, &esp_path, &kernel_path, "::/NANOX/KERNEL.ELF", log)?;
    }
    normalize_fat_timestamps(&esp_path)?;
    let mut disk = OpenOptions::new().write(true).open(disk_path)?;
    disk.seek(SeekFrom::Start(ESP_OFFSET))?;
    let copied = std::io::copy(&mut File::open(&esp_path)?, &mut disk)?;
    if copied != ESP_SIZE {
        return Err("ESP changed size during packaging".into());
    }
    disk.sync_all()?;
    Ok(())
}

fn copy_fat(options: &Options, esp: &Path, source: &Path, target: &str, log: &Path) -> Result<()> {
    logged(
        &options.root,
        "mcopy",
        &[
            "-o".into(),
            "-i".into(),
            record::string_path(esp)?,
            record::string_path(source)?,
            target.into(),
        ],
        log,
        options.build_timeout,
        &[],
    )
}

fn mutate_kernel(bytes: &mut Vec<u8>, scenario: Scenario) -> Result<()> {
    match scenario {
        Scenario::TruncatedElf => bytes.truncate(32),
        Scenario::BadSegments => {
            if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" {
                return Err("built kernel is not ELF64".into());
            }
            let offset = usize::try_from(u64::from_le_bytes(bytes[32..40].try_into()?))?;
            let stride = usize::from(u16::from_le_bytes(bytes[54..56].try_into()?));
            let count = usize::from(u16::from_le_bytes(bytes[56..58].try_into()?));
            if stride < 56 {
                return Err("kernel program-header stride is too short".into());
            }
            let mut mutated = false;
            for index in 0..count {
                let start = offset
                    .checked_add(index.checked_mul(stride).ok_or("program-header overflow")?)
                    .ok_or("program-header overflow")?;
                let end = start.checked_add(56).ok_or("program-header overflow")?;
                let header = bytes
                    .get_mut(start..end)
                    .ok_or("program-header outside kernel")?;
                if u32::from_le_bytes(header[..4].try_into()?) == 1 {
                    let mem_size = u64::from_le_bytes(header[40..48].try_into()?);
                    header[32..40].copy_from_slice(
                        &mem_size
                            .checked_add(1)
                            .ok_or("segment-size overflow")?
                            .to_le_bytes(),
                    );
                    mutated = true;
                    break;
                }
            }
            if !mutated {
                return Err("kernel has no load segment to corrupt".into());
            }
        }
        _ => (),
    }
    Ok(())
}

/// FAT32 timestamps are input metadata, not guest logs. Normalizing directory entries after
/// mtools removes wall-clock timestamps from both files and parent directories. LFN entries
/// are untouched. Every cluster read is bounded by the BPB and filesystem length.
fn normalize_fat_timestamps(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut bpb = [0_u8; 512];
    file.read_exact(&mut bpb)?;
    let u16_at = |i| u16::from_le_bytes([bpb[i], bpb[i + 1]]) as u64;
    let u32_at = |i| u32::from_le_bytes([bpb[i], bpb[i + 1], bpb[i + 2], bpb[i + 3]]) as u64;
    let sector = u16_at(11);
    let per_cluster = u64::from(bpb[13]);
    let reserved = u16_at(14);
    let fats = u64::from(bpb[16]);
    let fat_sectors = u32_at(36);
    let root = u32_at(44);
    if sector != 512
        || !per_cluster.is_power_of_two()
        || fats == 0
        || fat_sectors == 0
        || u16_at(17) != 0
    {
        return Err("unexpected FAT32 BPB".into());
    }
    let total = file.metadata()?.len();
    let data_start = (reserved + fats * fat_sectors)
        .checked_mul(sector)
        .ok_or("FAT offset overflow")?;
    let cluster_size = per_cluster.checked_mul(sector).ok_or("FAT size overflow")?;
    let max_cluster = total
        .checked_sub(data_start)
        .ok_or("FAT data outside image")?
        / cluster_size
        + 2;
    let mut directories = vec![root];
    let mut seen = HashSet::new();
    let date = (((2026 - 1980) << 9) | (9 << 5) | 22) as u16;
    while let Some(mut cluster) = directories.pop() {
        loop {
            if cluster < 2 || cluster >= max_cluster || !seen.insert(cluster) {
                return Err("invalid, cross-linked, or cyclic FAT directory chain".into());
            }
            let offset = data_start + (cluster - 2) * cluster_size;
            let mut block = vec![0; usize::try_from(cluster_size)?];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut block)?;
            let mut terminated = false;
            for entry in block.chunks_exact_mut(32) {
                if entry[0] == 0 {
                    terminated = true;
                    break;
                }
                if entry[0] == 0xe5 || entry[11] == 0x0f {
                    continue;
                }
                if entry[11] & 0x10 != 0 && entry[0] != b'.' {
                    let next = (u32::from(u16::from_le_bytes([entry[20], entry[21]])) << 16)
                        | u32::from(u16::from_le_bytes([entry[26], entry[27]]));
                    directories.push(u64::from(next));
                }
                entry[13] = 0;
                entry[14..16].fill(0);
                entry[16..18].copy_from_slice(&date.to_le_bytes());
                entry[18..20].copy_from_slice(&date.to_le_bytes());
                entry[22..24].fill(0);
                entry[24..26].copy_from_slice(&date.to_le_bytes());
            }
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&block)?;
            if terminated {
                break;
            }
            let fat_offset = reserved * sector + cluster * 4;
            if fat_offset + 4 > (reserved + fat_sectors) * sector {
                return Err("FAT chain outside table".into());
            }
            file.seek(SeekFrom::Start(fat_offset))?;
            let mut next = [0_u8; 4];
            file.read_exact(&mut next)?;
            cluster = u64::from(u32::from_le_bytes(next) & 0x0fff_ffff);
            if cluster >= 0x0fff_fff8 {
                break;
            }
        }
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_segment_cannot_overflow_or_read_outside_input() {
        let mut truncated = b"\x7fELF".to_vec();
        assert!(mutate_kernel(&mut truncated, Scenario::BadSegments).is_err());
        let mut elf = vec![0_u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1_u16.to_le_bytes());
        assert!(mutate_kernel(&mut elf, Scenario::BadSegments).is_err());
    }
}
