#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(rust_os::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader::{entry_point, BootInfo};
use rust_os::memory;
use rust_os::task::executor::Executor;
use rust_os::task::{shell, Task};
use rust_os::allocator;
#[cfg(not(test))]
use rust_os::println;
use x86_64::VirtAddr;

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    rust_os::init();

    let phys_mem_offset = VirtAddr::new(boot_info.physical_memory_offset);
    unsafe { memory::init_global(phys_mem_offset, &boot_info.memory_map) };
    memory::with_kernel_memory(|mapper, frame_allocator| {
        allocator::init_heap(mapper, frame_allocator)
    })
    .expect("heap initialization failed");

    rust_os::threads::init(); // needs the heap; adopts this flow as thread 0

    #[cfg(test)]
    test_main();

    let mut executor = Executor::new();
    executor.spawn(Task::new(shell::run_shell()));
    executor.run();
}

/// This function is called on panic.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("{}", info);
    rust_os::hlt_loop();
}

#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    rust_os::test_panic_handler(info)
}