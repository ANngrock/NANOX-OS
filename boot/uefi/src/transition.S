.text
.p2align 4
.global nanox_transition_start
.global nanox_transition_end
.global nanox_transition_fault
.global nanox_transition_test
nanox_transition_start:
    cli
    cld
    mov r15, rcx
    mov rsp, [r15 + 64]
    lgdt [r15 + 32]
    lidt [r15 + 48]
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov ss, ax
    lea rax, [rip + transition_cs]
    push 8
    push rax
    retfq
transition_cs:
    mov ecx, 0xc0000080
    rdmsr
    or eax, 0x800
    wrmsr
    mov rax, cr0
    or rax, 0x10000
    mov cr0, rax
    mov rax, [r15]
    mov cr3, rax
    mov rsp, [r15 + 8]
    xor ebp, ebp
    mov rdi, [r15 + 16]
    mov rax, [r15 + 24]
    jmp rax

// Fatal-only diagnostic: all vectors stop here without attempting IRET, so
// differing exception error-code frames need no unwinding. Existing kernel
// stack or private transition stack must still be mapped (no M1 IST promise).
nanox_transition_fault:
    cli
    cld
    lea rsi, [rip + fault_message]
fault_next:
    lodsb
    test al, al
    jz fault_exit
    mov bl, al
    mov ecx, 100000
    mov dx, 0x3fd
fault_poll:
    in al, dx
    test al, 0x20
    jnz fault_write
    loop fault_poll
fault_write:
    mov dx, 0x3f8
    mov al, bl
    out dx, al
    jmp fault_next
fault_exit:
    cmp byte ptr [rip + nanox_transition_test], 0
    je fault_halt
    mov dx, 0xf4
    mov eax, 0x11
    out dx, eax
fault_halt:
    hlt
    jmp fault_halt
fault_message:
    .asciz "NANOX:LOADER:ERROR:transition-fault\n"
nanox_transition_test:
    .byte 0
nanox_transition_end:
