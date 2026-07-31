//! Preemptive kernel threads.
//!
//! Each thread gets its own heap-allocated stack. The PIT timer interrupt
//! drives a round-robin scheduler: on every tick the current thread's
//! callee-saved registers are pushed to its stack, the stack pointer is
//! swapped, and the next thread resumes wherever it left off. A thread that
//! never yields still makes no difference — the timer preempts it.
//!
//! The boot flow (`kernel_main` and the async executor it runs) is itself
//! thread 0, using the bootloader-provided stack; the async world stays
//! cooperative *within* thread 0 while threads preempt around it.
//!
//! Correctness rules, hard-won and load-bearing:
//!
//! * **EOI is sent before switching.** The context switch happens at the end
//!   of the timer handler; if the PIC hadn't been acknowledged first, the
//!   thread we switch to would run with the timer IRQ still masked and
//!   preemption would silently stop.
//! * **The scheduler lock is released before the context switch.** Switching
//!   while holding it would deadlock the next thread the moment it calls
//!   `spawn` or `snapshot`.
//! * **The scheduler never allocates or frees on the interrupt path.** The
//!   run queue and finished list have fixed capacity, reserved up front.
//!   Freeing a dead thread's stack takes the heap lock, so it happens in
//!   `reap`, from thread context.
//! * Task-side scheduler access is wrapped in `without_interrupts`, like
//!   every other lock shared with an interrupt handler.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use core::arch::global_asm;
use core::mem;
use core::sync::atomic::{AtomicU64, Ordering};

use conquer_once::spin::OnceCell;
use spin::Mutex;
use x86_64::instructions::interrupts;

/// Upper bound on live threads (running + ready + finished-but-unreaped).
/// Keeping this fixed lets the scheduler pre-reserve all its storage and
/// never allocate in interrupt context.
pub const MAX_THREADS: usize = 16;

const STACK_SIZE: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Ready,
    Finished,
}

#[derive(Debug)]
pub enum SpawnError {
    NotInitialized,
    TooManyThreads,
}

struct Thread {
    id: u64,
    state: State,
    /// Stack pointer saved at the last switch away from this thread.
    saved_rsp: u64,
    /// Owned stack memory; `None` for thread 0, which runs on the boot stack.
    /// Held alive here and freed on reap.
    _stack: Option<Box<[u8]>>,
}

struct Scheduler {
    current: Box<Thread>,
    ready: VecDeque<Box<Thread>>,
    /// Threads that have exited but whose stacks can't be freed yet — a
    /// thread cannot free the stack it is standing on, and the timer handler
    /// must not touch the heap. Reaped from thread context.
    finished: [Option<Box<Thread>>; MAX_THREADS],
}

static SCHEDULER: OnceCell<Mutex<Scheduler>> = OnceCell::uninit();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Adopts the boot flow as thread 0 and prepares the scheduler. Requires the
/// heap. Until this runs, timer ticks simply don't preempt.
pub fn init() {
    const NO_THREAD: Option<Box<Thread>> = None;
    SCHEDULER
        .try_init_once(|| {
            Mutex::new(Scheduler {
                current: Box::new(Thread {
                    id: 0,
                    state: State::Running,
                    saved_rsp: 0,
                    _stack: None,
                }),
                ready: VecDeque::with_capacity(MAX_THREADS),
                finished: [NO_THREAD; MAX_THREADS],
            })
        })
        .expect("threads::init should only be called once");
}

/// Spawns a kernel thread running `entry`. The thread is preempted by the
/// timer like any other; when `entry` returns, the thread exits and its
/// stack is freed by a later `reap`.
pub fn spawn(entry: fn()) -> Result<u64, SpawnError> {
    let scheduler = SCHEDULER.try_get().map_err(|_| SpawnError::NotInitialized)?;

    // Free any dead stacks first — this also keeps `finished` slots open.
    reap();

    // Allocate and prepare the stack *before* taking the scheduler lock.
    let mut stack = vec![0u8; STACK_SIZE].into_boxed_slice();
    let saved_rsp = prepare_stack(&mut stack, entry);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let thread = Box::new(Thread {
        id,
        state: State::Ready,
        saved_rsp,
        _stack: Some(stack),
    });

    interrupts::without_interrupts(|| {
        let mut sched = scheduler.lock();
        let live = 1 // current
            + sched.ready.len()
            + sched.finished.iter().filter(|slot| slot.is_some()).count();
        if live + 1 > MAX_THREADS {
            return Err(SpawnError::TooManyThreads);
        }
        sched.ready.push_back(thread);
        Ok(())
    })?;

    Ok(id)
}

/// Frees the stacks of exited threads. Called from thread context only —
/// dropping a stack takes the heap lock, and the drop happens with
/// interrupts enabled, outside the scheduler lock.
pub fn reap() {
    let Ok(scheduler) = SCHEDULER.try_get() else {
        return;
    };
    const NO_THREAD: Option<Box<Thread>> = None;
    let mut dead = [NO_THREAD; MAX_THREADS];
    interrupts::without_interrupts(|| {
        let mut sched = scheduler.lock();
        for (slot, out) in sched.finished.iter_mut().zip(dead.iter_mut()) {
            *out = slot.take();
        }
    });
    drop(dead);
}

