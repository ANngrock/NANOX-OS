#ifndef NANOX_KERNEL_FAULTS_H
#define NANOX_KERNEL_FAULTS_H

#include <stdint.h>

/* Low-half address that the kernel page tables never map (M1). */
#define NX_FAULT_UNMAPPED_ADDR 0x00000000DEAD0000ull

void nx_fault_ud(void);
void nx_fault_gp(void);
void nx_fault_divzero(void);
void nx_fault_pagefault(void);
uint64_t nx_fault_nullderef(void);
void nx_fault_wprotect(void);
extern volatile uint8_t nx_fault_data_code[16];
void nx_fault_nxexec(void);
uint64_t nx_fault_stack_overflow(uint64_t depth);

#endif
