//! The smallest native program: starts, sets up a heap in its own BSS,
//! allocates, returns a code. It exercises the runtime (entry, heap, memory
//! routines, startup blob) inside a real freestanding ELF; the syscall layer
//! does not exist yet, so the exit hook parks the thread.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::panic::PanicInfo;
use core::ptr::addr_of_mut;

use nanox_runtime::{nanox_entry, LockedHeap, StartInfo};

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

const ARENA_BYTES: usize = 64 * 1024;
static mut ARENA: [u128; ARENA_BYTES / 16] = [0; ARENA_BYTES / 16];

fn main(info: &StartInfo<'_>) -> i32 {
    // SAFETY: ARENA is used only here, once, and lives for the whole process.
    unsafe {
        let start = addr_of_mut!(ARENA).cast::<u8>();
        if HEAP.add_region(start, ARENA_BYTES).is_err() {
            return 2;
        }
    }
    let mut v: Vec<u32> = Vec::new();
    for i in 0..(info.arg_count() as u32 + 8) {
        v.push(i * i);
    }
    let sum: u32 = v.iter().sum();
    if info.handle("authority").is_some() {
        sum as i32 + 1
    } else {
        sum as i32
    }
}

nanox_entry!(main);

/// Placeholder for the syscall layer (`thread_exit`, M2).
#[no_mangle]
pub extern "C" fn nanox_exit(_code: i32) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    nanox_exit(101)
}