/// A snapshot of thread IDs and states, for the shell's `threads` command.
/// Fills `buf` under the scheduler lock without allocating; returns the
/// number of entries written.
pub fn snapshot(buf: &mut [Option<(u64, State)>; MAX_THREADS]) -> usize {
    let Ok(scheduler) = SCHEDULER.try_get() else {
        return 0;
    };
    interrupts::without_interrupts(|| {
        let sched = scheduler.lock();
        let mut n = 0;
        let mut push = |entry: (u64, State)| {
            if n < buf.len() {
                buf[n] = Some(entry);
                n += 1;
            }
        };
        push((sched.current.id, sched.current.state));
        for thread in &sched.ready {
            push((thread.id, thread.state));
        }
        for thread in sched.finished.iter().flatten() {
            push((thread.id, thread.state));
        }
        n
    })
}

/// Switches to the next ready thread, if any. Called from the timer
/// interrupt handler (after EOI) and from `thread_exit`; interrupts must be
/// disabled. No-op before `init`.
pub(crate) fn preempt() {
    let Ok(scheduler) = SCHEDULER.try_get() else {
        return;
    };

    // Decide the switch under the lock, but *perform* it after release.
    let switch = {
        let mut sched = scheduler.lock();
        sched.rotate()
    };

    if let Some((old_rsp_slot, new_rsp)) = switch {
        unsafe { context_switch(old_rsp_slot, new_rsp) };
        // When we get here, this thread has been scheduled again.
    }
}

impl Scheduler {
    /// Makes the next ready thread current. Returns where to save the old
    /// thread's stack pointer, and the new thread's stack pointer.
    ///
    /// The returned pointer targets a field inside a `Box<Thread>`, so it
    /// stays valid when the queue shuffles. The write happens after the lock
    /// is released but before interrupts are re-enabled, so nothing can move
    /// or free the thread in between.
    fn rotate(&mut self) -> Option<(*mut u64, u64)> {
        let mut next = self.ready.pop_front()?;
        next.state = State::Running;
        let new_rsp = next.saved_rsp;

        let mut old = mem::replace(&mut self.current, next);
        let old_rsp_slot = if old.state == State::Finished {
            // Park for reaping instead of requeueing. A slot is always free:
            // spawn bounds live threads by MAX_THREADS.
            let slot = self.finished.iter_mut().find(|slot| slot.is_none())?;
            *slot = Some(old);
            &mut slot.as_mut().unwrap().saved_rsp as *mut u64
        } else {
            old.state = State::Ready;
            self.ready.push_back(old);
            &mut self.ready.back_mut().unwrap().saved_rsp as *mut u64
        };
        Some((old_rsp_slot, new_rsp))
    }
}

/// Writes the initial frame `context_switch` will pop onto a fresh stack:
/// six zeroed callee-saved registers (with the entry function in the r12
/// slot) below a return address pointing at `thread_start`. Returns the
/// initial stack pointer.
fn prepare_stack(stack: &mut [u8], entry: fn()) -> u64 {
    // 16-align the top, then lay out 7 words so that `thread_start` begins
    // with rsp % 16 == 0 — its `call` then gives the entry function the
    // 16-byte alignment the ABI requires.
    let top = (stack.as_mut_ptr() as u64 + stack.len() as u64) & !0xF;
    let frame = [
        0,                            // r15
        0,                            // r14
        0,                            // r13
        entry as usize as u64,        // r12: picked up by thread_start
        0,                            // rbx
        0,                            // rbp
        thread_start as unsafe extern "C" fn() as usize as u64, // return address
    ];
    let mut rsp = top;
    for value in frame.iter().rev() {
        rsp -= 8;
        unsafe { (rsp as *mut u64).write(*value) };
    }
    rsp
}

// The context-switch primitives live in threads.s; `thread_exit` is passed
// in as a symbol so it needs no `#[no_mangle]`.
global_asm!(include_str!("threads.s"), thread_exit = sym thread_exit);

unsafe extern "C" {
    /// Saves the current thread's callee-saved registers and stack pointer
    /// into `old_rsp_slot`, then resumes the thread whose stack is `new_rsp`.
    ///
    /// # Safety
    ///
    /// `old_rsp_slot` must be valid to write, `new_rsp` must be a stack
    /// pointer previously produced by this function or `prepare_stack`, and
    /// interrupts must be disabled across the call. See threads.s.
    fn context_switch(old_rsp_slot: *mut u64, new_rsp: u64);

    /// Entry trampoline for new threads; never called from Rust — its
    /// address goes into the initial stack frame built by `prepare_stack`.
    fn thread_start();
}

/// Marks the current thread finished and schedules away for the last time.
/// `rotate` parks finished threads for reaping instead of requeueing them.
extern "C" fn thread_exit() -> ! {
    interrupts::disable();
    if let Ok(scheduler) = SCHEDULER.try_get() {
        scheduler.lock().current.state = State::Finished;
    }
    loop {
        preempt();
        // Unreachable in practice: thread 0 never exits, so the ready queue
        // always has somewhere to go. If not, wait for a tick and retry.
        interrupts::enable_and_hlt();
        interrupts::disable();
    }
}
