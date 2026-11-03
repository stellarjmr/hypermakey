//! Sparse, timestamped logging to stderr, which launchd redirects to the service log file.

use std::fmt::{self, Write as _};
use std::io::{self, Write as _};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Writes one line to stderr, prefixed with the local time and its UTC offset.
pub fn log_message(arguments: fmt::Arguments<'_>) {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    let mut line = String::with_capacity(160);
    let _ = match Timestamp::local(since_epoch) {
        Some(timestamp) => write!(line, "{timestamp} "),
        None => write!(
            line,
            "@{}.{:03} ",
            since_epoch.as_secs(),
            since_epoch.subsec_millis()
        ),
    };
    let _ = line.write_fmt(arguments);
    line.push('\n');

    // One write per line keeps lines from different threads intact.
    let _ = io::stderr().lock().write_all(line.as_bytes());
}

/// A wall-clock time in local calendar fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timestamp {
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    millisecond: u32,
    utc_offset_seconds: i64,
}

impl Timestamp {
    #[cfg(target_os = "macos")]
    fn local(since_epoch: Duration) -> Option<Self> {
        let seconds = ffi::TimeT::try_from(since_epoch.as_secs()).ok()?;
        let mut fields = std::mem::MaybeUninit::<ffi::Tm>::uninit();

        // SAFETY: `seconds` is a live time_t and `fields` is writable storage for one `struct tm`.
        let result = unsafe { ffi::localtime_r(&seconds, fields.as_mut_ptr()) };
        if result.is_null() {
            return None;
        }

        // SAFETY: localtime_r returned non-null, so it filled in every field of `fields`.
        let fields = unsafe { fields.assume_init() };
        Some(Self {
            year: fields.tm_year + 1900,
            month: fields.tm_mon + 1,
            day: fields.tm_mday,
            hour: fields.tm_hour,
            minute: fields.tm_min,
            second: fields.tm_sec,
            millisecond: since_epoch.subsec_millis(),
            utc_offset_seconds: fields.tm_gmtoff,
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn local(_since_epoch: Duration) -> Option<Self> {
        None
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.utc_offset_seconds < 0 {
            '-'
        } else {
            '+'
        };
        let offset_minutes = self.utc_offset_seconds.unsigned_abs() / 60;

        write!(
            formatter,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {sign}{:02}{:02}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.millisecond,
            offset_minutes / 60,
            offset_minutes % 60,
        )
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::{c_char, c_int, c_long};

    /// `time_t` is `long` on Darwin.
    pub type TimeT = c_long;

    /// Darwin's `struct tm` from `<time.h>`.
    #[repr(C)]
    pub struct Tm {
        pub tm_sec: c_int,
        pub tm_min: c_int,
        pub tm_hour: c_int,
        pub tm_mday: c_int,
        pub tm_mon: c_int,
        pub tm_year: c_int,
        pub tm_wday: c_int,
        pub tm_yday: c_int,
        pub tm_isdst: c_int,
        pub tm_gmtoff: c_long,
        pub tm_zone: *mut c_char,
    }

    extern "C" {
        pub fn localtime_r(clock: *const TimeT, result: *mut Tm) -> *mut Tm;
    }
}

#[cfg(test)]
mod tests {
    use super::Timestamp;

    fn timestamp(utc_offset_seconds: i64) -> Timestamp {
        Timestamp {
            year: 2026,
            month: 11,
            day: 3,
            hour: 9,
            minute: 5,
            second: 7,
            millisecond: 42,
            utc_offset_seconds,
        }
    }

    #[test]
    fn formats_negative_utc_offset() {
        assert_eq!(
            timestamp(-5 * 3600).to_string(),
            "2026-11-03 09:05:07.042 -0500"
        );
    }

    #[test]
    fn formats_positive_and_fractional_hour_offsets() {
        assert_eq!(
            timestamp(5 * 3600 + 30 * 60).to_string(),
            "2026-11-03 09:05:07.042 +0530"
        );
        assert_eq!(
            timestamp(-(9 * 3600 + 30 * 60)).to_string(),
            "2026-11-03 09:05:07.042 -0930"
        );
    }

    #[test]
    fn formats_utc_as_plus_zero() {
        assert_eq!(timestamp(0).to_string(), "2026-11-03 09:05:07.042 +0000");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn local_time_matches_the_instant_in_any_time_zone() {
        use std::time::Duration;

        const INSTANT_SECONDS: i64 = 1_700_000_000; // 2023-11-14 22:13:20 UTC
        let timestamp =
            Timestamp::local(Duration::new(INSTANT_SECONDS.unsigned_abs(), 123_000_000))
                .expect("localtime_r should succeed");

        assert_eq!(timestamp.millisecond, 123);
        assert!(timestamp.utc_offset_seconds.abs() <= 18 * 3600);

        let local_seconds_of_day = i64::from(timestamp.hour) * 3600
            + i64::from(timestamp.minute) * 60
            + i64::from(timestamp.second);
        assert_eq!(
            (local_seconds_of_day - timestamp.utc_offset_seconds).rem_euclid(86_400),
            INSTANT_SECONDS.rem_euclid(86_400)
        );
    }
}
