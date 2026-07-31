//! Kernel-maintained wall clock.
//!
//! The CMOS RTC is read exactly once, at boot, to establish a reference
//! point; from then on the wall-clock time is derived from the PIT tick
//! counter. Querying the time therefore costs an atomic load and some
//! calendar arithmetic — no port I/O, no spinning on RTC update flags.
//!
//! Calendar conversions use Howard Hinnant's `days_from_civil` /
//! `civil_from_days` algorithms (public domain, proven in `<chrono>`).

use conquer_once::spin::OnceCell;

use crate::rtc::{self, DateTime};
use crate::time;

const SECONDS_PER_DAY: i64 = 86_400;

/// The wall-clock time and tick count captured together at boot.
struct BootReference {
    unix_seconds: i64,
    ticks: u64,
}

static BOOT_REFERENCE: OnceCell<BootReference> = OnceCell::uninit();

/// Seeds the clock from the RTC. Called once during kernel init, after the
/// PIT has started ticking.
pub fn init() {
    BOOT_REFERENCE
        .try_init_once(|| BootReference {
            unix_seconds: to_unix_seconds(rtc::read()),
            ticks: time::ticks(),
        })
        .expect("clock::init should only be called once");
}

/// The current wall-clock time (UTC), advanced from the boot reference by
/// elapsed PIT ticks. Falls back to reading the RTC directly if the clock
/// was never seeded.
pub fn now() -> DateTime {
    match BOOT_REFERENCE.try_get() {
        Ok(reference) => {
            let elapsed_seconds = (time::ticks() - reference.ticks) / time::TIMER_HZ;
            from_unix_seconds(reference.unix_seconds + elapsed_seconds as i64)
        }
        Err(_) => rtc::read(),
    }
}

/// Days from 1970-01-01 to the given civil date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Civil `(year, month, day)` from days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

fn to_unix_seconds(dt: DateTime) -> i64 {
    days_from_civil(dt.year as i64, dt.month as i64, dt.day as i64) * SECONDS_PER_DAY
        + dt.hour as i64 * 3600
        + dt.minute as i64 * 60
        + dt.second as i64
}

fn from_unix_seconds(seconds: i64) -> DateTime {
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    DateTime {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        hour: (seconds_of_day / 3600) as u8,
        minute: (seconds_of_day % 3600 / 60) as u8,
        second: (seconds_of_day % 60) as u8,
    }
}

// Tests

#[test_case]
fn unix_epoch_is_zero() {
    let epoch = DateTime {
        year: 1970,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    };
    assert_eq!(to_unix_seconds(epoch), 0);
    assert_eq!(from_unix_seconds(0), epoch);
}

#[test_case]
fn known_timestamp_round_trips() {
    // `date -u -d "2026-07-31 12:34:56"` -> 1785501296
    let dt = DateTime {
        year: 2026,
        month: 7,
        day: 31,
        hour: 12,
        minute: 34,
        second: 56,
    };
    assert_eq!(to_unix_seconds(dt), 1_785_501_296);
    assert_eq!(from_unix_seconds(1_785_501_296), dt);
}

#[test_case]
fn leap_day_round_trips() {
    // 2024-02-29 00:00:00 UTC -> 1709164800
    let dt = DateTime {
        year: 2024,
        month: 2,
        day: 29,
        hour: 0,
        minute: 0,
        second: 0,
    };
    assert_eq!(to_unix_seconds(dt), 1_709_164_800);
    assert_eq!(from_unix_seconds(1_709_164_800), dt);
}

#[test_case]
fn year_boundary_round_trips() {
    // 2025-12-31 23:59:59 UTC -> 1767225599
    let dt = DateTime {
        year: 2025,
        month: 12,
        day: 31,
        hour: 23,
        minute: 59,
        second: 59,
    };
    assert_eq!(to_unix_seconds(dt), 1_767_225_599);
    assert_eq!(from_unix_seconds(1_767_225_599), dt);
}
