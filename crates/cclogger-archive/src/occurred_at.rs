//! Normalizes `Observation.time` for the `observation.occurred_at` promoted column.
//!
//! `time`'s schema constraint is `format: date-time` (RFC 3339), which permits any
//! numeric offset (`+09:00`, `-05:00`, ...) and any fractional-second precision.
//! `occurred_at` exists purely for lexicographic range queries (see the
//! `CREATE TABLE observation` schema comment in `crate::ledger`), so an adapter that
//! emits an offset instead of `Z` would silently break every range filter built on
//! that column: `"2026-07-29T09:00:00+09:00"` sorts *after*
//! `"2026-07-29T01:00:00Z"` even though the first instant is eight hours *earlier*.
//! [`normalize`] rewrites the value to UTC with a literal `Z` suffix before it is
//! bound to `occurred_at`; the raw, unnormalized value is still stored verbatim in
//! `body`, so the observation's round-trip property is unaffected.

/// Normalize an RFC 3339 `date-time` string to UTC, `Z`-suffixed.
///
/// Returns `raw` unchanged if it does not parse as a recognizable
/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)` shape. This only ever affects the
/// promoted `occurred_at` column, never the value stored in `body`, so a value this
/// cannot parse is not lost -- just not normalized, exactly as `occurred_at` was
/// before this normalization existed.
pub(crate) fn normalize(raw: &str) -> String {
    match parse(raw) {
        Some(parsed) => parsed.to_utc_z(),
        None => raw.to_string(),
    }
}

struct Parsed {
    year: i64,
    month: u32,
    day: u32,
    hour: i64,
    minute: i64,
    second: i64,
    /// Includes the leading `.`, e.g. `".000"`; empty when the source had no
    /// fractional seconds. Carried through unchanged -- normalizing the offset does
    /// not need to touch sub-second precision.
    fraction: String,
    /// Signed minutes to subtract from the local time to reach UTC (i.e. `+09:00`
    /// becomes `540`, `-05:00` becomes `-300`, `Z` becomes `0`).
    offset_minutes: i64,
}

impl Parsed {
    fn to_utc_z(&self) -> String {
        let mut total_minutes = self.hour * 60 + self.minute - self.offset_minutes;
        let mut day_shift = 0i64;
        while total_minutes < 0 {
            total_minutes += 24 * 60;
            day_shift -= 1;
        }
        while total_minutes >= 24 * 60 {
            total_minutes -= 24 * 60;
            day_shift += 1;
        }
        let hour = total_minutes / 60;
        let minute = total_minutes % 60;

        let days = days_from_civil(self.year, self.month, self.day) + day_shift;
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{:02}{}Z",
            self.second, self.fraction
        )
    }
}

fn parse(raw: &str) -> Option<Parsed> {
    let t_pos = raw.find(['T', 't'])?;
    let (date_part, rest) = (&raw[..t_pos], &raw[t_pos + 1..]);

    let mut date_split = date_part.splitn(3, '-');
    let year: i64 = date_split.next()?.parse().ok()?;
    let month: u32 = date_split.next()?.parse().ok()?;
    let day: u32 = date_split.next()?.parse().ok()?;
    if date_split.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let (main_part, offset_minutes) = if let Some(stripped) = rest.strip_suffix(['Z', 'z']) {
        (stripped, 0i64)
    } else {
        let sign_pos = rest.rfind(['+', '-'])?;
        let (hms_and_frac, signed_offset) = rest.split_at(sign_pos);
        let negative = signed_offset.starts_with('-');
        let offset_str = &signed_offset[1..];
        let mut offset_split = offset_str.splitn(2, ':');
        let off_h: i64 = offset_split.next()?.parse().ok()?;
        let off_m: i64 = match offset_split.next() {
            Some(s) => s.parse().ok()?,
            None => 0,
        };
        let magnitude = off_h * 60 + off_m;
        (hms_and_frac, if negative { -magnitude } else { magnitude })
    };

    let (hms, fraction) = match main_part.find('.') {
        Some(idx) => (&main_part[..idx], main_part[idx..].to_string()),
        None => (main_part, String::new()),
    };
    if fraction.len() > 1 && !fraction[1..].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let mut hms_split = hms.splitn(3, ':');
    let hour: i64 = hms_split.next()?.parse().ok()?;
    let minute: i64 = hms_split.next()?.parse().ok()?;
    let second: i64 = hms_split.next()?.parse().ok()?;
    if hms_split.next().is_some() {
        return None;
    }
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) {
        return None;
    }

    Some(Parsed {
        year,
        month,
        day,
        hour,
        minute,
        second,
        fraction,
        offset_minutes,
    })
}

/// Howard Hinnant's days-from-civil / civil-from-days conversions -- the same
/// well-known algorithm `cclogger-cli`'s `timestamp()` helper uses
/// (`crates/cclogger-cli/src/main.rs`), reimplemented here rather than shared because
/// this is the only other call site and pulling in a date dependency for it would be
/// disproportionate.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m as i64;
    let doy = (153 * (if mp > 2 { mp - 3 } else { mp + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
    fn days_from_civil_and_civil_from_days_round_trip_known_dates() {
        for (y, m, d, days) in [(1970, 1, 1, 0), (2024, 1, 1, 19_723), (2026, 7, 29, 20_663)] {
            assert_eq!(days_from_civil(y, m, d), days, "{y:04}-{m:02}-{d:02}");
            assert_eq!(civil_from_days(days), (y, m, d), "day {days}");
        }
    }

    #[test]
    fn a_z_suffixed_timestamp_normalizes_to_itself() {
        assert_eq!(
            normalize("2026-07-29T00:00:00.000Z"),
            "2026-07-29T00:00:00.000Z"
        );
    }

    #[test]
    fn a_positive_offset_normalizes_to_the_equivalent_utc_instant() {
        // 2026-07-29T09:00:00+09:00 is 2026-07-29T00:00:00Z.
        assert_eq!(
            normalize("2026-07-29T09:00:00.000+09:00"),
            "2026-07-29T00:00:00.000Z"
        );
    }

    #[test]
    fn a_negative_offset_normalizes_and_can_roll_the_date_backward() {
        // 2026-07-29T00:30:00-05:00 is 2026-07-29T05:30:00Z (no rollover here);
        // pick one that actually crosses midnight to pin the day-shift arithmetic.
        assert_eq!(
            normalize("2026-07-29T02:00:00-05:00"),
            "2026-07-29T07:00:00Z"
        );
        assert_eq!(
            normalize("2026-07-29T01:00:00+09:00"),
            "2026-07-28T16:00:00Z",
            "a positive offset larger than the local hour must roll the date back a day"
        );
    }

    #[test]
    fn an_unparseable_value_is_returned_unchanged() {
        assert_eq!(normalize("not a timestamp"), "not a timestamp");
    }
}
