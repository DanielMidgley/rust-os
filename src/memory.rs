use bootloader::bootinfo::{MemoryMap, MemoryRegionType};
use conquer_once::spin::OnceCell;
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::FrameError;
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
    PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

/// The kernel's page table mapper and frame allocator, made globally
/// available so subsystems initialised after boot (e.g. user-mode setup) can
/// map pages. Never locked in interrupt context.
struct KernelMemory {
    mapper: OffsetPageTable<'static>,
    frame_allocator: BootInfoFrameAllocator,
}

static KERNEL_MEMORY: OnceCell<Mutex<KernelMemory>> = OnceCell::uninit();

/// The physical-memory mapping offset, kept for raw page-table walks.
static PHYS_OFFSET: OnceCell<VirtAddr> = OnceCell::uninit();

/// Initializes the global mapper and frame allocator.
///
/// # Safety
///
/// Same contract as [`init`] and [`BootInfoFrameAllocator::init`]: the
/// complete physical memory must be mapped at `physical_memory_offset`, the
/// memory map must be valid, and this must be called only once.
pub unsafe fn init_global(physical_memory_offset: VirtAddr, memory_map: &'static MemoryMap) {
    PHYS_OFFSET
        .try_init_once(|| physical_memory_offset)
        .expect("memory::init_global should only be called once");
    KERNEL_MEMORY
        .try_init_once(|| unsafe {
            Mutex::new(KernelMemory {
                mapper: init(physical_memory_offset),
                frame_allocator: BootInfoFrameAllocator::init(memory_map),
            })
        })
        .expect("memory::init_global should only be called once");
}

/// The offset at which the bootloader mapped all of physical memory.
///
/// Adding a physical address to this yields a virtual address the kernel can
/// use to touch that frame directly — which is how page tables and
/// not-currently-active address spaces are edited.
pub fn phys_offset() -> VirtAddr {
    *PHYS_OFFSET
        .try_get()
        .expect("memory::init_global has not been called")
}

/// Overwrites a frame with zeros through the physical-memory mapping.
///
/// # Safety
///
/// The frame must be owned by the caller and not mapped anywhere in use.
pub unsafe fn zero_frame(frame: PhysFrame) {
    let ptr = (phys_offset() + frame.start_address().as_u64()).as_mut_ptr::<u8>();
    unsafe { core::ptr::write_bytes(ptr, 0, 4096) };
}

/// Runs `f` with exclusive access to the global mapper and frame allocator.
///
/// Panics if [`init_global`] has not been called.
pub fn with_kernel_memory<R>(
    f: impl FnOnce(&mut OffsetPageTable<'static>, &mut BootInfoFrameAllocator) -> R,
) -> R {
    let memory = KERNEL_MEMORY
        .try_get()
        .expect("memory::init_global has not been called");
    let mut guard = memory.lock();
    let KernelMemory {
        mapper,
        frame_allocator,
    } = &mut *guard;
    f(mapper, frame_allocator)
}

/// Initialize a new OffsetPageTable.
///
/// This function is unsafe because the caller must guarantee that the
/// complete physical memory is mapped to virtual memory at the passed
/// `physical_memory_offset`. Also, this function must be only called once
/// to avoid aliasing `&mut` references (which is undefined behavior).
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    unsafe {
        let level_4_table = active_level_4_table(physical_memory_offset);
        OffsetPageTable::new(level_4_table, physical_memory_offset)
    }
}

/// How many freed frames can be held for reuse. Frames beyond this are
/// leaked; the bound keeps the allocator allocation-free.
const FREE_LIST_CAP: usize = 512;

/// A FrameAllocator that returns usable frames from the bootloader's memory map.
///
/// Bump-allocates from the memory map, but recycles frames handed back through
/// [`FrameDeallocator`] first — process teardown returns whole address spaces,
/// so without reuse a few `exec`s would exhaust memory.
pub struct BootInfoFrameAllocator {
    memory_map: &'static MemoryMap,
    next: usize,
    free: [Option<PhysFrame>; FREE_LIST_CAP],
    free_len: usize,
}

