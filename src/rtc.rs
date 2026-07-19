//! Wall-clock time from the CMOS real-time clock.
//!
//! The RTC is reached through the CMOS index/data port pair (0x70/0x71): write
//! a register number to 0x70, then read its value from 0x71.
//!
//! Two quirks make this fiddlier than it looks:
//!
//! * The chip may be mid-update when read, yielding a torn timestamp. We wait
//!   for the "update in progress" flag to clear and then read twice, accepting
//!   the value only once two consecutive reads agree.
//! * Values may be BCD or binary, and hours may be 12- or 24-hour, depending on
//!   status register B. Both are handled below.

use core::fmt;

use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

// CMOS register numbers.
const REG_SECOND: u8 = 0x00;
const REG_MINUTE: u8 = 0x02;
const REG_HOUR: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_CENTURY: u8 = 0x32;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

/// Upper bound on spin iterations, so a misbehaving RTC can't hang the kernel.
const MAX_SPINS: u32 = 1_000_000;

/// A wall-clock date and time, as reported by the RTC.
///
/// The RTC has no notion of time zone; on QEMU (and most systems) this is UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Raw register values, before BCD/12-hour normalisation.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Raw {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
}

/// Reads a single CMOS register.
///
/// Callers must hold interrupts off across an index/data pair, otherwise an
/// interrupt handler could retarget the index port between the two accesses.
unsafe fn cmos_read(reg: u8) -> u8 {
    let mut index: Port<u8> = Port::new(CMOS_INDEX);
    let mut data: Port<u8> = Port::new(CMOS_DATA);
    unsafe {
        index.write(reg);
        data.read()
    }
}

/// True while the RTC is updating its registers.
unsafe fn update_in_progress() -> bool {
    unsafe { cmos_read(REG_STATUS_A) & 0x80 != 0 }
}

/// Reads all time registers once, after waiting out any in-progress update.
unsafe fn read_raw() -> Raw {
    unsafe {
        let mut spins = 0;
        while update_in_progress() && spins < MAX_SPINS {
            spins += 1;
        }

        Raw {
            second: cmos_read(REG_SECOND),
            minute: cmos_read(REG_MINUTE),
            hour: cmos_read(REG_HOUR),
            day: cmos_read(REG_DAY),
            month: cmos_read(REG_MONTH),
            year: cmos_read(REG_YEAR),
            century: cmos_read(REG_CENTURY),
        }
    }
}

/// Converts a binary-coded-decimal byte to binary (e.g. 0x25 -> 25).
fn bcd_to_binary(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
}

/// Reads the current wall-clock time from the RTC.
pub fn read() -> DateTime {
    interrupts::without_interrupts(|| unsafe {
        // Read repeatedly until two consecutive reads agree, so we never
        // return a timestamp torn across an update.
        let mut previous = read_raw();
        let mut spins = 0;
        let raw = loop {
            let current = read_raw();
            if current == previous || spins >= MAX_SPINS {
                break current;
            }
            previous = current;
            spins += 1;
        };

        let status_b = cmos_read(REG_STATUS_B);
        let is_binary = status_b & 0x04 != 0;
        let is_24_hour = status_b & 0x02 != 0;

        // The high bit of the hour register is the PM flag in 12-hour mode;
        // strip it before any BCD conversion.
        let pm = raw.hour & 0x80 != 0;
        let hour_bits = raw.hour & 0x7F;

        let convert = |value: u8| if is_binary { value } else { bcd_to_binary(value) };

        let mut hour = convert(hour_bits);
        if !is_24_hour {
            if pm {
                hour = (hour % 12) + 12; // 12 PM stays 12, 1 PM -> 13
            } else if hour == 12 {
                hour = 0; // 12 AM -> 00
            }
        }

        // The century register isn't universally implemented; fall back to the
        // 21st century if it reports something implausible.
        let century = match convert(raw.century) {
            c @ 19..=21 => c as u16,
            _ => 20,
        };

        DateTime {
            year: century * 100 + convert(raw.year) as u16,
            month: convert(raw.month),
            day: convert(raw.day),
            hour,
            minute: convert(raw.minute),
            second: convert(raw.second),
        }
    })
}
