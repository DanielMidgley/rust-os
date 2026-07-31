//! Minimal ELF64 parser for statically linked executables.
//!
//! Reads just enough of the format to load a program: validate the identity
//! bytes and machine type, then collect the `PT_LOAD` program headers that say
//! "map this range of the file at this virtual address."
//!
//! Everything here treats the file as untrusted input. A program header is a
//! request to map memory at an address of the file's choosing, so sizes are
//! checked for overflow and the caller ([`crate::process`]) additionally
//! confirms every segment lands inside the user region — otherwise a crafted
//! ELF could ask to be loaded over the kernel.

/// Program headers must live within the first chunk of the file that the
/// loader buffers. Real linkers place them immediately after the ELF header,
/// so this is generous.
pub const HEADER_BUFFER: usize = 2048;

/// Enough for a conventional code/rodata/data/bss layout.
pub const MAX_SEGMENTS: usize = 8;

const EI_NIDENT: usize = 16;
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 0x3E;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Too short to even contain a header.
    Truncated,
    NotAnElf,
    /// 32-bit, big-endian, or an unexpected ELF version.
    UnsupportedFormat,
    /// Not a statically linked x86-64 executable (e.g. a shared object).
    UnsupportedType,
    /// Program headers are absent, malformed, or outside the buffered prefix.
    BadProgramHeaders,
    /// More loadable segments than [`MAX_SEGMENTS`].
    TooManySegments,
    /// A segment's file range or memory range is nonsensical.
    BadSegment,
}

/// One `PT_LOAD` segment: bytes `[file_offset, file_offset + file_size)` of the
/// file belong at `vaddr`, and the memory range extends to `mem_size` with the
/// remainder zero-filled (this is how `.bss` is expressed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub mem_size: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub writable: bool,
    pub executable: bool,
}

impl Segment {
    /// Exclusive end of the memory this segment occupies.
    pub fn mem_end(&self) -> u64 {
        self.vaddr + self.mem_size
    }
}

/// A parsed executable: where to start, and what to map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    pub entry: u64,
    segments: [Option<Segment>; MAX_SEGMENTS],
    count: usize,
}

impl Image {
    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments[..self.count].iter().flatten()
    }

    pub fn segment_count(&self) -> usize {
        self.count
    }
}