impl BootInfoFrameAllocator {
    /// Create a FrameAllocator from the passed memory map.
    ///
    /// This function is unsafe because the caller must guarantee that the passed
    /// memory map is valid. The main requirement is that all frames that are marked
    /// as `USABLE` in it are really unused.
    pub unsafe fn init(memory_map: &'static MemoryMap) -> Self {
        BootInfoFrameAllocator {
            memory_map,
            next: 0,
            free: [None; FREE_LIST_CAP],
            free_len: 0,
        }
    }

    /// Returns an iterator over the usable frames specified in the memory map.
    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        // get usable regions from memory map
        let regions = self.memory_map.iter();
        let usable_regions = regions
            .filter(|r| r.region_type == MemoryRegionType::Usable);
        // map each region to its address range
        let addr_ranges = usable_regions
            .map(|r| r.range.start_addr()..r.range.end_addr());
        // transform to an iterator of frame start addresses
        let frame_addresses = addr_ranges.flat_map(|r| r.step_by(4096));
        // create `PhysFrame` types from the start addresses
        frame_addresses.map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if self.free_len > 0 {
            self.free_len -= 1;
            return self.free[self.free_len].take();
        }
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}

impl FrameDeallocator<Size4KiB> for BootInfoFrameAllocator {
    /// # Safety
    ///
    /// The frame must no longer be mapped anywhere.
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        // A full list means the frame is leaked rather than double-handed-out;
        // leaking is safe, aliasing is not.
        if self.free_len < FREE_LIST_CAP {
            self.free[self.free_len] = Some(frame);
            self.free_len += 1;
        }
    }
}

/// Creates an example mapping for the given page to frame `0xb8000`.
pub fn create_example_mapping(
    page: Page,
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) {
    let frame = PhysFrame::containing_address(PhysAddr::new(0xb8000));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    let map_to_result = unsafe {
        // FIXME: this is not safe, we do it only for testing
        mapper.map_to(page, frame, flags, frame_allocator)
    };
    map_to_result.expect("map_to failed").flush();
}

/// A FrameAllocator that always returns `None`.
pub struct EmptyFrameAllocator;

unsafe impl FrameAllocator<Size4KiB> for EmptyFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        None
    }
}

/// Returns a mutable reference to the active level 4 table.
///
/// This function is unsafe because the caller must guarantee that the
/// complete physical memory is mapped to virtual memory at the passed
/// `physical_memory_offset`. 
/// 
/// This function must be only called once to avoid aliasing `&mut` references.
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr)
    -> &'static mut PageTable
{
    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

/// Translates the given virtual address to the mapped physical address, or
/// `None` if the address is not mapped.
///
/// This function is unsafe because the caller must guarantee that the
/// complete physical memory is mapped to virtual memory at the passed
/// `physical_memory_offset`.
pub unsafe fn translate_addr(addr: VirtAddr, physical_memory_offset: VirtAddr)
    -> Option<PhysAddr>
{
    translate_addr_inner(addr, physical_memory_offset)
}

/// Private function that is called by `translate_addr`.
///
/// This function is safe to limit the scope of `unsafe` because Rust treats
/// the whole body of unsafe functions as an unsafe block. This function must
/// only be reachable through `unsafe fn` from outside of this module.
fn translate_addr_inner(addr: VirtAddr, physical_memory_offset: VirtAddr)
    -> Option<PhysAddr>
{
    // read the active level 4 frame from the CR3 register
    let (level_4_table_frame, _) = Cr3::read();

    let table_indexes = [
        addr.p4_index(), addr.p3_index(), addr.p2_index(), addr.p1_index()
    ];
    let mut frame = level_4_table_frame;

    // traverse the multi-level page table
    for &index in &table_indexes {
        // convert the frame into a page table reference
        let virt = physical_memory_offset + frame.start_address().as_u64();
        let table_ptr: *const PageTable = virt.as_ptr();
        let table = unsafe {&*table_ptr};

        // read the page table entry and update `frame`
        let entry = &table[index];
        frame = match entry.frame() {
            Ok(frame) => frame,
            Err(FrameError::FrameNotPresent) => return None,
            Err(FrameError::HugeFrame) => panic!("huge pages not supported"),
        };
    }

    // calculate the physical address by adding the page offset
    Some(frame.start_address() + u64::from(addr.page_offset()))
}