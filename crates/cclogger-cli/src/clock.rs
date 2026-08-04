//! Small dependency-free wall-clock helpers shared by `archive`, `migrate`, and
//! `import`. Kept here rather than duplicated per-command (as `archive`'s original
//! `timestamp()`/`civil_from_days()` were) now that a second command needs the same
//! "one RFC3339 UTC-seconds timestamp per invocation" shape.

/// RFC3339 UTC seconds, without pulling in a date library for these few call sites.
pub(crate) fn now_utc_seconds() -> String {
    format_utc_seconds(now_epoch_seconds())
}

/// Seconds since the unix epoch, now.
pub(crate) fn now_epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
        .as_secs() as i64
}

/// RFC3339 UTC with millisecond precision, for the hook receiver's arrival stamp.
///
/// Seconds are not enough here and the extra digits are not decoration: a turn can end
/// and the next tool start inside the same second, and `duration_ms` is measured in
/// milliseconds, so a second-resolution arrival stamp would collapse events that are
/// genuinely ordered and would round every short interval to nothing. It would also
/// collapse two events' dedupe keys into one, since the key is derived partly from the
/// time -- which is a silent undercount, the failure this project treats as its worst.
///
/// Unlike [`now_utc_seconds`], this never panics: it is called on the hook hot path,
/// where a panic would exit non-zero and Claude Code would report a hook error (on some
/// events, block). A clock before the epoch yields the epoch instead.
pub(crate) fn now_utc_millis() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    format_utc_millis(millis)
}

/// One epoch millisecond as RFC3339 UTC, millisecond precision.
pub(crate) fn format_utc_millis(millis: i64) -> String {
    let seconds = format_utc_seconds(millis.div_euclid(1_000));
    // `format_utc_seconds` always ends in `Z`; the fraction goes before it.
    format!(
        "{}.{:03}Z",
        seconds.trim_end_matches('Z'),
        millis.rem_euclid(1_000)
    )
}

/// One epoch second as RFC3339 UTC, second precision.
pub(crate) fn format_utc_seconds(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's days-from-civil. The inverse of [`civil_from_days`], needed to
/// turn a calendar day the user asked for (`--day 2026-07-26`) back into the epoch
/// range it names.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m as i64;
    let doy = (153 * (if mp > 2 { mp - 3 } else { mp + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant's days-from-civil inverse.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_663), (2026, 7, 29));
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days_on_known_dates() {
        for (y, m, d, days) in [(1970, 1, 1, 0), (2024, 1, 1, 19_723), (2026, 7, 29, 20_663)] {
            assert_eq!(days_from_civil(y, m, d), days, "{y:04}-{m:02}-{d:02}");
            assert_eq!(civil_from_days(days), (y, m, d), "day {days}");
        }
    }

    #[test]
    fn format_utc_millis_keeps_the_fraction_a_hook_arrival_stamp_needs() {
        assert_eq!(format_utc_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc_millis(1), "1970-01-01T00:00:00.001Z");
        assert_eq!(format_utc_millis(999), "1970-01-01T00:00:00.999Z");
        assert_eq!(format_utc_millis(1_000), "1970-01-01T00:00:01.000Z");
        // Two events in the same second stay distinguishable, which is what keeps
        // their dedupe keys apart.
        assert_ne!(format_utc_millis(1_500), format_utc_millis(1_501));
        assert_eq!(
            format_utc_millis(days_from_civil(2026, 7, 25) * 86_400_000 + 15 * 3_600_000 + 42),
            "2026-07-25T15:00:00.042Z"
        );
    }

    #[test]
    fn format_utc_millis_round_trips_through_the_parser_the_adapters_use() {
        // The arrival stamp becomes an observation's `time`, which `report` reads back
        // through `rfc3339::epoch_millis`. A format the two disagreed on would put
        // every hook observation on no clock at all.
        for millis in [0i64, 1, 999, 1_000, 1_754_270_000_123] {
            let rendered = format_utc_millis(millis);
            assert_eq!(
                cclogger_adapters::rfc3339::epoch_millis(&rendered),
                Some(millis),
                "{rendered}"
            );
        }
    }

    #[test]
    fn format_utc_seconds_renders_the_instant_the_epoch_second_names() {
        assert_eq!(format_utc_seconds(0), "1970-01-01T00:00:00Z");
        // 2026-07-25T15:00:00Z -- the UTC start of 2026-07-26 in a +09:00 offset.
        assert_eq!(
            format_utc_seconds(days_from_civil(2026, 7, 25) * 86_400 + 15 * 3600),
            "2026-07-25T15:00:00Z"
        );
    }
}
