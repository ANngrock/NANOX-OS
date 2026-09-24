# GDB session for the NANOX M0 kernel.  Usage:
#   terminal 1:  make debug            (QEMU halted, GDB stub on tcp::1234)
#   terminal 2:  gdb -x tools/gdb/nanox.gdb
#
# The kernel is linked and loaded at physical == virtual 0x200000 and runs on
# the UEFI identity mapping, so the ELF symbols are valid as-is.  A hardware
# breakpoint is used because the loader copies the kernel into memory after
# GDB attaches; under TCG a software breakpoint works too, under KVM it would
# be overwritten by that copy.
set pagination off
set confirm off
file out/kernel.elf
target remote localhost:1234
hbreak kernel_main
continue
# Stopped at kernel_main: RDI = struct nx_boot_info *
print/x bi->magic
print *bi
