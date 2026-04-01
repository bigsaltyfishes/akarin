.section .text
.intel_syntax noprefix
.code16

.equ TRAMPOLINE_BASE, 0x8000
.equ STACK_PTR,       TRAMPOLINE_BASE + 0x0fc0
.equ ENTRY_PTR,       TRAMPOLINE_BASE + 0x0fc8
.equ BSP_CR3_PTR,     TRAMPOLINE_BASE + 0x0fd0
.equ TEMP_CR3_PTR,    TRAMPOLINE_BASE + 0x0fd8
.equ TEMP_STACK_TOP,  TRAMPOLINE_BASE + 0x0fe0

.equ mode64_paddr,    mode64 - ap_trampoline_start + TRAMPOLINE_BASE
.equ gdt_paddr,       gdt_table - ap_trampoline_start + TRAMPOLINE_BASE
.equ gdt_ptr_paddr,   gdt_ptr - ap_trampoline_start + TRAMPOLINE_BASE

.global ap_trampoline_start
.global ap_trampoline_end

ap_trampoline_start:
    cli
    cld

    xor     ax, ax
    mov     ds, ax
    mov     es, ax
    mov     ss, ax

    # Configure PAE and paging prerequisites.
    mov     eax, cr4
    or      eax, (1 << 5) | (1 << 7) | (1 << 9) | (1 << 10)
    mov     cr4, eax

    mov     eax, [TEMP_CR3_PTR]
    mov     cr3, eax

    # Enable IA-32e (LME) and NXE.
    mov     ecx, 0xC0000080
    rdmsr
    or      eax, (1 << 8) | (1 << 11)
    wrmsr

    # Turn on paging while already in protected mode.
    mov     eax, cr0
    or      eax, (1 << 0) | (1 << 1) | (1 << 31)
    mov     cr0, eax

    mov     esp, TEMP_STACK_TOP

    lgdt    [gdt_ptr_paddr]

    # Enter long mode code segment.
    # Encoding: 0x66 0xEA + ptr32:16 from 16-bit code.
    .byte   0x66, 0xEA
    .long   mode64_paddr
    .word   0x08

.code64
mode64:
    xor     ax, ax
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax

    # Switch to BSP's active page table and hand over to AP Rust entry.
    mov     rax, [BSP_CR3_PTR]
    mov     cr3, rax

    mov     rsp, [STACK_PTR]
    mov     rax, [ENTRY_PTR]
    call    rax

1:
    hlt
    jmp 1b

gdt_table:
    .quad 0x0000000000000000
    .quad 0x00209A0000000000  # 0x08: 64-bit code
    .quad 0x0000920000000000  # 0x10: data

.align 4
gdt_ptr:
    .word (3 * 8) - 1
    .long gdt_paddr

ap_trampoline_end:
