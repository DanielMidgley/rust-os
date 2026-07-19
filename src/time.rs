//! A simple monotonic clock and async `sleep`, driven by the Programmable
//! Interval Timer (PIT, channel 0 -> IRQ0).
//!
//! The PIT is programmed to fire `TIMER_HZ` times per second. Each interrupt
//! bumps a global tick counter and wakes any sleeping tasks whose deadline has
//! passed. `sleep(ms)` returns a future that completes once enough ticks have
//! elapsed.

use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

/// Timer interrupts per second. 100 Hz gives 10 ms resolution, which is plenty
/// for a hobby kernel and keeps the interrupt rate low.
pub const TIMER_HZ: u64 = 100;

/// The PIT's fixed input clock, ~1.193182 MHz.
const PIT_INPUT_HZ: u64 = 1_193_182;

/// Milliseconds elapsed can be derived from ticks; this is ticks since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    /// Tasks waiting on a deadline, as (deadline_in_ticks, waker) pairs.
    ///
    /// Locked with interrupts disabled on the task side (see `Sleep::poll`) so
    /// the timer interrupt can safely take the same lock without deadlocking.
    static ref SLEEPERS: Mutex<Vec<(u64, Waker)>> = Mutex::new(Vec::new());
}

/// Programs PIT channel 0 to fire at `TIMER_HZ`.
///
/// Must be called before interrupts are enabled. Uses mode 3 (square-wave
/// generator) with the low byte then high byte of the reload divisor.
pub fn init() {
    let divisor = PIT_INPUT_HZ / TIMER_HZ;
    debug_assert!(divisor <= u16::MAX as u64, "TIMER_HZ too low for the PIT");

    unsafe {
        let mut command: Port<u8> = Port::new(0x43);
        let mut channel0: Port<u8> = Port::new(0x40);
        // Channel 0, access lobyte/hibyte, mode 3, binary counter.
        command.write(0x36);
        channel0.write((divisor & 0xFF) as u8);
        channel0.write((divisor >> 8) as u8);
    }
}

/// Called from the timer interrupt handler on every tick.
///
/// Must not allocate or block. Locking `SLEEPERS` is safe here because the
/// only other locker disables interrupts while holding it.
pub fn tick() {
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;

    let mut sleepers = SLEEPERS.lock();
    sleepers.retain(|(deadline, waker)| {
        if *deadline <= now {
            waker.wake_by_ref();
            false // done: drop this sleeper
        } else {
            true // keep waiting
        }
    });
}

/// Ticks elapsed since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Milliseconds elapsed since boot.
pub fn uptime_ms() -> u64 {
    ticks() * 1000 / TIMER_HZ
}

/// Returns a future that completes after at least `ms` milliseconds.
///
/// Sub-tick durations are rounded up to one whole tick, so any non-zero `ms`
/// sleeps for at least one timer period.
pub fn sleep(ms: u64) -> Sleep {
    let wait_ticks = (ms * TIMER_HZ + 999) / 1000;
    Sleep {
        deadline: ticks() + wait_ticks,
    }
}

/// Future produced by [`sleep`].
pub struct Sleep {
    deadline: u64,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        if ticks() >= self.deadline {
            return Poll::Ready(());
        }

        // Register our waker to be woken when the deadline passes. Disable
        // interrupts so the timer can't fire (and try to lock SLEEPERS) while
        // we hold the lock.
        interrupts::without_interrupts(|| {
            SLEEPERS.lock().push((self.deadline, cx.waker().clone()));
        });

        // Re-check in case a tick landed between the check above and
        // registration; otherwise we could miss the wake and sleep forever.
        if ticks() >= self.deadline {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}
