//! Ring-3 transitions and system calls.
//!
//! This module owns the mechanics of leaving and re-entering kernel mode;
//! [`crate::process`] owns address spaces and deciding *what* to run. The
//! split matters because entry/exit is the part that must be exactly right at
//! the instruction level, and it is now independent of where the program came
//! from.
//!
//! ## Syscall ABI (int 0x80)
//!
//! rax = syscall number, rdi/rsi/rdx = arguments, return value in rax.
//!
//! | nr | name      | args      | returns            |
//! |----|-----------|-----------|--------------------|
//! | 0  | exit      | code      | (does not return)  |
//! | 1  | write     | ptr, len  | bytes written      |
//! | 2  | uptime_ms | —         | ms since boot      |
//! | 3  | getpid    | —         | current pid        |
//!
//! ## Constraints
//!
//! * **One user program at a time** ([`USER_ACTIVE`]): ring-3 execution
//!   borrows the single TSS `RSP0` stack and one saved-kernel-context slot.
//! * `write` refuses pointers outside the running process's region, so user
//!   code cannot make the kernel read kernel memory on its behalf.
//! * Syscalls run with interrupts disabled on the `RSP0` stack: keep them
//!   short and non-blocking.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use x86_64::VirtAddr;

use crate::{gdt, print, time};

/// Exit code reported when a fault handler kills a misbehaving user program.
pub const FAULT_EXIT_CODE: u64 = 0xDEAD;

const SYSCALL_EXIT: u64 = 0;
const SYSCALL_WRITE: u64 = 1;
const SYSCALL_UPTIME_MS: u64 = 2;
const SYSCALL_GETPID: u64 = 3;

global_asm!(include_str!("usermode.s"), syscall_handler = sym syscall_handler);

unsafe extern "C" {
    /// setjmp half: saves kernel context into `saved_rsp_slot`, `iretq`s to
    /// ring 3, and "returns" the exit code once `exit_user` longjmps back.
    fn enter_user(
        entry: u64,
        user_rsp: u64,
        saved_rsp_slot: *mut u64,
        user_cs: u64,
        user_ss: u64,
    ) -> u64;

    /// longjmp half: abandons the current stack and resumes `enter_user`'s
    /// caller with `code` as the return value.
    fn exit_user(kernel_rsp: u64, code: u64) -> !;

    /// int 0x80 entry stub; its address goes into the IDT.
    fn syscall_entry();
}

/// True while a user program is executing (or suspended by preemption).
static USER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Kernel rsp saved by `enter_user`, consumed by `exit_user`.
static SAVED_KERNEL_RSP: AtomicU64 = AtomicU64::new(0);

/// The running process's pid and the bounds of memory it may pass to the
/// kernel. Valid only while `USER_ACTIVE`.
static CURRENT_PID: AtomicU64 = AtomicU64::new(0);
static RANGE_START: AtomicU64 = AtomicU64::new(0);
static RANGE_END: AtomicU64 = AtomicU64::new(0);

/// The address of the int 0x80 entry stub, for IDT registration.
pub(crate) fn syscall_entry_addr() -> VirtAddr {
    VirtAddr::new(syscall_entry as unsafe extern "C" fn() as usize as u64)
}

/// True if a user program is currently active — used by fault handlers to
/// decide between "kill the user program" and "kernel bug".
pub(crate) fn user_active() -> bool {
    USER_ACTIVE.load(Ordering::SeqCst)
}

/// Enters ring 3 at `entry` and returns the program's exit code.
///
/// The caller is responsible for having made the target address space active
/// and for tearing it down afterwards.
///
/// # Safety
///
/// `entry` and `stack_top` must be mapped, user-accessible addresses in the
/// currently active address space, and `[range_start, range_end)` must
/// describe memory that address space lets user code reach.
pub(crate) unsafe fn enter(
    entry: u64,
    stack_top: u64,
    pid: u64,
    range_start: u64,
    range_end: u64,
) -> u64 {
    if USER_ACTIVE.swap(true, Ordering::SeqCst) {
        // process::exec checks this first; reaching here would mean two
        // programs sharing one RSP0 stack.
        panic!("a user program is already running");
    }
    CURRENT_PID.store(pid, Ordering::SeqCst);
    RANGE_START.store(range_start, Ordering::SeqCst);
    RANGE_END.store(range_end, Ordering::SeqCst);

    let (user_cs, user_ss) = gdt::user_selectors();
    let code = unsafe {
        enter_user(
            entry,
            stack_top,
            SAVED_KERNEL_RSP.as_ptr(),
            user_cs.0 as u64,
            user_ss.0 as u64,
        )
    };

    USER_ACTIVE.store(false, Ordering::SeqCst);
    code
}

/// Terminates the running user program with `code`, longjmping back into
/// [`enter`]. Called by the exit syscall and by fault handlers.
pub(crate) fn exit_user_program(code: u64) -> ! {
    assert!(
        user_active(),
        "exit_user_program called with no active user program"
    );
    let kernel_rsp = SAVED_KERNEL_RSP.load(Ordering::SeqCst);
    unsafe { exit_user(kernel_rsp, code) }
}

/// Rust half of the int 0x80 path; called by `syscall_entry` with interrupts
/// disabled, on the RSP0 stack.
extern "C" fn syscall_handler(nr: u64, a1: u64, a2: u64, _a3: u64) -> u64 {
    match nr {
        SYSCALL_EXIT => exit_user_program(a1),
        SYSCALL_WRITE => sys_write(a1, a2),
        SYSCALL_UPTIME_MS => time::uptime_ms(),
        SYSCALL_GETPID => CURRENT_PID.load(Ordering::SeqCst),
        _ => u64::MAX,
    }
}

/// write(ptr, len): prints bytes from *user* memory.
///
/// The buffer must lie entirely within the running process's region. Without
/// this check a user program could hand the kernel any address and have it
/// read kernel memory aloud — the classic confused-deputy bug.
fn sys_write(ptr: u64, len: u64) -> u64 {
    const MAX_WRITE: u64 = 4096;
    if len > MAX_WRITE {
        return u64::MAX;
    }
    let Some(end) = ptr.checked_add(len) else {
        return u64::MAX;
    };
    if ptr < RANGE_START.load(Ordering::SeqCst) || end > RANGE_END.load(Ordering::SeqCst) {
        return u64::MAX;
    }
    for i in 0..len {
        let byte = unsafe { ((ptr + i) as *const u8).read() };
        print!("{}", byte as char);
    }
    len
}
