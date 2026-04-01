.section __TEXT,__text,regular,pure_instructions
.global ___save_context
.global ___run_context
.extern _trap_handler
.extern _IRQ_NESTING_OFFSET
.extern _IRQ_STACK_TOP_OFFSET
.extern _TASK_STATE_SEGMENT_OFFSET

.macro trap_no_ec vector
.global ___vector_\vector
___vector_\vector:
    push 0
    push \vector
    jmp ___save_context
.endm

.macro trap_ec vector
.global ___vector_\vector
___vector_\vector:
    push \vector
    jmp ___save_context
.endm

.altmacro
.set i, 0
.rept 256
    /*
        Execptions with error code:
        #DF, #TS, #NP, #SS, #GP, #PF, #AC, #CP
    */
    .if i == 8 || (i >= 10 && i <= 14) || i == 17 || i == 21
        trap_ec %i
    .else
        trap_no_ec %i
    .endif
    .set i, i + 1
.endr

.macro DEF_TABLE_ENTRY, i
    .quad ___vector_\i
.endm

# Save the context and jump to the trap handler
___save_context:
    cli
    cld

    push 0 # placeholder for gs
    push 0 # placeholder for fs
    push r15 # Save r15 for a spare register

    rdgsbase r15 # Save GS base to r15
    mov [rsp + 2 * 8], r15 # Store GS base in the placeholder

    rdfsbase r15 # Save FS base to r15
    mov [rsp + 1 * 8], r15 # Store FS base in the placeholder

    # Save remaining general purpose registers
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rbp
    push rsi
    push rdi
    push rdx
    push rcx
    push rbx
    push rax

    # push uninitialized simd context ptr
    push 0
    push 0

    # return to original ctx if we are from user space
    cmp qword ptr [rsp + 22 * 8], 0x1b # Check if CS is user code segment
    jne .Lswitch_stack

    mov rdi, rsp # Pass pointer to the saved context as argument

    swapgs

    cmp qword ptr [rsp + 19 * 8], 0x1 # Debug jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0x2 # NMI jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0x3 # Software Breakpoint jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0x12 # Machine Check jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0xe # Not Page Fault jump to stack restore
    jne .Lrestore_kstack

    mov rbx, qword ptr [rip + _TASK_STATE_SEGMENT_OFFSET]
    mov rdi, gs:[rbx + 4] # Load user context pointer from TSS.RSP0
    sub rdi, 26*8 # Move pointer to the saved context in the user stack
    mov rsi, rsp
    mov rcx, 26*8 # Size of the context
    rep movsb # Copy context to user stack

.Lrestore_kstack:
    # Restore Kernel Stack
    mov rbx, qword ptr [rip + _TASK_STATE_SEGMENT_OFFSET]
    mov rsp, gs:[rbx + 12]

    # System V Callee-saved registers: rbx, rbp, r12, r13, r14, r15
    pop rbx
    pop rbp
    pop r12
    pop r13
    pop r14
    pop r15

    ret
.Lswitch_stack:
    mov rdi, rsp # Pass pointer to the saved context as argument
    
    cmp qword ptr [rsp + 19 * 8], 0x1 # Debug jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0x2 # NMI jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0x3 # Software Breakpoint jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0xe # Page Fault jump to trap handler
    je .Lenter_handler
    cmp qword ptr [rsp + 19 * 8], 0x12 # Machine Check jump to trap handler
    je .Lenter_handler

    mov rbx, qword ptr [rip + _IRQ_NESTING_OFFSET]
    inc qword ptr gs:[rbx] # Increment IRQ nesting level

    cmp qword ptr gs:[rbx], 1
    jg .Lenter_handler # If we are already in an interrupt, jump to the handler directly

    mov rbx, qword ptr [rip + _IRQ_STACK_TOP_OFFSET]
    mov rsp, gs:[rbx] # Switch to the interrupt stack
.Lenter_handler:
    # Cut off stack
    push 0
    push 0
    
    jmp _trap_handler

___run_context:
    cld
    cli
    
    # Only not retore from trap handler and switch to user space 
    # requires callee-saved registers save and kernel stack save.
    cmp qword ptr [rdi + 22 * 8], 0x1b
    jne .Lset_stack
    cmp rsi, 1
    je .Lset_stack

    # Save Callee-saved registers
    push r15
    push r14
    push r13
    push r12
    push rbp
    push rbx

    mov rax, qword ptr [rip + _TASK_STATE_SEGMENT_OFFSET]
    # Save Kernel RSP to TSS.RSP1
    mov gs:[rax + 12], rsp
.Lset_stack:
    mov rsp, rdi

    # Only not retore from trap handler and switch to user space 
    # requires callee-saved registers save and kernel stack save.
    cmp qword ptr [rsp + 22 * 8], 0x1b
    jne .Lrestore_context
    cmp rsi, 0x1
    je .Lrestore_context

    # Save context pointer to TSS.RSP0
    add rdi, 26*8
    mov gs:[rax + 4], rdi    
.Lrestore_context:
    # Check if we are in nested interrupt
    mov rax, qword ptr [rip + _IRQ_NESTING_OFFSET]
    cmp qword ptr gs:[rax], 0
    je .Lswapgs # If not in interrupt, jump to swapgs to check if we are returning to user space

    dec qword ptr gs:[rax] # Decrement IRQ nesting level
.Lswapgs:
    # Test if we are returning to user space
    cmp qword ptr [rsp + 22 * 8], 0x1b # Check if CS is user code segment
    jne .Lrestore_ctx

    # If returning to user space, we need to switch back to user GS
    swapgs
.Lrestore_ctx:
    # Skip simd context ptr
    add rsp, 16

    # Restore general purpose registers
    pop rax
    pop rbx
    pop rcx
    pop rdx
    pop rdi
    pop rsi
    pop rbp
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14

    # Restore FS and GS bases
    mov r15, [rsp + 1 * 8] # Load FS base to r15
    wrfsbase r15 # Restore FS base

    cmp qword ptr [rsp + 6 * 8], 0x1b # Do not restore GS base if we are returning to kernel space
    jne .Linterrupt_return

    mov r15, [rsp + 2 * 8] # Load GS base to r15
    wrgsbase r15 # Restore GS base
.Lsyscall_return:
    # If we are coming from syscall, we need to return with sysret
    cmp qword ptr [rsp + 3 * 8], 256
    jne .Linterrupt_return

    # Restore user stack
    mov rsp, qword ptr [rsp + 8 * 8]

    # Set RFLAGS and RIP for sysret
    mov r11, qword ptr [rsp + 7 * 8]
    mov rcx, qword ptr [rsp + 5 * 8]

    sysretq
.Linterrupt_return:

    pop r15 # Restore r15

    add rsp, 4 * 8 # Skip FS, GS bases, trap number and error code

    iretq

.section __DATA,__const
.global ___vectors
___vectors:
.set i, 0
.rept 256
    DEF_TABLE_ENTRY %i
    .set i, i + 1
.endr
