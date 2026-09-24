# NANOX-OS build, M0 bench.  One documented sequence from a fresh checkout:
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

LOADER_SRCS := boot/uefi/loader.c boot/uefi/elf_plan.c boot/uefi/mmap_convert.c \
    lib/serial.c lib/printf.c lib/string.c lib/sha256.c
LOADER_OBJS := $(patsubst %.c,$(BUILD)/loader/%.obj,$(LOADER_SRCS))

# ---- Kernel: ELF64, freestanding C17 + ASM, clang + ld.lld -----------------
KERNEL_CFLAGS := --target=x86_64-unknown-none-elf $(FREESTANDING_FLAGS) -O2 -g \
    -fno-pic -fno-pie -mcmodel=small -fno-asynchronous-unwind-tables \
    -fno-unwind-tables -Ikernel/include -Ikernel
KERNEL_ASFLAGS := --target=x86_64-unknown-none-elf -g $(REPRO_FLAGS)
KERNEL_LDFLAGS := -nostdlib -static --build-id=none -z max-page-size=4096 \
    -z noexecstack -T kernel/arch/x86_64/kernel.ld

KERNEL_CSRCS := kernel/main.c kernel/panic.c kernel/bootinfo_check.c \
    lib/serial.c lib/printf.c lib/string.c
KERNEL_ASRCS := kernel/arch/x86_64/entry.S
KERNEL_OBJS := $(patsubst %.c,$(BUILD)/kernel/%.o,$(KERNEL_CSRCS)) \
    $(patsubst %.S,$(BUILD)/kernel/%.o,$(KERNEL_ASRCS))

LOADER_EFI := $(OUT)/BOOTX64.EFI
KERNEL_ELF := $(OUT)/kernel.elf
IMAGE      := $(OUT)/nanox.img

.PHONY: all doctor test host-test py-test qemu-test run debug debug-check \
    repro-check clean distclean

all: $(LOADER_EFI) $(KERNEL_ELF) $(IMAGE) $(OUT)/SHA256SUMS

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

$(IMAGE): $(LOADER_EFI) $(KERNEL_ELF) tools/image/mkimage.py
	$(PYTHON) tools/image/mkimage.py --loader $(LOADER_EFI) --kernel $(KERNEL_ELF) --out $@

$(OUT)/SHA256SUMS: $(LOADER_EFI) $(KERNEL_ELF) $(IMAGE)
	cd $(OUT) && sha256sum BOOTX64.EFI kernel.elf nanox.img > SHA256SUMS
	@cat $@

-include $(LOADER_OBJS:.obj=.d) $(KERNEL_OBJS:.o=.d)

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
    kernel/bootinfo_check.c boot/uefi/elf_plan.c boot/uefi/mmap_convert.c lib/sha256.c

$(OUT)/host/test_host: $(HOST_TEST_SRCS) tests/host/test.h \
    $(wildcard abi/nanox/*.h lib/include/nanox/*.h kernel/*.h boot/uefi/*.h)
	@mkdir -p $(@D)
	$(HOST_CC) $(HOST_CFLAGS) -o $@ $(HOST_TEST_SRCS)

host-test: $(OUT)/host/test_host $(KERNEL_ELF)
	$(OUT)/host/test_host $(KERNEL_ELF)

py-test: all
	$(PYTHON) -m unittest discover -s tests/host -p 'test_*.py' -v

qemu-test: all
	$(PYTHON) tools/bench/harness.py test

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
	rm -rf $(BUILD) $(OUT)/host $(OUT)/images $(LOADER_EFI) $(KERNEL_ELF) $(IMAGE) \
	    $(OUT)/SHA256SUMS

# Also removes run records (out/runs).
distclean:
	rm -rf $(OUT)
