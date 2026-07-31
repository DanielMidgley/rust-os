use lazy_static::lazy_static;
use pic8259::ChainedPics;
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::PrivilegeLevel;

use crate::{gdt, hlt_loop, println, threads, time, usermode};

/// Software interrupt vector for system calls, reachable from ring 3.
pub const SYSCALL_VECTOR: u8 = 0x80;

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub static PICS: spin::Mutex<ChainedPics> =
    spin::Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
    /// Primary ATA bus. The disk driver polls rather than waiting on this, but
    /// the device still asserts it on command completion, and an IRQ with no
    /// IDT entry escalates into a double fault — so it must be handled.
    PrimaryAta = PIC_2_OFFSET + 6,
    /// Secondary ATA bus, same reasoning.
    SecondaryAta = PIC_2_OFFSET + 7,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }

    fn as_usize(self) -> usize {
        usize::from(self.as_u8())
    }
}

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        unsafe {
            idt.double_fault.set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX); // new
        }
        idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
        idt[InterruptIndex::PrimaryAta.as_u8()].set_handler_fn(primary_ata_interrupt_handler);
        idt[InterruptIndex::SecondaryAta.as_u8()].set_handler_fn(secondary_ata_interrupt_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault_handler);
        // Syscall gate: a raw asm entry stub (it must read the user's
        // registers), DPL 3 so ring 3 may invoke it.
        unsafe {
            idt[SYSCALL_VECTOR]
                .set_handler_addr(usermode::syscall_entry_addr())
                .set_privilege_level(PrivilegeLevel::Ring3);
        }

        idt
    };
}

pub fn init_idt() {
    IDT.load();
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame, _error_code: u64) -> !
{
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    time::tick();

    // EOI must precede the context switch: the thread we switch to runs with
    // this interrupt frame still on the old thread's stack, and the PIC won't
    // deliver another timer IRQ until it's acknowledged.
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }

    threads::preempt();
}

extern "x86-interrupt" fn keyboard_interrupt_handler(
    _stack_frame: InterruptStackFrame
) {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    crate::task::keyboard::add_scancode(scancode); // new

    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

/// The ATA driver polls and asks devices not to raise interrupts (nIEN), so
/// this should never fire — but a stray IRQ with no IDT entry escalates into a
/// double fault, so it is handled and acknowledged. Acknowledging also matters
/// because an un-EOI'd IRQ blocks every lower-priority one, including the timer.
extern "x86-interrupt" fn primary_ata_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::PrimaryAta.as_u8());
    }
}

extern "x86-interrupt" fn secondary_ata_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::SecondaryAta.as_u8());
    }
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // A fault from ring 3 kills the user program, not the kernel.
    if error_code.contains(PageFaultErrorCode::USER_MODE) && usermode::user_active() {
        println!(
            "user program page fault at {:?} ({:?}); terminating it",
            Cr2::read(),
            error_code
        );
        crate::serial_println!(
            "user page fault: cr2={:?} err={:?} rip={:?}",
            Cr2::read(),
            error_code,
            stack_frame.instruction_pointer
        );
        usermode::exit_user_program(usermode::FAULT_EXIT_CODE);
    }

    println!("EXCEPTION: PAGE FAULT");
    println!("Accessed Address: {:?}", Cr2::read());
    println!("Error Code: {:?}", error_code);
    println!("{:#?}", stack_frame);
    hlt_loop();
}

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    // Ring-3 GPF (privileged instruction, bad selector, ...): kill the user
    // program and carry on.
    if usermode::user_active() && stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3 {
        println!(
            "user program general protection fault (error code {}); terminating it",
            error_code
        );
        crate::serial_println!(
            "user gpf: err={} rip={:?}",
            error_code,
            stack_frame.instruction_pointer
        );
        usermode::exit_user_program(usermode::FAULT_EXIT_CODE);
    }

    panic!(
        "EXCEPTION: GENERAL PROTECTION FAULT (error code {})\n{:#?}",
        error_code, stack_frame
    );
}

#[test_case]
fn test_breakpoint_exception() {
    // invoke a breakpoint exception
    x86_64::instructions::interrupts::int3();
}