fn u16_at(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn u32_at(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn u64_at(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

/// Parses the ELF header and program headers out of a buffered file prefix.
pub fn parse(buf: &[u8]) -> Result<Image, ElfError> {
    if buf.len() < EHDR_SIZE {
        return Err(ElfError::Truncated);
    }
    if buf[0..4] != ELF_MAGIC {
        return Err(ElfError::NotAnElf);
    }
    if buf[4] != ELFCLASS64 || buf[5] != ELFDATA2LSB || buf[6] != EV_CURRENT {
        return Err(ElfError::UnsupportedFormat);
    }

    let e_type = u16_at(buf, EI_NIDENT);
    let e_machine = u16_at(buf, EI_NIDENT + 2);
    // ET_DYN would need relocation processing; only fixed-address executables
    // are supported.
    if e_type != ET_EXEC || e_machine != EM_X86_64 {
        return Err(ElfError::UnsupportedType);
    }

    let entry = u64_at(buf, 24);
    let e_phoff = u64_at(buf, 32) as usize;
    let e_phentsize = u16_at(buf, 54) as usize;
    let e_phnum = u16_at(buf, 56) as usize;

    if e_phnum == 0 || e_phentsize != PHDR_SIZE {
        return Err(ElfError::BadProgramHeaders);
    }
    let table_end = e_phoff
        .checked_add(e_phnum * PHDR_SIZE)
        .ok_or(ElfError::BadProgramHeaders)?;
    if table_end > buf.len() {
        return Err(ElfError::BadProgramHeaders);
    }

    let mut segments = [None; MAX_SEGMENTS];
    let mut count = 0;

    for i in 0..e_phnum {
        let phdr = &buf[e_phoff + i * PHDR_SIZE..e_phoff + (i + 1) * PHDR_SIZE];
        if u32_at(phdr, 0) != PT_LOAD {
            continue; // PT_PHDR, PT_NOTE, PT_GNU_STACK, ... not needed here
        }
        if count == MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }

        let flags = u32_at(phdr, 4);
        let file_offset = u64_at(phdr, 8);
        let vaddr = u64_at(phdr, 16);
        let file_size = u64_at(phdr, 32);
        let mem_size = u64_at(phdr, 40);

        // `mem_size < file_size` would mean "more bytes in the file than in
        // memory", which is meaningless; the overflow checks keep later
        // page-range arithmetic honest.
        if mem_size < file_size
            || vaddr.checked_add(mem_size).is_none()
            || file_offset.checked_add(file_size).is_none()
        {
            return Err(ElfError::BadSegment);
        }
        if mem_size == 0 {
            continue; // nothing to map
        }

        segments[count] = Some(Segment {
            vaddr,
            mem_size,
            file_offset,
            file_size,
            writable: flags & PF_W != 0,
            executable: flags & PF_X != 0,
        });
        count += 1;
    }

    if count == 0 {
        return Err(ElfError::BadProgramHeaders);
    }
    Ok(Image {
        entry,
        segments,
        count,
    })
}

// Tests

/// A hand-built 64-byte ELF header plus one PT_LOAD program header.
#[cfg(test)]
fn synthetic_elf() -> [u8; HEADER_BUFFER] {
    let mut buf = [0u8; HEADER_BUFFER];
    buf[0..4].copy_from_slice(&ELF_MAGIC);
    buf[4] = ELFCLASS64;
    buf[5] = ELFDATA2LSB;
    buf[6] = EV_CURRENT;
    buf[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    buf[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    buf[24..32].copy_from_slice(&0x2000_0000_1000u64.to_le_bytes()); // e_entry
    buf[32..40].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes()); // e_phoff
    buf[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    buf[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

    let ph = EHDR_SIZE;
    buf[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
    buf[ph + 4..ph + 8].copy_from_slice(&(PF_X | 4).to_le_bytes());
    buf[ph + 8..ph + 16].copy_from_slice(&0x1000u64.to_le_bytes()); // p_offset
    buf[ph + 16..ph + 24].copy_from_slice(&0x2000_0000_1000u64.to_le_bytes()); // p_vaddr
    buf[ph + 32..ph + 40].copy_from_slice(&0x40u64.to_le_bytes()); // p_filesz
    buf[ph + 40..ph + 48].copy_from_slice(&0x100u64.to_le_bytes()); // p_memsz
    buf
}

#[cfg(test)]
#[test_case]
fn parses_a_well_formed_header() {
    let image = parse(&synthetic_elf()).expect("valid ELF should parse");
    assert_eq!(image.entry, 0x2000_0000_1000);
    assert_eq!(image.segment_count(), 1);
    let segment = image.segments().next().unwrap();
    assert_eq!(segment.vaddr, 0x2000_0000_1000);
    assert_eq!(segment.file_size, 0x40);
    assert_eq!(segment.mem_size, 0x100); // the excess is .bss
    assert!(segment.executable);
    assert!(!segment.writable);
}

#[cfg(test)]
#[test_case]
fn rejects_non_elf_input() {
    let mut buf = synthetic_elf();
    buf[1] = b'X';
    assert_eq!(parse(&buf), Err(ElfError::NotAnElf));
    assert_eq!(parse(b"short"), Err(ElfError::Truncated));
}

#[cfg(test)]
#[test_case]
fn rejects_wrong_class_and_machine() {
    let mut buf = synthetic_elf();
    buf[4] = 1; // ELFCLASS32
    assert_eq!(parse(&buf), Err(ElfError::UnsupportedFormat));

    let mut buf = synthetic_elf();
    buf[18..20].copy_from_slice(&0x28u16.to_le_bytes()); // EM_ARM
    assert_eq!(parse(&buf), Err(ElfError::UnsupportedType));
}

#[cfg(test)]
#[test_case]
fn rejects_program_headers_outside_the_buffer() {
    let mut buf = synthetic_elf();
    buf[32..40].copy_from_slice(&(HEADER_BUFFER as u64).to_le_bytes());
    assert_eq!(parse(&buf), Err(ElfError::BadProgramHeaders));
}

#[cfg(test)]
#[test_case]
fn rejects_a_segment_smaller_in_memory_than_on_disk() {
    let mut buf = synthetic_elf();
    let ph = EHDR_SIZE;
    buf[ph + 40..ph + 48].copy_from_slice(&0x10u64.to_le_bytes()); // memsz < filesz
    assert_eq!(parse(&buf), Err(ElfError::BadSegment));
}
