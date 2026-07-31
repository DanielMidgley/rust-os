//! Processes: real ELF binaries loaded from disk into their own address
//! spaces.
//!
//! A process gets a fresh level-4 page table. The kernel's entries are copied
//! into it, so the kernel stays mapped at the same addresses no matter which
//! address space is active — essential, because an interrupt arriving while
//! user code runs must find the IDT, the handler, and a kernel stack exactly
//! where it expects them. One P4 slot ([`USER_P4_INDEX`]) is reserved for the
//! process's private mappings, and *only* that subtree is ever created or
//! freed. Two processes therefore cannot see each other's memory, and no
//! process can touch the kernel's (kernel pages lack `USER_ACCESSIBLE`).
//!
//! ## Scope
//!
//! Execution is synchronous: [`exec`] loads, runs, and tears down a process
//! before returning its exit code, and only one may run at a time. Concurrent
//! processes would additionally need per-thread `RSP0` and CR3 save/restore in
//! the scheduler; see the roadmap.
//!
//! Because kernel mappings are identical in every address space, preemption
//! while a process runs is safe: a kernel thread scheduled in on top of a
//! process's CR3 finds everything it needs mapped.

use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
    PageTableIndex, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::elf::{self, ElfError, Image};
use crate::fat::{self, FsError};
use crate::memory::{self, BootInfoFrameAllocator};
use crate::usermode;

/// Page-table slot reserved for user mappings. Probed to be unused by the
/// kernel: the bootloader and kernel occupy slots 0, 2-5, 31 and the heap's.
const USER_P4_INDEX: usize = 64;

const PAGE_SIZE: u64 = 4096;

/// Bounds of the per-process region — one 512 GiB P4 slot.
pub const USER_REGION_START: u64 = (USER_P4_INDEX as u64) << 39;
pub const USER_REGION_END: u64 = USER_REGION_START + (1 << 39);

/// Top of the user stack (it grows down from here), 2 MiB into the region so
/// that program segments loaded near the bottom cannot reach it.
pub const USER_STACK_TOP: u64 = USER_REGION_START + 0x0020_0000;
const USER_STACK_PAGES: u64 = 4;
const USER_STACK_BOTTOM: u64 = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;

/// Cap on how much memory one program may map, so a crafted `p_memsz` cannot
/// exhaust every frame in the system.
const MAX_IMAGE_PAGES: u64 = 256; // 1 MiB

pub const MAX_PROCESSES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcError {
    /// A user program is already running (one at a time).
    Busy,
    Fs(FsError),
    Elf(ElfError),
    /// Out of physical frames.
    OutOfMemory,
    /// A segment asked to be mapped outside the user region — the check that
    /// stops a crafted ELF from being loaded over the kernel.
    SegmentOutOfRange,
    /// A segment would collide with the user stack.
    SegmentOverlapsStack,
    /// The image wants more memory than [`MAX_IMAGE_PAGES`].
    ImageTooLarge,
    /// The entry point is not inside a loaded segment.
    BadEntryPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Exited(u64),
}

/// A process-table row. Kept after exit so `ps` can show what happened.
#[derive(Clone, Copy)]
pub struct Entry {
    pub pid: u64,
    pub name: [u8; 16],
    pub name_len: usize,
    pub state: State,
}

impl Entry {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }
}

static TABLE: Mutex<[Option<Entry>; MAX_PROCESSES]> = Mutex::new([None; MAX_PROCESSES]);
static NEXT_PID: AtomicU64 = AtomicU64::new(1);

/// A process address space: its level-4 page table frame.
struct AddressSpace {
    p4_frame: PhysFrame,
}

impl AddressSpace {
    /// Builds an address space that mirrors the kernel's mappings but has an
    /// empty user slot.
    fn create() -> Result<Self, ProcError> {
        let offset = memory::phys_offset();
        let p4_frame = memory::with_kernel_memory(|_, frames| frames.allocate_frame())
            .ok_or(ProcError::OutOfMemory)?;
        unsafe { memory::zero_frame(p4_frame) };

        let (kernel_p4_frame, _) = Cr3::read();
        let kernel_p4 =
            unsafe { &*(offset + kernel_p4_frame.start_address().as_u64()).as_ptr::<PageTable>() };
        let new_p4 =
            unsafe { &mut *(offset + p4_frame.start_address().as_u64()).as_mut_ptr::<PageTable>() };

        // Copying every kernel entry (rather than only a higher half) is what
        // keeps the kernel mapped: this kernel's image, physical map, and heap
        // live in scattered slots, not one contiguous half.
        for (new, kernel) in new_p4.iter_mut().zip(kernel_p4.iter()) {
            *new = kernel.clone();
        }
        // ...but the user slot starts empty, so nothing is shared there.
        new_p4[PageTableIndex::new(USER_P4_INDEX as u16)].set_unused();

        Ok(AddressSpace { p4_frame })
    }

