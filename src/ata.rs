//! ATA PIO disk driver (primary bus), polling mode.
//!
//! Reads sectors by writing an LBA and a command to the task-file registers,
//! then polling the status port and pulling 256 16-bit words out of the data
//! port. Polling rather than IRQ 14 keeps the driver synchronous and simple:
//! a read is an ordinary blocking function call, with no handler, no wakers,
//! and no state machine.
//!
//! ## Locking, and the rule this file deliberately breaks
//!
//! Everywhere else in this kernel, a lock shared with an interrupt handler is
//! taken inside `without_interrupts`. Here the opposite is correct: no
//! interrupt handler touches ATA, and a sector read can spin for milliseconds
//! waiting on the device. Disabling interrupts for that long would stall the
//! timer — stopping the clock and preemption. So [`BUS`] is a plain spinlock
//! held with interrupts *enabled*: two threads racing for the disk is
//! resolved by preemption, exactly like the heap allocator's lock.
//!
//! The lock is what makes the multi-register command sequence atomic. Without
//! it, two threads interleaving "select drive, set LBA, issue command" would
//! each read the other's sector.

use spin::Mutex;
use x86_64::instructions::port::Port;

pub const SECTOR_SIZE: usize = 512;

const DATA: u16 = 0x1F0;
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LOW: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HIGH: u16 = 0x1F5;
const DRIVE_HEAD: u16 = 0x1F6;
const STATUS_COMMAND: u16 = 0x1F7;
/// Reading this port yields the alternate status, which (unlike 0x1F7) has no
/// side effects, so it is the correct port for timing delays. Writing it
/// targets the device control register.
const ALT_STATUS: u16 = 0x3F6;

/// Device control bit 1 (nIEN): stop the device asserting its IRQ line. This
/// driver polls, so completion interrupts are pure noise — and an unhandled
/// one escalates into a double fault.
const CTRL_NIEN: u8 = 0x02;

const ST_ERR: u8 = 0x01;
const ST_DRQ: u8 = 0x08;
const ST_DF: u8 = 0x20;
const ST_BSY: u8 = 0x80;

const CMD_READ_SECTORS: u8 = 0x20;
const CMD_IDENTIFY: u8 = 0xEC;

/// Ceiling on any hardware wait, so a wedged or absent device degrades into
/// an error instead of hanging the kernel.
const MAX_SPINS: u32 = 1_000_000;

/// LBA28 addressing tops out here; beyond it a real driver needs LBA48.
const MAX_LBA28: u32 = 0x0FFF_FFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtaError {
    /// Nothing responded on this drive number.
    NoDevice,
    /// Something responded, but it is not an ATA disk (e.g. an ATAPI CD-ROM).
    NotAta,
    /// The device never became ready.
    Timeout,
    /// The device reported ERR or a device fault.
    DeviceError,
    /// Sector past the end of the disk, or beyond LBA28's reach.
    OutOfRange,
}

/// Guards the primary ATA bus. Zero-sized: the register ports are recreated
/// per operation (a `Port` is just a port number, so this is free).
struct Bus {
    _private: (),
}

static BUS: Mutex<Bus> = Mutex::new(Bus { _private: () });

struct Regs {
    data: Port<u16>,
    sector_count: Port<u8>,
    lba_low: Port<u8>,
    lba_mid: Port<u8>,
    lba_high: Port<u8>,
    drive_head: Port<u8>,
    status_command: Port<u8>,
    alt_status: Port<u8>,
    device_control: Port<u8>,
}

impl Regs {
    fn new() -> Self {
        Regs {
            data: Port::new(DATA),
            sector_count: Port::new(SECTOR_COUNT),
            lba_low: Port::new(LBA_LOW),
            lba_mid: Port::new(LBA_MID),
            lba_high: Port::new(LBA_HIGH),
            drive_head: Port::new(DRIVE_HEAD),
            status_command: Port::new(STATUS_COMMAND),
            alt_status: Port::new(ALT_STATUS),
            device_control: Port::new(ALT_STATUS),
        }
    }

    /// The device needs ~400 ns after a drive select before its status is
    /// meaningful; four alternate-status reads is the conventional way to
    /// spend that time.
    fn delay_400ns(&mut self) {
        for _ in 0..4 {
            unsafe { self.alt_status.read() };
        }
    }

    fn status(&mut self) -> u8 {
        unsafe { self.status_command.read() }
    }

