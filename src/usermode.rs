//! User mode (ring 3) and system calls.
//!
//! There is no ELF loader yet, so the "user program" is a small assembly
//! routine embedded in the kernel image (see usermode.s), copied at run time
//! into pages mapped `USER_ACCESSIBLE`. `run_user_program` enters it via
//! `iretq`; it talks back to the kernel through `int 0x80` and returns via
//! the exit syscall — or by faulting, in which case the fault handlers kill
//! it and the kernel carries on.
//!
//! ## Syscall ABI (int 0x80)
//!
//! rax = syscall number, rdi/rsi/rdx = arguments, return value in rax.
//!
//! | nr | name      | args           | returns            |
//! |----|-----------|----------------|--------------------|
//! | 0  | exit      | code           | (does not return)  |
//! | 1  | write     | ptr, len       | bytes written      |
//! | 2  | uptime_ms | —              | ms since boot      |
//!
//! ## Constraints
//!
//! * **One user program at a time** (`USER_ACTIVE`): ring-3 execution
//!   borrows the single TSS RSP0 stack and the single saved-kernel-context
//!   slot. Only the shell (thread 0) enters user mode.
//! * User pages live in their own region; `sys_write` refuses pointers
//!   outside it, so user code can't make the kernel read kernel memory.
//! * The page-table *parents* of user mappings need `USER_ACCESSIBLE` too —
//!   hence `map_to_with_table_flags`.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

use crate::{gdt, memory, print, time};

/// Exit code reported when a fault handler kills a misbehaving user program.
pub const FAULT_EXIT_CODE: u64 = 0xDEAD;

const PAGE_SIZE: u64 = 4096;
const USER_CODE_START: u64 = 0x5000_0000;
const USER_CODE_SIZE: u64 = 4 * PAGE_SIZE;
const USER_STACK_START: u64 = 0x5010_0000;
const USER_STACK_SIZE: u64 = 4 * PAGE_SIZE;
const USER_STACK_TOP: u64 = USER_STACK_START + USER_STACK_SIZE;

const SYSCALL_EXIT: u64 = 0;
const SYSCALL_WRITE: u64 = 1;
const SYSCALL_UPTIME_MS: u64 = 2;

global_asm!(include_str!("usermode.s"), syscall_handler = sym syscall_handler);

#[allow(non_upper_case_globals)]
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

    static user_program_start: u8;
    static user_program_end: u8;
}

/// True while a user program is executing (or suspended by preemption).
static USER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Kernel rsp saved by `enter_user`, consumed by `exit_user`.
static SAVED_KERNEL_RSP: AtomicU64 = AtomicU64::new(0);

static USER_PAGES_MAPPED: AtomicBool = AtomicBool::new(false);

/// The address of the int 0x80 entry stub, for IDT registration.
pub(crate) fn syscall_entry_addr() -> VirtAddr {
    VirtAddr::new(syscall_entry as unsafe extern "C" fn() as usize as u64)
}

/// True if a user program is currently active — used by fault handlers to
/// decide between "kill the user program" and "kernel bug".
pub(crate) fn user_active() -> bool {
    USER_ACTIVE.load(Ordering::SeqCst)
}

/// Copies the embedded demo program into user pages and runs it in ring 3.
/// Blocks until it exits (kernel threads still preempt); returns its exit
/// code, [`FAULT_EXIT_CODE`] if it was killed by a fault.
pub fn run_user_program() -> Result<u64, &'static str> {
    if USER_ACTIVE.swap(true, Ordering::SeqCst) {
        return Err("a user program is already running");
    }

    ensure_user_pages();
    if let Err(err) = copy_program() {
        USER_ACTIVE.store(false, Ordering::SeqCst);
        return Err(err);
    }

    let (user_cs, user_ss) = gdt::user_selectors();
    let code = unsafe {
        enter_user(
            USER_CODE_START,
            USER_STACK_TOP,
            SAVED_KERNEL_RSP.as_ptr(),
            user_cs.0 as u64,
            user_ss.0 as u64,
        )
    };
    USER_ACTIVE.store(false, Ordering::SeqCst);
    Ok(code)
}

