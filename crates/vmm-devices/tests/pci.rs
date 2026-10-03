//! PCI configuration space: one function's registers, and the bus that finds them.

use vmm_devices::machine::*;
use vmm_devices::pci::*;

fn device() -> Config {
    let mut c = Config::new(0x1AF4, 0x1042, 0x01_0000, 1, (0x1AF4, 0x0002), 1);
    c.define_bar(0, BarKind::Memory(0x4000));
    c.define_bar(2, BarKind::Io(0x40));
    c
}

#[test]
fn identification_registers() {
    let c = device();
    assert_eq!(c.read(VENDOR_ID, 4), 0x1042_1AF4);
    assert_eq!(c.read(VENDOR_ID, 2), 0x1AF4);
    assert_eq!(c.read(DEVICE_ID, 2), 0x1042);
    assert_eq!(
        c.read(REVISION, 4),
        0x0100_0001,
        "class code and revision in one dword"
    );
    assert_eq!(
        c.read(HEADER_TYPE, 1),
        0,
        "a plain single-function type 0 header"
    );
    assert_eq!(c.read(SUBSYSTEM_VENDOR, 4), 0x0002_1AF4);
    assert_eq!(c.read(INTERRUPT_PIN, 1), 1);
    assert_eq!(c.read(CAPABILITIES, 1), 0);
    assert_eq!(c.read(0x100, 4), 0xFFFF_FFFF, "beyond the 256 bytes: ones");
}

#[test]
fn read_only_registers_ignore_writes_and_count_them() {
    let mut c = device();
    c.write(VENDOR_ID, 4, 0);
    c.write(REVISION, 1, 0x77);
    c.write(HEADER_TYPE, 1, 0x80);
    assert_eq!(c.read(VENDOR_ID, 4), 0x1042_1AF4);
    assert_eq!(c.read(HEADER_TYPE, 1), 0);
    assert_eq!(
        c.ignored_writes,
        4 + 1 + 1,
        "four bytes of the vendor/device dword, one revision, one header type"
    );
}

#[test]
fn the_command_register_has_only_the_bits_it_implements() {
    let mut c = device();
    assert_eq!(c.command(), 0);
    c.write(COMMAND, 2, 0xFFFF);
    assert_eq!(
        c.command(),
        CMD_IO | CMD_MEMORY | CMD_BUS_MASTER | (1 << 6) | (1 << 8) | CMD_INTX_DISABLE
    );
    assert_eq!(c.read(COMMAND, 2), u32::from(c.command()));
    c.write(COMMAND, 2, 0);
    assert_eq!(c.command(), 0);
    c.write(COMMAND + 1, 1, 0x04);
    assert_eq!(
        c.command(),
        CMD_INTX_DISABLE,
        "a byte write changes that byte only"
    );
    assert!(!c.intx_enabled());
    c.write(COMMAND, 1, 0x07);
    assert!(c.io_enabled() && c.memory_enabled() && c.bus_master());
}

#[test]
fn status_follows_the_interrupt_and_the_capability_list() {
    let mut c = device();
    assert_eq!(c.read(STATUS, 2), 0);
    c.set_interrupt_status(true);
    assert_eq!(c.read(STATUS, 2), 1 << 3);
    c.write(STATUS, 2, 0xFFFF);
    assert_eq!(c.read(STATUS, 2), 1 << 3, "status is read-only here");
    c.set_interrupt_status(false);
    c.add_capability(&[0x09, 0, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0]);
    assert_eq!(c.read(STATUS, 2), 1 << 4, "the capability list bit");
}

#[test]
fn bar_sizing_as_every_pci_driver_does_it() {
    let mut c = device();
    assert_eq!(c.read(BAR0, 4), 0, "memory BAR, 32-bit, non-prefetchable");
    assert_eq!(c.read(BAR0 + 8, 4), 1, "I/O BAR: bit 0 set");
    c.write(BAR0, 4, 0xFFFF_FFFF);
    assert_eq!(
        c.read(BAR0, 4),
        0xFFFF_C000,
        "size 16 KiB: the low 14 bits are zero"
    );
    c.write(BAR0 + 8, 4, 0xFFFF_FFFF);
    assert_eq!(
        c.read(BAR0 + 8, 4),
        0xFFFF_FFC1,
        "size 64 bytes, I/O flag kept"
    );
    c.write(BAR0, 4, 0xFE00_0000);
    assert_eq!(c.read(BAR0, 4), 0xFE00_0000);
    c.write(BAR0, 4, 0xFE00_1234);
    assert_eq!(
        c.read(BAR0, 4),
        0xFE00_0000,
        "the address is aligned to the size"
    );
    c.write(BAR0 + 8, 4, 0xC041);
    assert_eq!(c.read(BAR0 + 8, 4), 0xC041);
    // an undefined BAR reads zero and stays zero
    c.write(BAR0 + 4, 4, 0xFFFF_FFFF);
    assert_eq!(c.read(BAR0 + 4, 4), 0);
    assert_eq!(c.read(BAR0 + 20, 4), 0);
    // sub-dword writes do not program a BAR
    c.write(BAR0, 1, 0xAB);
    assert_eq!(c.read(BAR0, 4), 0xFE00_0000);
}

#[test]
fn a_bar_decodes_only_while_its_space_is_enabled() {
    let mut c = device();
    c.write(BAR0, 4, 0xFE00_0000);
    c.write(BAR0 + 8, 4, 0xC040);
    assert_eq!(c.bar_base(0), None, "memory space is off");
    assert_eq!(c.memory_hit(0xFE00_0010), None);
    c.write(COMMAND, 2, u32::from(CMD_MEMORY));
    assert_eq!(c.bar_base(0), Some(0xFE00_0000));
    assert_eq!(c.memory_hit(0xFE00_0010), Some((0, 0x10)));
    assert_eq!(c.memory_hit(0xFE00_3FFF), Some((0, 0x3FFF)));
    assert_eq!(c.memory_hit(0xFE00_4000), None, "one past the end");
    assert_eq!(c.memory_hit(0xFDFF_FFFF), None);
    assert_eq!(c.io_hit(0xC044), None, "I/O space is still off");
    c.write(COMMAND, 2, u32::from(CMD_IO));
    assert_eq!(c.io_hit(0xC044), Some((2, 4)));
    assert_eq!(c.io_hit(0xC040 + 0x40), None);
    assert_eq!(c.memory_hit(0xFE00_0010), None, "memory space off again");
}

#[test]
fn a_bar_at_address_zero_decodes_nothing() {
    let mut c = device();
    c.write(COMMAND, 2, u32::from(CMD_MEMORY | CMD_IO));
    assert_eq!(
        c.memory_hit(0x10),
        None,
        "an unprogrammed BAR is not a window at 0"
    );
    assert_eq!(c.io_hit(0x10), None);
}

#[test]
fn interrupt_line_cache_line_and_latency_are_writable() {
    let mut c = device();
    c.write(INTERRUPT_LINE, 1, 11);
    assert_eq!(c.interrupt_line(), 11);
    c.write(INTERRUPT_PIN, 1, 4);
    assert_eq!(c.read(INTERRUPT_PIN, 1), 1, "the pin is fixed");
    c.write(0x0C, 1, 16);
    c.write(0x0D, 1, 64);
    assert_eq!(c.read(0x0C, 2), 16 | (64 << 8));
}

#[test]
fn capabilities_are_chained_in_order_and_aligned() {
    let mut c = device();
    let a = c.add_capability(&[0x09, 0xFF, 6, 0, 0, 0]); // the next-pointer byte is overwritten
    let b = c.add_capability(&[0x09, 0, 6, 0, 0, 0]);
    let d = c.add_capability(&[0x05, 0, 0, 0]);
    assert_eq!((a, b, d), (0x40, 0x48, 0x50));
    assert_eq!(c.read(CAPABILITIES, 1), 0x40);
    assert_eq!(c.read(0x41, 1), 0x48);
    assert_eq!(c.read(0x49, 1), 0x50);
    assert_eq!(c.read(0x51, 1), 0, "the last one ends the list");
    assert_eq!(c.read(0x40, 1), 0x09);
    assert_eq!(c.read(0x50, 1), 0x05);
}

// ------------------------------------------------------------------------ bus

fn machine() -> Machine {
    Machine::new(1_790_944_496, 100_000_000)
}

fn cfg_addr(bus: u8, dev: u8, func: u8, reg: u8) -> u32 {
    (1 << 31)
        | (u32::from(bus) << 16)
        | (u32::from(dev) << 11)
        | (u32::from(func) << 8)
        | u32::from(reg & 0xFC)
}

fn cf8_read(m: &mut Machine, bus: u8, dev: u8, func: u8, reg: u8) -> u32 {
    m.io_out(PCI_ADDRESS, 4, cfg_addr(bus, dev, func, reg), 0);
    m.io_in(PCI_DATA, 4, 0)
}

fn ecam(bus: u8, dev: u8, func: u8, reg: u16) -> u64 {
    ECAM_BASE
        + (u64::from(bus) << 20)
        + (u64::from(dev) << 15)
        + (u64::from(func) << 12)
        + u64::from(reg)
}

#[test]
fn the_host_bridge_is_found_at_00_00_0() {
    let mut m = machine();
    assert_eq!(cf8_read(&mut m, 0, 0, 0, 0), 0x29C0_8086);
    assert_eq!(
        cf8_read(&mut m, 0, 0, 0, 8) >> 8,
        0x06_0000,
        "host bridge class"
    );
    assert_eq!(cf8_read(&mut m, 0, 0, 0, 0x2C), 0x1100_1AF4);
    assert_eq!(m.unclaimed_in + m.unclaimed_out, 0);
}

#[test]
fn scanning_the_bus_the_way_linux_does_finds_exactly_the_present_functions() {
    let mut m = machine();
    let mut found = Vec::new();
    for bus in [0u8, 1, 7, 255] {
        for dev in 0..32u8 {
            for func in 0..8u8 {
                let id = cf8_read(&mut m, bus, dev, func, 0);
                if id & 0xFFFF != 0xFFFF {
                    found.push((bus, dev, func, id));
                }
            }
        }
    }
    assert_eq!(
        found,
        [
            (0, 0, 0, 0x29C0_8086),
            (0, 3, 0, 0x1042_1AF4),
            (0, 4, 0, 0x1041_1AF4)
        ]
    );
}

#[test]
fn the_address_register_must_have_its_enable_bit() {
    let mut m = machine();
    m.io_out(PCI_ADDRESS, 4, cfg_addr(0, 0, 0, 0) & !(1 << 31), 0);
    assert_eq!(m.io_in(PCI_DATA, 4, 0), 0xFFFF_FFFF);
    m.io_out(PCI_DATA, 4, 0x1234, 0);
    assert_eq!(
        m.io_in(PCI_ADDRESS, 4, 0),
        cfg_addr(0, 0, 0, 0) & !(1 << 31),
        "the address register reads back"
    );
}

#[test]
fn sub_dword_accesses_through_the_data_window() {
    let mut m = machine();
    m.io_out(PCI_ADDRESS, 4, cfg_addr(0, 0, 0, 0), 0);
    assert_eq!(m.io_in(PCI_DATA, 2, 0), 0x8086);
    assert_eq!(m.io_in(PCI_DATA + 2, 2, 0), 0x29C0);
    assert_eq!(m.io_in(PCI_DATA + 3, 1, 0), 0x29);
    assert_eq!(m.io_in(PCI_DATA + 1, 1, 0), 0x80);
    // the command register by byte and word
    m.io_out(PCI_ADDRESS, 4, cfg_addr(0, 0, 0, 4), 0);
    m.io_out(PCI_DATA, 2, 0x0006, 0);
    assert_eq!(m.io_in(PCI_DATA, 2, 0), 0x0006);
    m.io_out(PCI_DATA + 1, 1, 0x04, 0);
    assert_eq!(m.io_in(PCI_DATA, 2, 0), 0x0406);
    // an address with low bits set selects the dword that contains it
    m.io_out(PCI_ADDRESS, 4, cfg_addr(0, 0, 0, 4) | 3, 0);
    assert_eq!(m.io_in(PCI_DATA, 2, 0), 0x0406);
}

#[test]
fn ecam_shows_the_same_registers() {
    let mut m = machine();
    assert_eq!(m.mmio_read(ecam(0, 0, 0, 0), 4, 0), 0x29C0_8086);
    assert_eq!(m.mmio_read(ecam(0, 0, 0, 0), 2, 0), 0x8086);
    assert_eq!(m.mmio_read(ecam(0, 0, 0, 2), 2, 0), 0x29C0);
    assert_eq!(m.mmio_read(ecam(0, 0, 0, 3), 1, 0), 0x29);
    assert_eq!(m.mmio_read(ecam(0, 5, 0, 0), 4, 0), 0xFFFF_FFFF);
    assert_eq!(m.mmio_read(ecam(1, 0, 0, 0), 4, 0), 0xFFFF_FFFF);
    assert_eq!(m.mmio_read(ecam(0, 0, 1, 0), 4, 0), 0xFFFF_FFFF);
    assert_eq!(
        m.mmio_read(ecam(0, 0, 0, 0x100), 4, 0),
        0xFFFF_FFFF,
        "extended space: ones"
    );
    m.mmio_write(ecam(0, 0, 0, 4), 2, 0x0006, 0);
    m.io_out(PCI_ADDRESS, 4, cfg_addr(0, 0, 0, 4), 0);
    assert_eq!(
        m.io_in(PCI_DATA, 2, 0),
        0x0006,
        "a write through ECAM is seen through the ports"
    );
    assert_eq!(m.mmio_read(ECAM_BASE + ECAM_SIZE - 4, 4, 0), 0xFFFF_FFFF);
    assert_eq!(m.mmio_read(ECAM_BASE + ECAM_SIZE, 4, 0), 0xFFFF_FFFF);
    assert_eq!(
        m.unclaimed_mmio, 1,
        "only the access past the window is unclaimed"
    );
    assert_eq!(
        m.mmio_read(ecam(0, 0, 0, 0), 8, 0) as u32,
        0xFFFF_FFFF,
        "8-byte accesses are not config accesses"
    );
}

#[test]
fn writes_to_an_empty_slot_go_nowhere() {
    let mut m = machine();
    m.io_out(PCI_ADDRESS, 4, cfg_addr(0, 5, 0, 4), 0);
    m.io_out(PCI_DATA, 4, 0xFFFF, 0);
    assert_eq!(m.io_in(PCI_DATA, 4, 0), 0xFFFF_FFFF);
    assert_eq!((m.unclaimed_in, m.unclaimed_out), (0, 0));
}

#[test]
fn the_address_register_takes_byte_and_word_accesses_too() {
    let mut m = machine();
    m.io_out(PCI_ADDRESS, 4, 0x8000_1234, 0);
    assert_eq!(m.io_in(PCI_ADDRESS, 1, 0), 0x34);
    assert_eq!(m.io_in(PCI_ADDRESS + 1, 1, 0), 0x12);
    assert_eq!(m.io_in(PCI_ADDRESS + 2, 2, 0), 0x8000);
    assert_eq!(m.io_in(PCI_ADDRESS + 1, 2, 0), 0x0012, "bytes 1 and 2");
    m.io_out(PCI_ADDRESS + 3, 1, 0x00, 0);
    assert_eq!(
        m.io_in(PCI_ADDRESS, 4, 0),
        0x0000_1234,
        "writing the top byte cleared enable"
    );
    m.io_out(PCI_ADDRESS + 2, 2, 0x8001, 0);
    assert_eq!(m.io_in(PCI_ADDRESS, 4, 0), 0x8001_1234);
    m.io_out(PCI_ADDRESS, 1, 0xFF, 0);
    assert_eq!(
        m.io_in(PCI_ADDRESS, 4, 0),
        0x8001_12FF,
        "only the byte written changed"
    );
    assert_eq!(m.unclaimed_in + m.unclaimed_out, 0);
}
