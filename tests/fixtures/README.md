# Boot contract fixtures

`generate.py` uses only Python's standard library to generate independent,
little-endian ELF and BootInfo golden bytes. Regenerate with
`python3 tests/fixtures/generate.py`; verify with `--check`.

`valid-minimal.elf` is parser input, not a runnable NANOX kernel. It contains a
four-byte instruction payload and two pages of memory to exercise BSS handling
metadata. Its deliberately irrelevant physical address proves that the parser
does not constrain firmware allocation to `p_paddr`.

The malformed inputs exercise truncated headers, page overlap, writable code,
and checked file-offset arithmetic. Rust host tests additionally mutate the
valid input to cover boundary cases without checking in large binary fixtures.

`boot-info-v1.bin` independently encodes every documented field offset. The host
test compares the complete 160 bytes to the Rust representation. Compile-time
size, alignment, and all field-offset assertions run for host and both guests.
