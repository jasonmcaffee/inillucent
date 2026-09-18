//! The machine's local time zone, for the `utc` and `localtime` modifiers.
//!
//! Invariant: **this is a question only the operating system can answer, so it
//! is asked here.** The offset between local time and UTC is not a constant -
//! it changes at a daylight saving boundary and it has changed by legislation -
//! so it is asked for one instant at a time rather than read once and kept. The
//! date and time built-ins in `inillucent-scalar` call this; the module doc
//! there records why the two modifiers used to answer NULL instead.
//!
//! Neither implementation reads a time zone database of its own. On Unix it is
//! `localtime_r`, which is what SQLite's `date.c` calls; on Windows it is
//! `SystemTimeToTzSpecificLocalTime` against the zone the process is running
//! in, which is what the C runtime's `localtime` calls underneath.

/// Returns the local zone's offset from UTC at one instant, in seconds east.
///
/// A positive answer means local time is ahead of UTC. `None` means the
/// operating system would not answer - an instant outside the range its
/// conversion accepts, or a machine with no zone configured - and the caller
/// answers NULL rather than guessing an offset.
///
/// @param utc_seconds - the instant, in seconds since 1970-01-01 00:00 UTC
#[cfg(unix)]
pub fn local_offset_seconds(utc_seconds: i64) -> Option<i64> {
    let instant = utc_seconds as libc::time_t;
    // SAFETY: `libc::tm` is a plain C struct of integers and a pointer, and a
    // zeroed one is what every caller of `localtime_r` hands it. Its fields
    // differ between platforms, so there is no field list to write out here
    // that would compile everywhere.
    let mut broken: libc::tm = unsafe { core::mem::zeroed() };
    // SAFETY: `localtime_r` writes into a `tm` this call owns and reads one
    // `time_t` by pointer. It is the reentrant form on purpose: the shared
    // buffer `localtime` returns is not safe to hand back from a connection
    // that may be one of several in a process.
    let answered = unsafe { libc::localtime_r(&instant, &mut broken) };
    if answered.is_null() {
        return None;
    }
    Some(broken.tm_gmtoff as i64)
}

/// The seconds between 1601-01-01, where a Windows FILETIME starts, and
/// 1970-01-01, where Unix time starts.
#[cfg(windows)]
const FILETIME_EPOCH_OFFSET: i64 = 11_644_473_600;

/// The 100-nanosecond ticks in one second, which is a FILETIME's unit.
#[cfg(windows)]
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Returns the local zone's offset from UTC at one instant, in seconds east.
///
/// The offset is measured rather than read: the instant is converted to local
/// time through the zone the process is running in and back to a FILETIME, and
/// the difference between the two FILETIMEs is the offset. That is one call
/// against the zone's own rules, so a daylight saving boundary and a historical
/// rule change are both accounted for without this module knowing either.
///
/// @param utc_seconds - the instant, in seconds since 1970-01-01 00:00 UTC
#[cfg(windows)]
pub fn local_offset_seconds(utc_seconds: i64) -> Option<i64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    };
    let ticks = utc_seconds
        .checked_add(FILETIME_EPOCH_OFFSET)?
        .checked_mul(TICKS_PER_SECOND)?;
    if ticks < 0 {
        return None;
    }
    let held = ticks as u64;
    let universal = FILETIME {
        dwLowDateTime: held as u32,
        dwHighDateTime: (held >> 32) as u32,
    };
    let mut broken = empty_system_time();
    let mut local = empty_system_time();
    let mut converted = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: every pointer is to a local of the right type, and the null
    // zone argument is what asks for the process's own zone rather than a
    // caller-supplied one. Each call reports failure through its return value,
    // which is checked before the output is read.
    let answered = unsafe {
        FileTimeToSystemTime(&universal, &mut broken) != 0
            && SystemTimeToTzSpecificLocalTime(core::ptr::null(), &broken, &mut local) != 0
            && SystemTimeToFileTime(&local, &mut converted) != 0
    };
    if !answered {
        return None;
    }
    let after = (i64::from(converted.dwHighDateTime) << 32) | i64::from(converted.dwLowDateTime);
    Some((after - ticks) / TICKS_PER_SECOND)
}

/// Returns a zeroed `SYSTEMTIME`, for a call that is about to fill one.
///
/// Written out field by field rather than zeroed through `core::mem`, so that
/// nothing in this module needs `unsafe` for a struct of eight integers.
#[cfg(windows)]
fn empty_system_time() -> windows_sys::Win32::Foundation::SYSTEMTIME {
    windows_sys::Win32::Foundation::SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    }
}

/// Returns `None`, because this target has no zone lookup.
///
/// @param utc_seconds - the instant, in seconds since 1970-01-01 00:00 UTC
#[cfg(not(any(unix, windows)))]
pub fn local_offset_seconds(utc_seconds: i64) -> Option<i64> {
    let _ = utc_seconds;
    None
}

#[cfg(test)]
mod tests {
    use super::local_offset_seconds;

    /// 2020-01-01 12:00 UTC, the winter instant both tests ask about.
    const WINTER: i64 = 1_577_880_000;

    /// 2020-07-01 12:00 UTC, the summer one.
    const SUMMER: i64 = 1_593_604_800;

    /// The offset is a whole number of minutes, and one a zone can hold.
    ///
    /// The value itself is the machine's and cannot be written down here, so
    /// what is asserted is the shape every real zone has: an answer at all,
    /// between UTC-12 and UTC+14, and a whole minute. A lookup that gave up
    /// would fail the first of those rather than skip the test.
    #[test]
    fn the_offset_is_a_whole_minute_inside_the_range_zones_use() {
        let offset = local_offset_seconds(WINTER).expect("the machine has a zone");
        assert_eq!(offset % 60, 0, "the offset is {offset} seconds");
        assert!(
            (-12 * 3600..=14 * 3600).contains(&offset),
            "the offset is {offset} seconds"
        );
    }

    /// Two instants six months apart differ by at most one hour.
    ///
    /// A zone that observes daylight saving answers two different offsets for
    /// January and July, and one that does not answers the same twice. Either
    /// is right; what would be wrong is a difference larger than any daylight
    /// saving rule uses, which is what a lookup that read the wrong field or
    /// the wrong instant would give.
    #[test]
    fn a_summer_instant_and_a_winter_one_differ_by_at_most_an_hour() {
        let winter = local_offset_seconds(WINTER).expect("the machine has a zone");
        let summer = local_offset_seconds(SUMMER).expect("the machine has a zone");
        assert!(
            (winter - summer).abs() <= 3600,
            "winter {winter} and summer {summer} are further apart than a daylight saving rule"
        );
    }
}
