/* User-mode entry/exit primitives and the embedded demo program.
 *
 * Pulled in via global_asm!(include_str!("usermode.s")) — Intel syntax.
 * `{syscall_handler}` is substituted with the mangled symbol of
 * usermode::syscall_handler.
 */

.section .text

/* int 0x80 entry point (IDT gate, DPL 3).
 *
 * The CPU has already switched to the TSS RSP0 stack, cleared IF, and pushed
 * ss/rsp/rflags/cs/rip. User register state: rax = syscall number,
 * rdi/rsi/rdx = args. Marshal into the C ABI (rdi, rsi, rdx, rcx) and call
 * the Rust dispatcher; its return value stays in rax for the user.
 *
 * The CPU aligned RSP0 to 16 and pushed 5 words, so rsp is 8 mod 16 here;
 * our 8 pushes keep it there, and `sub rsp, 8` restores the ABI-required
 * 16-byte alignment at the call.
 */
.global syscall_entry
syscall_entry:
    push rcx
    push rdx
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    sub rsp, 8
    call {syscall_handler}
    add rsp, 8
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    iretq

/* enter_user(entry [rdi], user_rsp [rsi], saved_rsp_slot [rdx],
 *            user_cs [rcx], user_ss [r8]) -> u64
 *
 * setjmp half: save the callee-saved registers and flags, park rsp in
 * *saved_rsp_slot, then iretq into ring 3. "Returns" only when exit_user
 * longjmps back; the returned rax is the user program's exit code.
 * rflags 0x202 = IF set, so user code runs with interrupts (and preemption)
 * enabled.
 */
.global enter_user
enter_user:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    pushfq
    mov [rdx], rsp
    push r8
    push rsi
    push 0x202
    push rcx
    push rdi
    iretq

/* exit_user(kernel_rsp [rdi], code [rsi]) -> !
 *
 * longjmp half: abandon the current (RSP0) stack, restore the kernel context
 * saved by enter_user, and return to enter_user's caller with rax = code.
 */
.global exit_user
exit_user:
    mov rsp, rdi
    mov rax, rsi
    popfq
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

/* The embedded ring-3 demo program. Copied verbatim into user-accessible
 * pages and entered at its first byte; rip-relative addressing keeps the
 * message reachable after the copy. Exits with its own CPL (cs & 3) so the
 * kernel can assert the program really ran in ring 3.
 */
.global user_program_start
user_program_start:
    /* write(message, len) */
    mov rax, 1
    lea rdi, [rip + user_msg]
    mov rsi, offset user_msg_len /* `offset`: the constant, not a load */
    int 0x80
    /* exit(cs & 3): 3 proves ring 3 */
    xor rax, rax
    xor edi, edi
    mov di, cs
    and edi, 3
    int 0x80
1:  jmp 1b
user_msg:
    .ascii "Hello from ring 3! (write syscall via int 0x80)\n"
user_msg_end:
.set user_msg_len, user_msg_end - user_msg
.global user_program_end
user_program_end:
