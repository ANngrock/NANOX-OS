OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)
PHDRS
{
  text PT_LOAD FLAGS(5);
  rodata PT_LOAD FLAGS(4);
  data PT_LOAD FLAGS(6);
}
SECTIONS
{
  . = 0xffffffff80000000;
  .text ALIGN(4096) : { KEEP(*(.text.entry)) *(.text .text.*) } :text
  .rodata ALIGN(4096) : { *(.rodata .rodata.*) } :rodata
  .data ALIGN(4096) : { *(.data .data.*) *(.got .got.*) } :data
  .bss ALIGN(4096) (NOLOAD) : { *(.bss .bss.*) *(COMMON) } :data
  /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
