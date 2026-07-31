//! Proves timer-driven preemption: a kernel thread that never yields makes
//! progress anyway, while the main flow spin-waits without yielding either.
//! If preemption is broken, neither side ever gives up the CPU and the test
//! fails by timeout.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(rust_os::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};

use bootloader::{entry_point, BootInfo};
use rust_os::memory::{self, BootInfoFrameAllocator};
use rust_os::{allocator, threads, time};
use x86_64::VirtAddr;

entry_point!(main);

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn main(boot_info: &'static BootInfo) -> ! {
    rust_os::init();
    let phys_mem_offset = VirtAddr::new(boot_info.physical_memory_offset);
    let mut mapper = unsafe { memory::init(phys_mem_offset) };
    let mut frame_allocator = unsafe {
        BootInfoFrameAllocator::init(&boot_info.memory_map)
    };
    allocator::init_heap(&mut mapper, &mut frame_allocator)
        .expect("heap initialization failed");
    threads::init();

    test_main();
    loop {}
}

fn busy_worker() {
    loop {
        COUNTER.fetch_add(1, Ordering::Relaxed);
    }
}

#[test_case]
fn busy_thread_is_preempted() {
    threads::spawn(busy_worker).expect("spawn failed");

    // Spin without yielding. Only a timer-driven context switch can let the
    // worker run; give it a few seconds' worth of ticks.
    let deadline = time::ticks() + 3 * time::TIMER_HZ;
    while time::ticks() < deadline {
        if COUNTER.load(Ordering::Relaxed) > 0 {
            return;
        }
        core::hint::spin_loop();
    }
    panic!("busy worker never ran: preemption is not happening");
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    rust_os::test_panic_handler(info)
}
