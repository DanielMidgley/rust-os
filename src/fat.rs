//! Read-only FAT16 filesystem.
//!
//! Enough of FAT16 to list directories and read files: BPB parsing, the fixed
//! root directory region, subdirectory cluster chains, 8.3 short names, and
//! FAT chain walking.
//!
//! ## No allocation, anywhere
//!
//! The kernel heap is 100 KiB, and `cat` on a large file must not consume it.
//! Every operation here works through a 512-byte stack buffer and hands data
//! to a caller-supplied callback, so listing a directory or reading a file of
//! any size allocates nothing. That is why the public API is
//! [`list`]/[`read_file`] taking closures rather than returning `Vec`/`String`.
//!
//! ## Untrusted input
//!
//! Everything read from disk is attacker-controlled as far as the kernel is
//! concerned: the BPB is validated before use, cluster numbers are
//! range-checked against the volume's cluster count, and every chain walk is
//! bounded by the total cluster count so a cyclic FAT cannot spin forever.

use core::str;

use spin::Mutex;

use crate::ata::{self, AtaError, SECTOR_SIZE};

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
/// A directory entry with exactly these bits set is a long-filename fragment,
/// not a real entry.
const ATTR_LONG_NAME: u8 = 0x0F;

const DIR_ENTRY_SIZE: usize = 32;
const ENTRY_FREE: u8 = 0xE5;

/// FAT type is defined by cluster count, not by any field on disk.
const FAT16_MIN_CLUSTERS: u32 = 4085;
const FAT16_MAX_CLUSTERS: u32 = 65524;

/// Values at or above this in a FAT entry terminate the chain.
const CHAIN_END: u16 = 0xFFF8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// No attached drive holds a FAT16 volume.
    NoFilesystem,
    /// The disk driver failed.
    Io(AtaError),
    NotFound,
    NotADirectory,
    IsADirectory,
    /// A cluster number was out of range, or a chain looped.
    CorruptChain,
}

/// A mounted volume: everything needed to translate clusters to sector
/// numbers. Small and `Copy`, so callers take a snapshot rather than holding
/// the lock across disk I/O.
#[derive(Clone, Copy)]
struct Volume {
    drive: u8,
    sectors_per_cluster: u32,
    fat_start: u32,
    root_start: u32,
    root_sectors: u32,
    data_start: u32,
    cluster_count: u32,
}

static VOLUME: Mutex<Option<Volume>> = Mutex::new(None);

/// Where a directory's entries live: the fixed root region, or a cluster
/// chain (every directory except the root).
#[derive(Clone, Copy)]
enum Dir {
    Root,
    Cluster(u16),
}

/// One 8.3 directory entry, name already formatted.
#[derive(Clone, Copy)]
struct Entry {
    name: [u8; 12],
    name_len: usize,
    attr: u8,
    cluster: u16,
    size: u32,
}

impl Entry {
    fn name(&self) -> &str {
        str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }

    fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }
}

/// Summary of the mounted volume, for the shell's `disk` command.
#[derive(Clone, Copy)]
pub struct Info {
    pub drive: u8,
    pub cluster_count: u32,
    pub bytes_per_cluster: u32,
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

/// Mounts the first attached drive holding a FAT16 volume. Idempotent, so
/// callers can simply call it before each operation.
pub fn mount() -> Result<(), FsError> {
    if VOLUME.lock().is_some() {
        return Ok(());
    }
    // Drive 0 is normally the boot disk (the kernel image, not a filesystem);
    // `probe` rejects it and we fall through to the data disk on drive 1.
    for drive in [0u8, 1u8] {
        if ata::identify(drive).is_err() {
            continue;
        }
        if let Ok(volume) = probe(drive) {
            *VOLUME.lock() = Some(volume);
            return Ok(());
        }
    }
    Err(FsError::NoFilesystem)
}

/// Reads and validates a boot sector, deriving the volume layout.
///
/// The validation is strict on purpose: this same routine is pointed at the
/// boot disk, whose sector 0 also ends in 0x55AA, and must reject it.
fn probe(drive: u8) -> Result<Volume, FsError> {
    let mut boot = [0u8; SECTOR_SIZE];
    ata::read_sector(drive, 0, &mut boot).map_err(FsError::Io)?;

    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(FsError::NoFilesystem);
    }
    // The filesystem-type string is advisory per the spec, but it is what
    // distinguishes a FAT boot sector from any other bootable sector.
    if &boot[54..58] != b"FAT1" {
        return Err(FsError::NoFilesystem);
    }

    let bytes_per_sector = u16_at(&boot, 11) as u32;
    let sectors_per_cluster = boot[13] as u32;
    let reserved_sectors = u16_at(&boot, 14) as u32;
    let num_fats = boot[16] as u32;
    let root_entries = u16_at(&boot, 17) as u32;
    let total_sectors_16 = u16_at(&boot, 19) as u32;
    let sectors_per_fat = u16_at(&boot, 22) as u32;
    let total_sectors_32 = u32_at(&boot, 32);

