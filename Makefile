# NANOX-OS build (M0 bench, M1/M2 kernel).  One documented sequence from a
# fresh checkout:
#
#     make doctor && make && make test
#
# See README.md and docs/m0-bench.md.

.DEFAULT_GOAL := all
.SUFFIXES:
.DELETE_ON_ERROR:

CLANG    ?= clang
LD_LLD   ?= ld.lld
LLD_LINK ?= lld-link
HOST_CC  ?= clang
PYTHON   ?= python3

OUT   := out
BUILD := $(OUT)/build

# Reproducibility: no absolute paths in objects or debug info.
REPRO_FLAGS := -ffile-prefix-map=$(CURDIR)=.

WARN_FLAGS := -Wall -Wextra -Werror -Wno-unused-parameter
FREESTANDING_FLAGS := -std=c17 -ffreestanding -fno-builtin -nostdlibinc \
    -fno-stack-protector -mno-red-zone -mgeneral-regs-only \
    -Iabi -Ilib/include $(WARN_FLAGS) $(REPRO_FLAGS)

# ---- UEFI loader: PE/COFF via clang (MS ABI target) + lld-link -------------
LOADER_CFLAGS := --target=x86_64-unknown-windows $(FREESTANDING_FLAGS) -O2 \
    -mno-stack-arg-probe -fno-asynchronous-unwind-tables
LOADER_LDFLAGS := /subsystem:efi_application /entry:efi_main /nodefaultlib \
    /machine:x64 /Brepro

LOADER_SRCS := boot/uefi/loader.c lib/elf_plan.c boot/uefi/mmap_convert.c \
    lib/serial.c lib/printf.c lib/string.c lib/sha256.c
LOADER_OBJS := $(patsubst %.c,$(BUILD)/loader/%.obj,$(LOADER_SRCS))

# ---- Kernel: ELF64, freestanding C17 + ASM, clang + ld.lld -----------------
KERNEL_CFLAGS := --target=x86_64-unknown-none-elf $(FREESTANDING_FLAGS) -O2 -g \
    -fno-pic -fno-pie -mcmodel=small -fno-asynchronous-unwind-tables \
    -fno-unwind-tables -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer \
    -Ikernel/include -Ikernel
KERNEL_ASFLAGS := --target=x86_64-unknown-none-elf -g $(REPRO_FLAGS)
KERNEL_LDFLAGS := -nostdlib -static --build-id=none -z max-page-size=4096 \
    -z noexecstack -T kernel/arch/x86_64/kernel.ld

KERNEL_CSRCS := kernel/main.c kernel/panic.c kernel/bootinfo_check.c \
    kernel/initramfs.c kernel/faults.c kernel/arch/x86_64/gdt.c \
    kernel/arch/x86_64/idt.c kernel/arch/x86_64/timer.c kernel/mm/pmm.c kernel/mm/pt.c \
    kernel/mm/vmm.c kernel/mm/uaccess.c kernel/obj/handle.c kernel/obj/ipc.c \
    kernel/obj/objects.c kernel/task.c kernel/syscall.c kernel/m2test.c \
    lib/elf_plan.c lib/serial.c lib/printf.c lib/string.c lib/sha256.c
KERNEL_ASRCS := kernel/arch/x86_64/entry.S kernel/arch/x86_64/isr.S
KERNEL_OBJS := $(patsubst %.c,$(BUILD)/kernel/%.o,$(KERNEL_CSRCS)) \
    $(patsubst %.S,$(BUILD)/kernel/%.o,$(KERNEL_ASRCS))

# ---- User programs (M2): static ELF64 for ring 3, packed into the initramfs --
# -fpie gives RIP-relative code and data references, so the programs can be
# linked at NX_USER_BASE (0x8000000000, abi/nanox/syscall.h), beyond the
# reach of the 32-bit absolute addresses of -mcmodel=small.  The link is
# static and not PIE: the result has no dynamic relocations.
USER_CFLAGS := --target=x86_64-unknown-none-elf $(FREESTANDING_FLAGS) -O2 -fpie \
    -fno-asynchronous-unwind-tables -fno-unwind-tables -Iuser/rt
USER_ASFLAGS := --target=x86_64-unknown-none-elf $(REPRO_FLAGS)
USER_LDFLAGS := -nostdlib -static --build-id=none -z max-page-size=4096 \
    -z noexecstack -T user/user.ld
USER_RT_OBJS := $(BUILD)/user/user/rt/start.o $(BUILD)/user/user/rt/rt.o \
    $(BUILD)/user/lib/string.o
USER_PROGS := hello spin ipc-send ipc-recv
USER_ELFS := $(patsubst %,$(BUILD)/user/bin/%,$(USER_PROGS))

LOADER_EFI := $(OUT)/BOOTX64.EFI
KERNEL_ELF := $(OUT)/kernel.elf
INITRD     := $(OUT)/initrd.img
IMAGE      := $(OUT)/nanox.img
INITRD_FILES := $(shell find initrd -type f 2>/dev/null | LC_ALL=C sort)

.PHONY: all doctor test host-test py-test qemu-test run debug debug-check \
    repro-check clean distclean

all: $(LOADER_EFI) $(KERNEL_ELF) $(INITRD) $(IMAGE) $(OUT)/SHA256SUMS

$(BUILD)/loader/%.obj: %.c
	@mkdir -p $(@D)
	$(CLANG) $(LOADER_CFLAGS) -MMD -MP -c $< -o $@

