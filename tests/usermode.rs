//! Boots the kernel, runs the embedded user program in ring 3, and checks
//! that it exited via the exit syscall with its own CPL — code 3 proves the
//! program really executed with ring-3 privileges and returned through
//! int 0x80.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(rust_os::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader::{entry_point, BootInfo};
use rust_os::{allocator, memory, usermode};
use x86_64::VirtAddr;

entry_point!(main);

fn main(boot_info: &'static BootInfo) -> ! {
    rust_os::init();
    let phys_mem_offset = VirtAddr::new(boot_info.physical_memory_offset);
    unsafe { memory::init_global(phys_mem_offset, &boot_info.memory_map) };
    memory::with_kernel_memory(|mapper, frame_allocator| {
        allocator::init_heap(mapper, frame_allocator)
    })
    .expect("heap initialization failed");

    test_main();
    loop {}
}

#[test_case]
fn user_program_runs_in_ring3() {
    let code = usermode::run_user_program().expect("user program failed to start");
    assert_eq!(code, 3, "exit code should be the program's CPL (ring 3)");
}

#[test_case]
fn user_program_can_run_again() {
    // A second run reuses the mappings and the saved-context slot.
    let code = usermode::run_user_program().expect("second run failed to start");
    assert_eq!(code, 3);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    rust_os::test_panic_handler(info)
}
