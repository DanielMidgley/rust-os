/* Context-switch primitives for src/threads.rs.
 *
 * Pulled in via global_asm!(include_str!("threads.s")) — assembled by LLVM's
 * integrated assembler, Intel syntax. `{thread_exit}` is substituted by the
 * global_asm! invocation with the mangled symbol of threads::thread_exit.
 */

.section .text

/* context_switch(old_rsp_slot: *mut u64 [rdi], new_rsp: u64 [rsi])
 *
 * Saves the callee-saved registers and stack pointer of the current thread
 * and resumes another. The final `ret` lands wherever the target thread's
 * saved stack points: mid-`context_switch` for a preempted thread, or
 * `thread_start` for a fresh one. Caller-saved state is covered by the
 * surrounding call (the timer handler's compiler-generated prologue/epilogue
 * for preemption, or the plain C ABI for voluntary exits).
 *
 * Interrupts must be disabled across this call.
 */
.global context_switch
context_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

/* First code every new thread runs. Reached via context_switch's `ret` with
 * the entry function in r12 (placed there by prepare_stack). Interrupts are
 * off during a switch, so re-enable them before calling the entry function;
 * when it returns, fall into thread_exit, which never comes back.
 */
.global thread_start
thread_start:
    sti
    call r12
    call {thread_exit}
