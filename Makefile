# Canonical Rust/Nix entry points for NANOX-OS.
# Run from the Linux/WSL checkout inside the pinned Nix development shell.
.DEFAULT_GOAL := help

NIX ?= nix
CARGO ?= cargo
XTASK = $(NIX) develop --command $(CARGO) xtask

.PHONY: help doctor build run test replay reproduce-build

help:
	@printf "%s\n" \
	  "NANOX-OS canonical Rust/Nix commands:" \
	  "  make doctor           Check the pinned toolchain" \
	  "  make build            Build the Rust workspace and guest image" \
	  "  make run              Run the default QEMU scenario" \
	  "  make test             Run host tests and QEMU scenarios" \
	  "  make replay           Run tests with record/replay checks" \
	  "  make reproduce-build  Compare two clean builds" \
	  "Archived C17/ASM build: make -f docs/legacy-c/Makefile doctor"

doctor:
	$(XTASK) doctor

build:
	$(XTASK) build

run:
	$(XTASK) run

test:
	$(XTASK) test

replay:
	$(XTASK) test --replay

reproduce-build:
	$(XTASK) reproduce-build