    if bytes_per_sector as usize != SECTOR_SIZE
        || sectors_per_cluster == 0
        || !sectors_per_cluster.is_power_of_two()
        || sectors_per_cluster > 128
        || reserved_sectors == 0
        || num_fats == 0
        || num_fats > 2
        || sectors_per_fat == 0
    {
        return Err(FsError::NoFilesystem);
    }
    // A zero root-entry count means FAT32, which lays out its root directory
    // as a cluster chain and is not supported here.
    if root_entries == 0 || (root_entries * DIR_ENTRY_SIZE as u32) % bytes_per_sector != 0 {
        return Err(FsError::NoFilesystem);
    }

    let total_sectors = if total_sectors_16 != 0 {
        total_sectors_16
    } else {
        total_sectors_32
    };
    let root_sectors = root_entries * DIR_ENTRY_SIZE as u32 / bytes_per_sector;
    let root_start = reserved_sectors + num_fats * sectors_per_fat;
    let data_start = root_start + root_sectors;
    if total_sectors <= data_start {
        return Err(FsError::NoFilesystem);
    }
    let cluster_count = (total_sectors - data_start) / sectors_per_cluster;
    if !(FAT16_MIN_CLUSTERS..=FAT16_MAX_CLUSTERS).contains(&cluster_count) {
        return Err(FsError::NoFilesystem); // FAT12 or FAT32
    }
    // The FAT must be large enough to describe every cluster.
    if (cluster_count + 2) * 2 > sectors_per_fat * bytes_per_sector {
        return Err(FsError::NoFilesystem);
    }

    Ok(Volume {
        drive,
        sectors_per_cluster,
        fat_start: reserved_sectors,
        root_start,
        root_sectors,
        data_start,
        cluster_count,
    })
}

fn volume() -> Result<Volume, FsError> {
    VOLUME.lock().ok_or(FsError::NoFilesystem)
}

/// Mounts if necessary and returns a summary of the volume.
pub fn info() -> Result<Info, FsError> {
    mount()?;
    let volume = volume()?;
    Ok(Info {
        drive: volume.drive,
        cluster_count: volume.cluster_count,
        bytes_per_cluster: volume.sectors_per_cluster * SECTOR_SIZE as u32,
    })
}

/// First sector of a cluster.
fn cluster_lba(volume: &Volume, cluster: u16) -> u32 {
    volume.data_start + (cluster as u32 - 2) * volume.sectors_per_cluster
}

/// Follows one link of a cluster chain. `Ok(None)` marks the end.
fn next_cluster(volume: &Volume, cluster: u16) -> Result<Option<u16>, FsError> {
    let offset = cluster as u32 * 2;
    let lba = volume.fat_start + offset / SECTOR_SIZE as u32;
    let index = (offset % SECTOR_SIZE as u32) as usize;

    let mut sector = [0u8; SECTOR_SIZE];
    ata::read_sector(volume.drive, lba, &mut sector).map_err(FsError::Io)?;

    let value = u16_at(&sector, index);
    if value >= CHAIN_END {
        return Ok(None);
    }
    if value < 2 || value as u32 >= volume.cluster_count + 2 {
        return Err(FsError::CorruptChain);
    }
    Ok(Some(value))
}

/// Turns a raw 32-byte directory entry into an `Entry`, formatting the padded
/// 8.3 name (`"HELLO   TXT"`) into a conventional one (`"HELLO.TXT"`).
fn parse_entry(raw: &[u8]) -> Entry {
    let mut name = [0u8; 12];
    let mut len = 0;

    for &byte in &raw[0..8] {
        if byte == b' ' {
            break;
        }
        name[len] = byte;
        len += 1;
    }

    let ext_len = raw[8..11].iter().take_while(|&&b| b != b' ').count();
    if ext_len > 0 {
        name[len] = b'.';
        len += 1;
        for &byte in &raw[8..8 + ext_len] {
            name[len] = byte;
            len += 1;
        }
    }

    Entry {
        name,
        name_len: len,
        attr: raw[11],
        cluster: u16_at(raw, 26),
        size: u32_at(raw, 28),
    }
}

/// Walks one sector of directory entries. Returns `false` once the caller is
/// done, or the end-of-directory marker is reached.
fn scan_sector(sector: &[u8; SECTOR_SIZE], visit: &mut impl FnMut(&Entry) -> bool) -> bool {
    for raw in sector.chunks_exact(DIR_ENTRY_SIZE) {
        match raw[0] {
            0x00 => return false, // no entries beyond this point, ever
            ENTRY_FREE => continue,
            _ => {}
        }
        let attr = raw[11];
        if attr & ATTR_LONG_NAME == ATTR_LONG_NAME || attr & ATTR_VOLUME_ID != 0 {
            continue; // long-name fragment or volume label
        }
        if !visit(&parse_entry(raw)) {
            return false;
        }
    }
    true
}