    fn table(&self) -> &'static mut PageTable {
        let offset = memory::phys_offset();
        unsafe { &mut *(offset + self.p4_frame.start_address().as_u64()).as_mut_ptr::<PageTable>() }
    }

    /// Maps `count` zeroed pages starting at `start`, which must be page
    /// aligned and inside the user region.
    fn map_pages(&mut self, start: u64, count: u64, writable: bool) -> Result<(), ProcError> {
        let offset = memory::phys_offset();
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if writable {
            flags |= PageTableFlags::WRITABLE;
        }
        // Intermediate tables must themselves be user-accessible: the CPU
        // checks the flag at *every* level of the walk. They are created here,
        // so unlike a shared kernel table they can simply be built that way.
        let parent_flags =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

        let table = self.table();
        memory::with_kernel_memory(|_, frames| {
            let mut mapper = unsafe { OffsetPageTable::new(table, offset) };
            for i in 0..count {
                let addr = VirtAddr::new(start + i * PAGE_SIZE);
                let page: Page<Size4KiB> = Page::containing_address(addr);
                // Already mapped (segments can share a page); leave it alone.
                if mapper.translate_page(page).is_ok() {
                    continue;
                }
                let frame = frames.allocate_frame().ok_or(ProcError::OutOfMemory)?;
                unsafe { memory::zero_frame(frame) };
                unsafe {
                    mapper
                        .map_to_with_table_flags(page, frame, flags, parent_flags, frames)
                        .map_err(|_| ProcError::OutOfMemory)?
                        // This address space is not active, so there is nothing
                        // in the TLB to invalidate.
                        .ignore();
                }
            }
            Ok(())
        })
    }

    /// Translates a user virtual address using this (possibly inactive)
    /// address space's tables.
    fn translate(&self, addr: VirtAddr) -> Option<PhysAddr> {
        let offset = memory::phys_offset();
        let mut table =
            unsafe { &*(offset + self.p4_frame.start_address().as_u64()).as_ptr::<PageTable>() };
        for index in [addr.p4_index(), addr.p3_index(), addr.p2_index()] {
            let entry = &table[index];
            if entry.is_unused() || entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                return None;
            }
            table = unsafe { &*(offset + entry.addr().as_u64()).as_ptr::<PageTable>() };
        }
        let entry = &table[addr.p1_index()];
        if entry.is_unused() {
            return None;
        }
        Some(entry.addr() + u64::from(addr.page_offset()))
    }

    /// Copies bytes into the address space through the physical-memory map,
    /// so the target space never has to be made active.
    fn write(&self, vaddr: u64, mut bytes: &[u8]) -> Result<(), ProcError> {
        let offset = memory::phys_offset();
        let mut addr = vaddr;
        while !bytes.is_empty() {
            let virt = VirtAddr::new(addr);
            let phys = self.translate(virt).ok_or(ProcError::SegmentOutOfRange)?;
            let page_left = (PAGE_SIZE - u64::from(virt.page_offset())) as usize;
            let take = page_left.min(bytes.len());
            let dest = (offset + phys.as_u64()).as_mut_ptr::<u8>();
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, take) };
            bytes = &bytes[take..];
            addr += take as u64;
        }
        Ok(())
    }

    /// Frees every frame the process owns: the private subtree and the P4
    /// itself. Kernel entries are copies of live kernel tables and must never
    /// be walked here.
    fn destroy(self) {
        memory::with_kernel_memory(|_, frames| {
            let table = self.table();
            let entry = &mut table[PageTableIndex::new(USER_P4_INDEX as u16)];
            if !entry.is_unused() {
                let p3 = PhysFrame::containing_address(entry.addr());
                free_table(p3, 3, frames);
                entry.set_unused();
            }
            unsafe { frames.deallocate_frame(self.p4_frame) };
        });
    }
}

/// Recursively frees a page table and everything below it. `level` is 3 for a
/// P3, 2 for a P2, 1 for a P1 (whose entries are data frames).
fn free_table(frame: PhysFrame, level: u8, frames: &mut BootInfoFrameAllocator) {
    let offset = memory::phys_offset();
    let table = unsafe { &mut *(offset + frame.start_address().as_u64()).as_mut_ptr::<PageTable>() };
    for entry in table.iter_mut() {
        if entry.is_unused() {
            continue;
        }
        let child = PhysFrame::containing_address(entry.addr());
        if level > 1 && !entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            free_table(child, level - 1, frames);
        } else {
            unsafe { frames.deallocate_frame(child) };
        }
        entry.set_unused();
    }
    unsafe { frames.deallocate_frame(frame) };
}

fn page_range(start: u64, size: u64) -> (u64, u64) {
    let first = start & !(PAGE_SIZE - 1);
    let last_byte = start + size - 1;
    let last = last_byte & !(PAGE_SIZE - 1);
    (first, (last - first) / PAGE_SIZE + 1)
}

/// Checks every segment before a single frame is allocated: inside the user
/// region, clear of the stack, and collectively bounded in size.
fn validate(image: &Image) -> Result<(), ProcError> {
    let mut total_pages = 0;
    for segment in image.segments() {
        if segment.vaddr < USER_REGION_START || segment.mem_end() > USER_REGION_END {
            return Err(ProcError::SegmentOutOfRange);
        }
        if segment.vaddr < USER_STACK_TOP && segment.mem_end() > USER_STACK_BOTTOM {
            return Err(ProcError::SegmentOverlapsStack);
        }
        let (_, pages) = page_range(segment.vaddr, segment.mem_size);
        total_pages += pages;
        if total_pages > MAX_IMAGE_PAGES {
            return Err(ProcError::ImageTooLarge);
        }
    }

    let entry_mapped = image
        .segments()
        .any(|s| image.entry >= s.vaddr && image.entry < s.mem_end() && s.executable);
    if !entry_mapped {
        return Err(ProcError::BadEntryPoint);
    }
    Ok(())
}

/// Reads the first [`elf::HEADER_BUFFER`] bytes of a file.
fn read_header(path: &str) -> Result<[u8; elf::HEADER_BUFFER], ProcError> {
    let mut header = [0u8; elf::HEADER_BUFFER];
    let mut filled = 0;
    fat::read_file(path, |chunk| {
        if filled < header.len() {
            let take = chunk.len().min(header.len() - filled);
            header[filled..filled + take].copy_from_slice(&chunk[..take]);
            filled += take;
        }
    })
    .map_err(ProcError::Fs)?;
    Ok(header)
}

/// Streams the file again, copying each segment's bytes to its virtual
/// address. Bytes past `p_filesz` are already zero because every frame is
/// zeroed at map time — that is what gives `.bss` its zero fill.
fn load_segments(path: &str, image: &Image, space: &AddressSpace) -> Result<(), ProcError> {
    let mut file_pos = 0u64;
    let mut failure = None;

    fat::read_file(path, |chunk| {
        let chunk_start = file_pos;
        let chunk_end = chunk_start + chunk.len() as u64;
        file_pos = chunk_end;
        if failure.is_some() {
            return;
        }
        for segment in image.segments() {
            let seg_end = segment.file_offset + segment.file_size;
            let start = chunk_start.max(segment.file_offset);
            let end = chunk_end.min(seg_end);
            if start >= end {
                continue;
            }
            let src = &chunk[(start - chunk_start) as usize..(end - chunk_start) as usize];
            let dest = segment.vaddr + (start - segment.file_offset);
            if let Err(err) = space.write(dest, src) {
                failure = Some(err);
                return;
            }
        }
    })
    .map_err(ProcError::Fs)?;

    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn record(pid: u64, path: &str) {
    let mut name = [0u8; 16];
    // Keep the last path component, truncated to fit.
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let len = base.len().min(name.len());
    name[..len].copy_from_slice(&base.as_bytes()[..len]);

    let entry = Entry {
        pid,
        name,
        name_len: len,
        state: State::Running,
    };

    let mut table = TABLE.lock();
    // Reuse the oldest slot once the table is full; this is a history buffer.
    let slot = table
        .iter()
        .position(|e| e.is_none())
        .unwrap_or(((pid - 1) as usize) % MAX_PROCESSES);
    table[slot] = Some(entry);
}

fn finish(pid: u64, code: u64) {
    let mut table = TABLE.lock();
    for entry in table.iter_mut().flatten() {
        if entry.pid == pid {
            entry.state = State::Exited(code);
        }
    }
}

/// Loads the ELF binary at `path` and runs it in ring 3, returning its exit
/// code. Blocks until the process exits or is killed by a fault.
pub fn exec(path: &str) -> Result<u64, ProcError> {
    if usermode::user_active() {
        return Err(ProcError::Busy);
    }

    let header = read_header(path)?;
    let image = elf::parse(&header).map_err(ProcError::Elf)?;
    validate(&image)?;

    let mut space = AddressSpace::create()?;

    // Map the image, then the stack. Any failure from here must still free
    // what was mapped, so errors go through `abort`.
    let mut setup = || -> Result<(), ProcError> {
        for segment in image.segments() {
            let (start, pages) = page_range(segment.vaddr, segment.mem_size);
            space.map_pages(start, pages, segment.writable)?;
        }
        space.map_pages(USER_STACK_BOTTOM, USER_STACK_PAGES, true)?;
        load_segments(path, &image, &space)
    };
    if let Err(err) = setup() {
        space.destroy();
        return Err(err);
    }

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    record(pid, path);

    let (kernel_p4, kernel_flags) = Cr3::read();
    let code = unsafe {
        Cr3::write(space.p4_frame, Cr3Flags::empty());
        // Returns when the process exits or a fault handler kills it; either
        // way control resumes here, still on the kernel stack.
        let code = usermode::enter(
            image.entry,
            USER_STACK_TOP,
            pid,
            USER_REGION_START,
            USER_REGION_END,
        );
        Cr3::write(kernel_p4, kernel_flags);
        code
    };

    space.destroy();
    finish(pid, code);
    Ok(code)
}

/// Copies the process table out for `ps`, returning how many rows were written.
pub fn snapshot(buf: &mut [Option<Entry>; MAX_PROCESSES]) -> usize {
    let table = TABLE.lock();
    let mut count = 0;
    for entry in table.iter().flatten() {
        buf[count] = Some(*entry);
        count += 1;
    }
    count
}
