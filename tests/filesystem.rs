//! Exercises the ATA driver and FAT16 parser against the real `disk.img`
//! attached by `cargo test` (see `test-args` in Cargo.toml). Every assertion
//! is against content written by `tools/mkfatimg.py`.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(rust_os::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader::{entry_point, BootInfo};
use rust_os::fat::{self, FsError};

entry_point!(main);

fn main(_boot_info: &'static BootInfo) -> ! {
    rust_os::init();
    test_main();
    loop {}
}

/// Collects a file into a fixed buffer (no heap in this test binary).
struct Sink {
    buf: [u8; 4096],
    len: usize,
}

impl Sink {
    fn new() -> Self {
        Sink {
            buf: [0; 4096],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).expect("file was not valid UTF-8")
    }
}

#[test_case]
fn mounts_the_fat16_volume() {
    fat::mount().expect("no FAT16 volume found");
    let info = fat::info().expect("info after mount");
    assert_eq!(info.bytes_per_cluster, 512);
    // Below 4085 clusters it would be FAT12, above 65524 FAT32.
    assert!(info.cluster_count >= 4085 && info.cluster_count <= 65524);
}

#[test_case]
fn lists_the_root_directory() {
    let mut seen_hello = false;
    let mut seen_docs_dir = false;
    let count = fat::list("/", |name, is_dir, size| {
        if name == "HELLO.TXT" {
            seen_hello = true;
            assert!(!is_dir);
            assert_eq!(size, 33);
        }
        if name == "DOCS" {
            seen_docs_dir = true;
            assert!(is_dir);
        }
    })
    .expect("listing the root directory");

    assert!(seen_hello, "HELLO.TXT missing from the root directory");
    assert!(seen_docs_dir, "DOCS/ missing from the root directory");
    // HELLO.TXT, README.TXT, POEM.TXT, DOCS, BIN -- the volume label is not
    // a directory entry.
    assert_eq!(count, 5);
}

#[test_case]
fn reads_a_single_cluster_file() {
    let mut sink = Sink::new();
    let size = fat::read_file("/HELLO.TXT", |chunk| {
        sink.buf[sink.len..sink.len + chunk.len()].copy_from_slice(chunk);
        sink.len += chunk.len();
    })
    .expect("reading HELLO.TXT");

    assert_eq!(size, 33);
    assert_eq!(sink.len, 33);
    assert_eq!(sink.as_str(), "Hello from the FAT16 filesystem!\n");
}

#[test_case]
fn reads_a_file_spanning_a_cluster_chain() {
    // POEM.TXT is 1960 bytes over four 512-byte clusters, so reading it
    // correctly requires following FAT links rather than reading one cluster.
    let mut sink = Sink::new();
    let size = fat::read_file("/POEM.TXT", |chunk| {
        sink.buf[sink.len..sink.len + chunk.len()].copy_from_slice(chunk);
        sink.len += chunk.len();
    })
    .expect("reading POEM.TXT");

    assert_eq!(size, 1960);
    assert_eq!(sink.len, 1960);
    let text = sink.as_str();
    assert!(text.starts_with("001: the quick brown fox"));
    assert!(text.ends_with("040: the quick brown fox jumps over the lazy dog\n"));
    // The 21st line starts in the third cluster, so this byte only lands here
    // if the chain was walked in the right order.
    assert!(text.contains("021: the quick"));
}

#[test_case]
fn reads_through_a_subdirectory() {
    let mut seen_notes = false;
    fat::list("/DOCS", |name, _is_dir, _size| {
        if name == "NOTES.TXT" {
            seen_notes = true;
        }
    })
    .expect("listing /DOCS");
    assert!(seen_notes, "NOTES.TXT missing from /DOCS");

    let mut sink = Sink::new();
    fat::read_file("docs/notes.txt", |chunk| {
        sink.buf[sink.len..sink.len + chunk.len()].copy_from_slice(chunk);
        sink.len += chunk.len();
    })
    .expect("reading docs/notes.txt by a lowercase relative path");
    assert!(sink.as_str().starts_with("Notes\n-----"));
}

#[test_case]
fn reports_missing_and_mistyped_paths() {
    assert_eq!(
        fat::read_file("/NOPE.TXT", |_| {}).unwrap_err(),
        FsError::NotFound
    );
    assert_eq!(fat::list("/NOPE", |_, _, _| {}).unwrap_err(), FsError::NotFound);
    // A file is not a directory, and a directory is not a file.
    assert_eq!(
        fat::list("/HELLO.TXT", |_, _, _| {}).unwrap_err(),
        FsError::NotADirectory
    );
    assert_eq!(
        fat::read_file("/DOCS", |_| {}).unwrap_err(),
        FsError::IsADirectory
    );
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    rust_os::test_panic_handler(info)
}
