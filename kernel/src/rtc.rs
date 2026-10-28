//! Wall-clock time kept in software: a reference date plus the kernel tick.
//! Set by the user (Settings) or later by the network. Lost on reset.

use crate::{irq, time};
use core::cell::UnsafeCell;
use p4os_sdk::DateTime;

struct Base(UnsafeCell<Option<(i64, u32)>>);
unsafe impl Sync for Base {}

/// (unix seconds, kernel tick in ms) at the moment the clock was set.
static BASE: Base = Base(UnsafeCell::new(None));

pub fn set(t: DateTime) {
    let secs = to_unix(t);
    irq::free(|| unsafe { *BASE.0.get() = Some((secs, time::ticks())) });
}

pub fn now() -> Option<DateTime> {
    let (secs, tick) = irq::free(|| unsafe { *BASE.0.get() })?;
    let elapsed = time::ticks().wrapping_sub(tick) as i64 / 1000;
    Some(from_unix(secs + elapsed))
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn to_unix(t: DateTime) -> i64 {
    days_from_civil(t.year as i64, t.month as i64, t.day as i64) * 86_400
        + t.hour as i64 * 3600
        + t.minute as i64 * 60
        + t.second as i64
}

fn from_unix(secs: i64) -> DateTime {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as u16;
    DateTime { year, month, day, hour: (rem / 3600) as u8, minute: (rem / 60 % 60) as u8, second: (rem % 60) as u8 }
}
