.section __TEXT,__text,regular,pure_instructions
.extern _TASK_STATE_SEGMENT_OFFSET
.global ___syscall_entry

___syscall_entry:
    cli

    # Swap GS first
    swapgs

    # Save user stack to CR2
    mov cr2, rsp

    # Load TSS offset
    mov rsp, qword ptr [rip + _TASK_STATE_SEGMENT_OFFSET]

    # Load user context pointer from TSS.RSP0
    mov rsp, gs:[rsp + 4]

    push 0x23 # User data segment selector
    push 0 # Dummy RSP
    push r11 # User RFLAGS
    mov r11, cr2
    mov qword ptr [rsp + 8], r11 # Store user RSP
    mov r11, 0
    mov cr2, r11 # Clear CR2
    push 0x1b # User code segment selector
    push rcx # User RIP

    push 0 # Dummy error code
    push 256 # Syscall vector number
    
    swapgs # Swap back to user GS
    rdgsbase r11
    swapgs # Swap back to kernel GS

    push r11 # gs
    rdfsbase rcx
    push rcx # fs
    
    push r15 # r15
    push r14 # r14
    push r13 # r13
    push r12 # r12
    push r11 # r11
    push r10
    push r9 # r9
    push r8
    push rbp # rbp
    push rsi # rsi
    push rdi # rdi
    push rdx # rdx
    push rcx # rcx
    push rbx # rbx
    push rax # rax

    # simd pointer
    push 0
    push 0

    mov rdi, rsp

    # Restore kernel stack
    mov rax, qword ptr [rip + _TASK_STATE_SEGMENT_OFFSET]
    mov rsp, gs:[rax + 12]

    # System V Callee-saved registers: rbx, rbp, r12, r13, r14, r15
    pop rbx
    pop rbp
    pop r12
    pop r13
    pop r14
    pop r15

    ret