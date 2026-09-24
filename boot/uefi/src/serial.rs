use core::arch::asm;

fn out(port: u16, value: u8) {
    // SAFETY: M0 runs at CPL0 on its fixed x86 COM1 profile; port I/O has no
    // memory operands and is serialized by the single bootstrap execution path.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack));
    }
}

fn input(port: u16) -> u8 {
    let value;
    // SAFETY: fixed COM1 register, CPL0, no memory or concurrent UART users.
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack));
    }
    value
}

pub fn init() {
    out(0x3f9, 0);
    out(0x3fb, 0x80);
    out(0x3f8, 1);
    out(0x3f9, 0);
    out(0x3fb, 3);
    out(0x3fa, 0xc7);
    out(0x3fc, 3);
}

pub fn write(text: &str) {
    for byte in text.bytes() {
        for _ in 0..100_000 {
            if input(0x3fd) & 0x20 != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        out(0x3f8, byte);
    }
}

pub fn hex(value: u64) {
    write("0x");
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 15) as usize;
        write(core::str::from_utf8(&b"0123456789abcdef"[nibble..nibble + 1]).unwrap());
    }
}

pub fn stop(test: bool) -> ! {
    if test {
        // SAFETY: only the explicit manifest test profile enables the fixed
        // QEMU isa-debug-exit port. No memory operands or shared state.
        unsafe {
            asm!("out dx, eax", in("dx") 0xf4u16, in("eax") 0x11u32, options(nomem, nostack));
        }
    }
    loop {
        // SAFETY: fatal loader path cannot return to firmware; mask interrupts
        // before halting so the failed bootstrap cannot perform more effects.
        unsafe {
            asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}
