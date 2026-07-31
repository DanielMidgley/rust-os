//! End-to-end tests for the ELF loader and processes: real binaries read off
//! the FAT16 disk, loaded into their own address spaces, and run in ring 3.
//!
//! The programs are built by `tools/mkelf.py`; each one encodes its result in
//! its exit code so the kernel can assert on it.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(rust_os::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader::{entry_point, BootInfo};
use rust_os::process::{self, ProcError};
use rust_os::usermode::FAULT_EXIT_CODE;
use rust_os::{allocator, memory};
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
fn runs_a_program_in_ring3() {
    // ring3.elf exits with `cs & 3` -- its own privilege level. Only code
    // genuinely executing in ring 3 can produce 3.
    let code = process::exec("/bin/ring3.elf").expect("ring3.elf should run");
    assert_eq!(code, 3, "exit code should be the program's CPL");
}

#[test_case]
fn runs_a_program_that_writes_and_exits() {
    let code = process::exec("/bin/hello.elf").expect("hello.elf should run");
    assert_eq!(code, 0);
}

#[test_case]
fn zero_fills_and_maps_bss_writable() {
    // bss.elf reads a .bss byte (must be 0), writes 5, reads it back, and
    // exits with the sum -- so 5 proves both zero-fill and writability.
    let code = process::exec("/bin/bss.elf").expect("bss.elf should run");
    assert_eq!(code, 5, "bss must be zero-filled and writable");
}

#[test_case]
fn syscalls_return_values_to_user_space() {
    // syscall.elf exits with getpid(). Pids increase, so just check it is
    // non-zero and matches the newest process-table row.
    let code = process::exec("/bin/syscall.elf").expect("syscall.elf should run");
    assert!(code > 0, "getpid should return a real pid");

    let mut buf = [None; process::MAX_PROCESSES];
    let count = process::snapshot(&mut buf);
    let newest = buf[..count]
        .iter()
        .flatten()
        .map(|entry| entry.pid)
        .max()
        .expect("process table should not be empty");
    assert_eq!(code, newest);
}

#[test_case]
fn contains_a_crashing_process() {
    // crash.elf dereferences a null pointer. The kernel must kill just the
    // process...
    let code = process::exec("/bin/crash.elf").expect("crash.elf should load");
    assert_eq!(code, FAULT_EXIT_CODE);

    // ...and still be healthy enough to run another one, which also proves
    // the faulted process's address space was torn down cleanly.
    let code = process::exec("/bin/ring3.elf").expect("kernel survives a crashed process");
    assert_eq!(code, 3);
}

#[test_case]
fn rejects_a_segment_outside_the_user_region() {
    // evil.elf is a valid ELF whose PT_LOAD asks to be mapped over kernel
    // memory. It must be refused before a single frame is allocated.
    assert_eq!(
        process::exec("/bin/evil.elf"),
        Err(ProcError::SegmentOutOfRange)
    );
}

#[test_case]
fn rejects_files_that_are_not_elf_binaries() {
    match process::exec("/README.TXT") {
        Err(ProcError::Elf(_)) => {}
        other => panic!("expected an ELF error, got {:?}", other),
    }
    match process::exec("/bin/nope.elf") {
        Err(ProcError::Fs(_)) => {}
        other => panic!("expected a filesystem error, got {:?}", other),
    }
}

#[test_case]
fn reuses_frames_across_many_processes() {
    // Address spaces are ~10 frames each. Without teardown returning frames
    // to the allocator, this loop would exhaust memory.
    for _ in 0..40 {
        assert_eq!(process::exec("/bin/ring3.elf").expect("repeat exec"), 3);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    rust_os::test_panic_handler(info)
}
