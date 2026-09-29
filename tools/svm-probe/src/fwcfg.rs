//! QEMU fw_cfg over I/O ports (selector 0x510, data 0x511): the runner hands
//! the candidate kernel ELF to the probe as a fw_cfg file, so the probe
//! needs no UEFI file protocols. QEMU only; on hardware the ELF would come
//! from the ESP.

use crate::hw;

const SELECTOR: u16 = 0x510;
const DATA: u16 = 0x511;
const SIGNATURE: u16 = 0x0000;
const FILE_DIR: u16 = 0x0019;

fn select(key: u16) {
    hw::outw(SELECTOR, key);
}

fn read(out: &mut [u8]) {
    for b in out {
        *b = hw::inb(DATA);
    }
}

/// Reads fw_cfg file `name` into `buf`.
pub fn read_file<'b>(name: &str, buf: &'b mut [u8]) -> Result<&'b [u8], &'static str> {
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
            if size > buf.len() {
                return Err("file-too-large");
            }
            select(key);
            read(&mut buf[..size]);
            return Ok(&buf[..size]);
        }
    }
    Err("file-not-found")
}