$(LOADER_EFI): $(LOADER_OBJS)
	@mkdir -p $(@D)
	$(LLD_LINK) $(LOADER_LDFLAGS) /out:$@ $^

$(BUILD)/kernel/%.o: %.c
	@mkdir -p $(@D)
	$(CLANG) $(KERNEL_CFLAGS) -MMD -MP -c $< -o $@

$(BUILD)/kernel/%.o: %.S
	@mkdir -p $(@D)
	$(CLANG) $(KERNEL_ASFLAGS) -MMD -MP -c $< -o $@

$(KERNEL_ELF): $(KERNEL_OBJS) kernel/arch/x86_64/kernel.ld
	@mkdir -p $(@D)
	$(LD_LLD) $(KERNEL_LDFLAGS) -o $@ $(KERNEL_OBJS)

$(BUILD)/user/%.o: %.c
	@mkdir -p $(@D)
	$(CLANG) $(USER_CFLAGS) -MMD -MP -c $< -o $@

$(BUILD)/user/%.o: %.S
	@mkdir -p $(@D)
	$(CLANG) $(USER_ASFLAGS) -MMD -MP -c $< -o $@

$(BUILD)/user/bin/%: $(BUILD)/user/user/test/%.o $(USER_RT_OBJS) user/user.ld
	@mkdir -p $(@D)
	$(LD_LLD) $(USER_LDFLAGS) -o $@ $(USER_RT_OBJS) $<

# The user programs go to bin/<name> in the initramfs.
$(INITRD): $(INITRD_FILES) $(USER_ELFS) tools/image/mkinitrd.py
	@mkdir -p $(@D)
	$(PYTHON) tools/image/mkinitrd.py --root initrd --out $@ \
	    $(foreach p,$(USER_PROGS),--file bin/$(p)=$(BUILD)/user/bin/$(p))

$(IMAGE): $(LOADER_EFI) $(KERNEL_ELF) $(INITRD) tools/image/mkimage.py
	$(PYTHON) tools/image/mkimage.py --loader $(LOADER_EFI) --kernel $(KERNEL_ELF) \
	    --initrd $(INITRD) --out $@

$(OUT)/SHA256SUMS: $(LOADER_EFI) $(KERNEL_ELF) $(INITRD) $(IMAGE)
	cd $(OUT) && sha256sum BOOTX64.EFI kernel.elf initrd.img nanox.img > SHA256SUMS
	@cat $@

-include $(LOADER_OBJS:.obj=.d) $(KERNEL_OBJS:.o=.d) $(USER_RT_OBJS:.o=.d) \
    $(patsubst %,$(BUILD)/user/user/test/%.d,$(USER_PROGS))

# ---- Environment check ------------------------------------------------------
doctor:
	$(PYTHON) tools/doctor.py

# ---- Tests --------------------------------------------------------------------
# Host unit tests: loader/kernel C code compiled for the host.
HOST_SAN ?= -fsanitize=undefined -fsanitize-trap=undefined
HOST_CFLAGS := -std=c17 -O1 -g $(WARN_FLAGS) $(HOST_SAN) -Iabi -Ilib/include \
    -Ikernel -Iboot/uefi
HOST_TEST_SRCS := tests/host/test_main.c tests/host/test_bootinfo.c \
    tests/host/test_sha256.c tests/host/test_elf.c tests/host/test_mmap.c \
    tests/host/test_initramfs.c tests/host/test_pt.c tests/host/test_pmm.c \
    tests/host/test_handle.c tests/host/test_ipc.c \
    kernel/bootinfo_check.c kernel/initramfs.c kernel/mm/pt.c kernel/mm/pmm.c \
    kernel/obj/handle.c kernel/obj/ipc.c \
    lib/elf_plan.c boot/uefi/mmap_convert.c lib/sha256.c

$(OUT)/host/test_host: $(HOST_TEST_SRCS) tests/host/test.h \
    $(wildcard abi/nanox/*.h lib/include/nanox/*.h kernel/*.h kernel/mm/*.h kernel/obj/*.h \
        boot/uefi/*.h)
	@mkdir -p $(@D)
	$(HOST_CC) $(HOST_CFLAGS) -o $@ $(HOST_TEST_SRCS)

host-test: $(OUT)/host/test_host $(KERNEL_ELF) $(INITRD)
	$(OUT)/host/test_host $(KERNEL_ELF) $(INITRD)

py-test: all
	$(PYTHON) -m unittest discover -s tests/host -p 'test_*.py' -v

# All scenarios, then repeatability: the normal boot and the page-fault crash
# must produce identical serial markers in three runs; the preemptive
# scheduler scenario (M2) likewise, except that the output lines of its tasks
# are compared as a multiset (their interleaving differs from run to run).
qemu-test: all
	$(PYTHON) tools/bench/harness.py test
	$(PYTHON) tools/bench/harness.py repeat normal pagefault m2-sched --count 3

test: host-test py-test qemu-test

# ---- Bench --------------------------------------------------------------------
run: all
	$(PYTHON) tools/bench/harness.py run normal --echo

debug: all
	$(PYTHON) tools/bench/qemu.py debug

debug-check: all
	$(PYTHON) tools/bench/gdb_check.py

repro-check:
	$(PYTHON) tools/repro_check.py

clean:
	rm -rf $(BUILD) $(OUT)/host $(OUT)/images $(LOADER_EFI) $(KERNEL_ELF) $(INITRD) \
	    $(IMAGE) $(OUT)/SHA256SUMS

# Also removes run records (out/runs).
distclean:
	rm -rf $(OUT)
