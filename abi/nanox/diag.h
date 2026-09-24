/*
 * M0 bench diagnostics contract: QEMU isa-debug-exit codes and loader error
 * codes.  The serial markers that accompany them are listed in
 * docs/m0-bench.md; tools/bench/harness.py relies on both.
 *
 * isa-debug-exit (iobase 0xf4, iosize 1): writing value V makes QEMU exit
 * with status (V << 1) | 1.
 */
#ifndef NANOX_ABI_DIAG_H
#define NANOX_ABI_DIAG_H

#define NX_DEBUG_EXIT_PORT 0xF4u

#define NX_EXIT_TEST_PASS 0x10u    /* QEMU exit status 33 */
#define NX_EXIT_TEST_FAIL 0x11u    /* QEMU exit status 35 */
#define NX_EXIT_PANIC 0x12u        /* QEMU exit status 37 */
#define NX_EXIT_LOADER_ERROR 0x13u /* QEMU exit status 39 */
#define NX_EXIT_EXCEPTION 0x14u    /* QEMU exit status 41: unhandled CPU exception */
#define NX_EXIT_CRASH_POINT 0x15u  /* QEMU exit status 43: M4 test stop at a crash point */

/* Loader error codes, printed as "NANOX: LOADER ERROR <NAME> (<code>): ...". */
enum nx_loader_error {
    NX_LE_OK = 0,
    NX_LE_PROTOCOL = 1,           /* LoadedImage / SimpleFileSystem unavailable */
    NX_LE_MANIFEST_OPEN = 2,      /* \NANOX\MANIFEST.BIN missing or unreadable */
    NX_LE_MANIFEST_INVALID = 3,   /* wrong size, magic, version or reserved bytes */
    NX_LE_KERNEL_OPEN = 4,        /* \NANOX\KERNEL.ELF missing */
    NX_LE_KERNEL_READ = 5,        /* read error */
    NX_LE_KERNEL_SIZE = 6,        /* size differs from manifest */
    NX_LE_KERNEL_HASH = 7,        /* SHA-256 differs from manifest */
    NX_LE_ELF_INVALID = 8,        /* ELF header / program headers rejected */
    NX_LE_KERNEL_ALLOC = 9,       /* fixed load address unavailable */
    NX_LE_CMDLINE = 10,           /* \NANOX\CMDLINE.TXT unreadable or invalid */
    NX_LE_MEMMAP = 11,            /* memory map unavailable or inconsistent */
    NX_LE_EXIT_BOOT_SERVICES = 12,/* ExitBootServices() kept failing */
    NX_LE_OUT_OF_MEMORY = 13,     /* pool/page allocation failed */
    NX_LE_INITRD_OPEN = 14,       /* \NANOX\INITRD.IMG missing */
    NX_LE_INITRD_READ = 15,       /* read error */
    NX_LE_INITRD_SIZE = 16,       /* size differs from manifest */
    NX_LE_INITRD_HASH = 17        /* SHA-256 differs from manifest */
};

#endif /* NANOX_ABI_DIAG_H */