    /// Selects a drive and silences its interrupt line. nIEN is a per-device
    /// setting, so it is reasserted on every select.
    fn select(&mut self, value: u8) {
        unsafe {
            self.drive_head.write(value);
            self.device_control.write(CTRL_NIEN);
        }
        self.delay_400ns();
    }

    /// Spins until BSY clears, then returns the status byte.
    fn wait_not_busy(&mut self) -> Result<u8, AtaError> {
        for _ in 0..MAX_SPINS {
            let status = self.status();
            if status & ST_BSY == 0 {
                return Ok(status);
            }
        }
        Err(AtaError::Timeout)
    }

    /// Spins until the device has data to transfer, failing fast on errors.
    fn wait_for_data(&mut self) -> Result<(), AtaError> {
        for _ in 0..MAX_SPINS {
            let status = self.status();
            if status & (ST_ERR | ST_DF) != 0 {
                return Err(AtaError::DeviceError);
            }
            if status & ST_BSY == 0 && status & ST_DRQ != 0 {
                return Ok(());
            }
        }
        Err(AtaError::Timeout)
    }

    /// Reads one sector's worth of 16-bit words out of the data port.
    fn read_data(&mut self, buf: &mut [u8; SECTOR_SIZE]) {
        for chunk in buf.chunks_exact_mut(2) {
            let word = unsafe { self.data.read() };
            chunk[0] = word as u8;
            chunk[1] = (word >> 8) as u8;
        }
    }
}

impl Bus {
    /// Issues IDENTIFY and returns the drive's LBA28 sector count.
    fn identify(&mut self, drive: u8) -> Result<u32, AtaError> {
        let mut regs = Regs::new();
        regs.select(0xA0 | (drive & 1) << 4);
        unsafe {
            regs.sector_count.write(0);
            regs.lba_low.write(0);
            regs.lba_mid.write(0);
            regs.lba_high.write(0);
            regs.status_command.write(CMD_IDENTIFY);
        }

        // Status 0 means nothing is attached; 0xFF is the "floating bus" a
        // pull-up resistor produces when no device drives the lines at all.
        let status = regs.status();
        if status == 0 || status == 0xFF {
            return Err(AtaError::NoDevice);
        }
        regs.wait_not_busy()?;

        // A non-zero LBA mid/high after IDENTIFY is the device signalling
        // that it speaks ATAPI (or SATA), not ATA.
        let (mid, high) = unsafe { (regs.lba_mid.read(), regs.lba_high.read()) };
        if mid != 0 || high != 0 {
            return Err(AtaError::NotAta);
        }

        regs.wait_for_data()?;
        let mut identity = [0u8; SECTOR_SIZE];
        regs.read_data(&mut identity);

        // Words 60..61 hold the total addressable sectors in LBA28 mode.
        let sectors = u32::from_le_bytes([
            identity[120],
            identity[121],
            identity[122],
            identity[123],
        ]);
        Ok(sectors)
    }

    fn read_sector(
        &mut self,
        drive: u8,
        lba: u32,
        buf: &mut [u8; SECTOR_SIZE],
    ) -> Result<(), AtaError> {
        if lba > MAX_LBA28 {
            return Err(AtaError::OutOfRange);
        }
        let mut regs = Regs::new();
        // Drive/head register: LBA mode, drive number, and the top four
        // address bits.
        regs.select(0xE0 | (drive & 1) << 4 | ((lba >> 24) & 0x0F) as u8);
        unsafe {
            regs.sector_count.write(1);
            regs.lba_low.write(lba as u8);
            regs.lba_mid.write((lba >> 8) as u8);
            regs.lba_high.write((lba >> 16) as u8);
            regs.status_command.write(CMD_READ_SECTORS);
        }

        let status = regs.status();
        if status == 0 || status == 0xFF {
            return Err(AtaError::NoDevice);
        }
        regs.wait_for_data()?;
        regs.read_data(buf);
        Ok(())
    }
}

/// Probes `drive` (0 = master, 1 = slave) and returns its sector count.
pub fn identify(drive: u8) -> Result<u32, AtaError> {
    BUS.lock().identify(drive)
}

/// Reads a single 512-byte sector. Blocks (with interrupts enabled) until the
/// device delivers the data or the spin ceiling is hit.
pub fn read_sector(drive: u8, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), AtaError> {
    BUS.lock().read_sector(drive, lba, buf)
}
