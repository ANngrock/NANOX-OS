//! QEMU fw_cfg over I/O ports (selector 0x510, data 0x511, DMA 0x514): the
//! runner hands the candidate kernel ELF (and for the `linux` case a bzImage
//! and an initramfs) to the probe as fw_cfg files, so the probe needs no
//! UEFI file protocols. QEMU only; on hardware they would come from the ESP.

use crate::hw;

const SELECTOR: u16 = 0x510;
const DATA: u16 = 0x511;
/// The DMA address register: big-endian, high half at 0x514, low half at
/// 0x518; writing the low half starts the transfer.
const DMA_HIGH: u16 = 0x514;
const DMA_LOW: u16 = 0x518;
const SIGNATURE: u16 = 0x0000;
const ID: u16 = 0x0001;
const FILE_DIR: u16 = 0x0019;
/// FW_CFG_ID bit: the DMA interface is present.
const ID_DMA: u32 = 1 << 1;
/// FWCfgDmaAccess control bits.
const DMA_ERROR: u32 = 1 << 0;
const DMA_READ: u32 = 1 << 1;
const DMA_SELECT: u32 = 1 << 3;

fn select(key: u16) {
    hw::outw(SELECTOR, key);
}

fn read(out: &mut [u8]) {
    for b in out {
        *b = hw::inb(DATA);
    }
}

/// A fw_cfg file: its selector key and size.
#[derive(Clone, Copy)]
pub struct File {
    key: u16,
    pub size: usize,
}

/// Looks up fw_cfg file `name`.
pub fn find(name: &str) -> Result<File, &'static str> {
    select(SIGNATURE);
    let mut sig = [0u8; 4];
    read(&mut sig);
    if &sig != b"QEMU" {
        return Err("no-fw-cfg");
    }
    select(FILE_DIR);
    let mut n = [0u8; 4];
    read(&mut n);
    for _ in 0..u32::from_be_bytes(n) {
        // struct FWCfgFile: be32 size, be16 select, u16 reserved, name[56].
        let mut e = [0u8; 64];
        read(&mut e);
        let size = u32::from_be_bytes([e[0], e[1], e[2], e[3]]) as usize;
        let key = u16::from_be_bytes([e[4], e[5]]);
        let raw = &e[8..];
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        if &raw[..len] == name.as_bytes() {
            return Ok(File { key, size });
        }
    }
    Err("file-not-found")
}

/// FWCfgDmaAccess: every field big-endian.
#[repr(C, align(16))]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

/// Reads the whole of `file` into `buf` (at least `file.size` bytes), by DMA
/// when the device offers it (a 16 MB kernel byte by byte through the data
/// port takes long under TCG), else through the data port.
pub fn read_into(file: File, buf: &mut [u8]) -> Result<&[u8], &'static str> {
    if file.size > buf.len() {
        return Err("file-too-large");
    }
    let out = &mut buf[..file.size];
    select(ID);
    let mut id = [0u8; 4];
    read(&mut id);
    if u32::from_le_bytes(id) & ID_DMA == 0 {
        select(file.key);
        read(out);
        return Ok(out);
    }
    let mut dma = DmaAccess {
        control: (u32::from(file.key) << 16 | DMA_SELECT | DMA_READ).to_be(),
        length: (out.len() as u32).to_be(),
        address: (out.as_mut_ptr() as u64).to_be(),
    };
    let at = (&raw mut dma) as u64;
    hw::outl(DMA_HIGH, ((at >> 32) as u32).to_be());
    hw::outl(DMA_LOW, (at as u32).to_be());
    // QEMU completes the transfer during the OUT; the device clears the
    // control word (or leaves the error bit).
    // SAFETY: a local the device wrote through its physical address
    // (identity-mapped); read back volatile so the store is not assumed.
    let control = u32::from_be(unsafe { (&raw const dma.control).read_volatile() });
    if control & DMA_ERROR != 0 || control != 0 {
        return Err("dma-error");
    }
    Ok(out)
}

/// Reads fw_cfg file `name` into `buf`.
pub fn read_file<'b>(name: &str, buf: &'b mut [u8]) -> Result<&'b [u8], &'static str> {
    let file = find(name)?;
    read_into(file, buf)
}
