//! The memory routines a freestanding program needs (the compiler emits
//! calls to `memcpy`, `memmove`, `memset`, `memcmp` and `bcmp` whenever it
//! likes). Copies and fills use `rep movsb` / `rep stosb`, which the compiler
//! cannot turn back into a call to itself; comparisons read through volatile
//! pointers for the same reason. With the `mem-symbols` feature the C-ABI
//! symbols are exported; without it the functions are plain Rust functions so
//! tests can compare them with the standard library.

/// Copies `n` bytes forward. The ranges must not overlap destructively: if
/// they overlap, `dst` must be below `src`.
///
/// # Safety
/// `src` readable and `dst` writable for `n` bytes.
#[cfg(target_arch = "x86_64")]
pub unsafe fn copy_forward(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: the caller guarantees both ranges; the instruction only
    // touches them.
    unsafe {
        core::arch::asm!(
            "rep movsb",
            inout("rcx") n => _,
            inout("rdi") dst => _,
            inout("rsi") src => _,
            options(nostack, preserves_flags),
        );
    }
}

/// Copies `n` bytes from the end towards the start, for overlapping ranges
/// with `dst` above `src`.
///
/// # Safety
/// As [`copy_forward`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn copy_backward(dst: *mut u8, src: *const u8, n: usize) {
    if n == 0 {
        return;
    }
    // SAFETY: the last bytes of the ranges are inside them; the direction
    // flag is cleared again before returning, as the C ABI requires.
    unsafe {
        core::arch::asm!(
            "std",
            "rep movsb",
            "cld",
            inout("rcx") n => _,
            inout("rdi") dst.add(n - 1) => _,
            inout("rsi") src.add(n - 1) => _,
            options(nostack),
        );
    }
}

/// Fills `n` bytes with `value`.
///
/// # Safety
/// `dst` writable for `n` bytes.
#[cfg(target_arch = "x86_64")]
pub unsafe fn fill(dst: *mut u8, value: u8, n: usize) {
    // SAFETY: the caller guarantees the range.
    unsafe {
        core::arch::asm!(
            "rep stosb",
            inout("rcx") n => _,
            inout("rdi") dst => _,
            in("al") value,
            options(nostack, preserves_flags),
        );
    }
}

// Byte-loop fallbacks so the crate and its tests build on other hosts; the
// volatile accesses keep the compiler from recognising a memcpy loop.
#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn copy_forward(dst: *mut u8, src: *const u8, n: usize) {
    for i in 0..n {
        // SAFETY: in range by the caller contract.
        unsafe { dst.add(i).write_volatile(src.add(i).read_volatile()) };
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn copy_backward(dst: *mut u8, src: *const u8, n: usize) {
    for i in (0..n).rev() {
        // SAFETY: in range by the caller contract.
        unsafe { dst.add(i).write_volatile(src.add(i).read_volatile()) };
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn fill(dst: *mut u8, value: u8, n: usize) {
    for i in 0..n {
        // SAFETY: in range by the caller contract.
        unsafe { dst.add(i).write_volatile(value) };
    }
}

/// Copies `n` bytes; the ranges may overlap.
///
/// # Safety
/// As [`copy_forward`].
pub unsafe fn move_bytes(dst: *mut u8, src: *const u8, n: usize) {
    // dst below src, or entirely above the source range: forward is safe.
    if (dst as usize).wrapping_sub(src as usize) >= n {
        // SAFETY: forwarded contract.
        unsafe { copy_forward(dst, src, n) };
    } else {
        // SAFETY: forwarded contract.
        unsafe { copy_backward(dst, src, n) };
    }
}

/// Difference of the first bytes that differ (as unsigned bytes), or 0.
///
/// # Safety
/// Both ranges readable for `n` bytes.
pub unsafe fn compare(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        // SAFETY: in range by the caller contract.
        let (x, y) = unsafe { (a.add(i).read_volatile(), b.add(i).read_volatile()) };
        if x != y {
            return i32::from(x) - i32::from(y);
        }
        i += 1;
    }
    0
}

#[cfg(feature = "mem-symbols")]
mod symbols {
    #[no_mangle]
    pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
        // SAFETY: C contract: valid, non-overlapping ranges.
        unsafe { super::copy_forward(dst, src, n) };
        dst
    }

    #[no_mangle]
    pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
        // SAFETY: C contract: valid ranges.
        unsafe { super::move_bytes(dst, src, n) };
        dst
    }

    #[no_mangle]
    pub unsafe extern "C" fn memset(dst: *mut u8, value: i32, n: usize) -> *mut u8 {
        // SAFETY: C contract: a valid range; the value is converted to a byte.
        unsafe { super::fill(dst, value as u8, n) };
        dst
    }

    #[no_mangle]
    pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
        // SAFETY: C contract: valid ranges.
        unsafe { super::compare(a, b, n) }
    }

    #[no_mangle]
    pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
        // SAFETY: C contract: valid ranges.
        unsafe { super::compare(a, b, n) }
    }
}
