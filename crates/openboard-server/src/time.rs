//! RFC 3339 timestamps without a date-time dependency.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time as an RFC 3339 UTC timestamp with millisecond precision.
pub(crate) fn now_rfc3339() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(since_epoch.as_secs() as i64, since_epoch.subsec_millis())
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

/// Formats milliseconds since the Unix epoch like [`now_rfc3339`].
pub(crate) fn rfc3339_from_ms(ms: i64) -> String {
    format_rfc3339(ms.div_euclid(1000), ms.rem_euclid(1000) as u32)
}

/// Formats seconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub(crate) fn format_rfc3339(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days, after Howard Hinnant's public domain algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::format_rfc3339;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339(951_782_400, 7), "2000-02-29T00:00:00.007Z");
        assert_eq!(
            format_rfc3339(1_794_960_000 + 86_399, 999),
            "2026-11-18T23:59:59.999Z"
        );
    }
}