/// Terminates the running user program with `code`, longjmping back into
/// `run_user_program`. Called by the exit syscall and by fault handlers.
pub(crate) fn exit_user_program(code: u64) -> ! {
    assert!(
        user_active(),
        "exit_user_program called with no active user program"
    );
    let kernel_rsp = SAVED_KERNEL_RSP.load(Ordering::SeqCst);
    unsafe { exit_user(kernel_rsp, code) }
}

/// Maps (once) and zeroes the user code and stack regions. Every page —
/// and, crucially, every parent page-table entry — gets `USER_ACCESSIBLE`.
fn ensure_user_pages() {
    if USER_PAGES_MAPPED.load(Ordering::SeqCst) {
        return;
    }

    let flags =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    memory::with_kernel_memory(|mapper, frame_allocator| {
        let regions = [
            (USER_CODE_START, USER_CODE_SIZE),
            (USER_STACK_START, USER_STACK_SIZE),
        ];
        for (start, size) in regions {
            for i in 0..size / PAGE_SIZE {
                let addr = VirtAddr::new(start + i * PAGE_SIZE);
                let page: Page<Size4KiB> = Page::containing_address(addr);
                let frame = frame_allocator
                    .allocate_frame()
                    .expect("out of frames for user pages");
                unsafe {
                    mapper
                        .map_to_with_table_flags(page, frame, flags, flags, frame_allocator)
                        .expect("failed to map user page")
                        .flush();
                }
                // map_to_with_table_flags only flags tables it creates;
                // pre-existing parent entries (the bootloader's) must be
                // widened by hand or the ring-3 page walk faults.
                memory::mark_parents_user_accessible(addr);
            }
        }
    });
    x86_64::instructions::tlb::flush_all();

    // Fresh frames hold whatever was in RAM; zero both regions through the
    // new mappings.
    unsafe {
        core::ptr::write_bytes(USER_CODE_START as *mut u8, 0, USER_CODE_SIZE as usize);
        core::ptr::write_bytes(USER_STACK_START as *mut u8, 0, USER_STACK_SIZE as usize);
    }

    USER_PAGES_MAPPED.store(true, Ordering::SeqCst);
}

/// Copies the embedded program bytes from the kernel image into the user
/// code region.
fn copy_program() -> Result<(), &'static str> {
    let start = &raw const user_program_start;
    let end = &raw const user_program_end;
    let len = end as usize - start as usize;
    if len as u64 > USER_CODE_SIZE {
        return Err("embedded user program exceeds the user code region");
    }
    unsafe { core::ptr::copy_nonoverlapping(start, USER_CODE_START as *mut u8, len) };
    Ok(())
}

/// Rust half of the int 0x80 path; called by `syscall_entry` with interrupts
/// disabled, on the RSP0 stack.
extern "C" fn syscall_handler(nr: u64, a1: u64, a2: u64, _a3: u64) -> u64 {
    match nr {
        SYSCALL_EXIT => exit_user_program(a1),
        SYSCALL_WRITE => sys_write(a1, a2),
        SYSCALL_UPTIME_MS => time::uptime_ms(),
        _ => u64::MAX,
    }
}

/// write(ptr, len): prints bytes from *user* memory. The pointer must lie
/// entirely within the user region — the kernel will not read arbitrary
/// addresses on the user's behalf.
fn sys_write(ptr: u64, len: u64) -> u64 {
    const MAX_WRITE: u64 = 4096;
    if len > MAX_WRITE {
        return u64::MAX;
    }
    let Some(end) = ptr.checked_add(len) else {
        return u64::MAX;
    };
    if ptr < USER_CODE_START || end > USER_STACK_TOP {
        return u64::MAX;
    }
    for i in 0..len {
        let byte = unsafe { ((ptr + i) as *const u8).read() };
        print!("{}", byte as char);
    }
    len
}