/// Iterates a directory's entries, wherever they live.
fn visit_dir(
    volume: &Volume,
    dir: Dir,
    visit: &mut impl FnMut(&Entry) -> bool,
) -> Result<(), FsError> {
    let mut sector = [0u8; SECTOR_SIZE];
    match dir {
        Dir::Root => {
            for i in 0..volume.root_sectors {
                ata::read_sector(volume.drive, volume.root_start + i, &mut sector)
                    .map_err(FsError::Io)?;
                if !scan_sector(&sector, visit) {
                    return Ok(());
                }
            }
        }
        Dir::Cluster(first) => {
            let mut cluster = first;
            if cluster < 2 || cluster as u32 >= volume.cluster_count + 2 {
                return Err(FsError::CorruptChain);
            }
            for _ in 0..=volume.cluster_count {
                let lba = cluster_lba(volume, cluster);
                for i in 0..volume.sectors_per_cluster {
                    ata::read_sector(volume.drive, lba + i, &mut sector).map_err(FsError::Io)?;
                    if !scan_sector(&sector, visit) {
                        return Ok(());
                    }
                }
                match next_cluster(volume, cluster)? {
                    Some(next) => cluster = next,
                    None => return Ok(()),
                }
            }
            return Err(FsError::CorruptChain); // chain longer than the volume
        }
    }
    Ok(())
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

fn find_entry(volume: &Volume, dir: Dir, name: &str) -> Result<Entry, FsError> {
    let mut found = None;
    visit_dir(volume, dir, &mut |entry| {
        if eq_ignore_case(entry.name(), name) {
            found = Some(*entry);
            false
        } else {
            true
        }
    })?;
    found.ok_or(FsError::NotFound)
}

/// Resolves a path to its final entry. `Ok(None)` means the path denotes the
/// root directory, which has no directory entry of its own.
fn resolve(volume: &Volume, path: &str) -> Result<Option<Entry>, FsError> {
    let mut current: Option<Entry> = None;

    for component in path
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
    {
        let dir = match current {
            None => Dir::Root,
            Some(entry) if !entry.is_dir() => return Err(FsError::NotADirectory),
            // A ".." entry in a top-level directory points at cluster 0,
            // which is the spec's way of naming the root.
            Some(entry) if entry.cluster == 0 => Dir::Root,
            Some(entry) => Dir::Cluster(entry.cluster),
        };
        current = Some(find_entry(volume, dir, component)?);
    }

    Ok(current)
}

/// Lists a directory, invoking `visit(name, is_dir, size)` per entry and
/// returning the number of entries. `.` and `..` are hidden.
pub fn list(path: &str, mut visit: impl FnMut(&str, bool, u32)) -> Result<u32, FsError> {
    mount()?;
    let volume = volume()?;

    let dir = match resolve(&volume, path)? {
        None => Dir::Root,
        Some(entry) if !entry.is_dir() => return Err(FsError::NotADirectory),
        Some(entry) if entry.cluster == 0 => Dir::Root,
        Some(entry) => Dir::Cluster(entry.cluster),
    };

    let mut count = 0;
    visit_dir(&volume, dir, &mut |entry| {
        let name = entry.name();
        if name != "." && name != ".." {
            count += 1;
            visit(name, entry.is_dir(), entry.size);
        }
        true
    })?;
    Ok(count)
}

/// Reads a file, handing each chunk to `visit` in order, and returns the
/// number of bytes delivered. Chunks are at most one sector; nothing is
/// buffered, so file size is irrelevant to memory use.
pub fn read_file(path: &str, mut visit: impl FnMut(&[u8])) -> Result<u32, FsError> {
    mount()?;
    let volume = volume()?;

    let entry = resolve(&volume, path)?.ok_or(FsError::IsADirectory)?;
    if entry.is_dir() {
        return Err(FsError::IsADirectory);
    }
    if entry.size == 0 {
        return Ok(0);
    }
    let mut cluster = entry.cluster;
    if cluster < 2 || cluster as u32 >= volume.cluster_count + 2 {
        return Err(FsError::CorruptChain);
    }

    let mut remaining = entry.size;
    let mut sector = [0u8; SECTOR_SIZE];
    for _ in 0..=volume.cluster_count {
        let lba = cluster_lba(&volume, cluster);
        for i in 0..volume.sectors_per_cluster {
            if remaining == 0 {
                return Ok(entry.size);
            }
            ata::read_sector(volume.drive, lba + i, &mut sector).map_err(FsError::Io)?;
            let take = remaining.min(SECTOR_SIZE as u32) as usize;
            visit(&sector[..take]);
            remaining -= take as u32;
        }
        if remaining == 0 {
            return Ok(entry.size);
        }
        match next_cluster(&volume, cluster)? {
            Some(next) => cluster = next,
            // The chain ended before the file's recorded size was delivered.
            None => return Err(FsError::CorruptChain),
        }
    }
    Err(FsError::CorruptChain)
}
