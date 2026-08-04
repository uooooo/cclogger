//! Just enough RFC 3339 arithmetic to measure an interval between two source
//! timestamps.
//!
//! Adapters are pure -- no clock, no I/O -- but *parsing* a timestamp the source
//! record already carries is neither. This is what lets the historical adapter emit a
//! real `duration_ms` for a tool call: the importer hands it the `tool_use` record's
//! timestamp through the [`crate::Keystore`], and the tool-result arm subtracts.
//!
//! Deliberately not shared with `cclogger_archive::occurred_at`, which does the adjacent
//! job of normalizing a timestamp to UTC `Z` for a promoted SQL column: `cclogger-adapters`
//! does not (and should not) depend on `cclogger-archive`, and pulling in a date crate for
//! ~60 lines of civil-calendar arithmetic would be disproportionate. This is the same
//! trade `bucket` and `tool_family` already make between the two adapter modules.

/// Milliseconds since the Unix epoch for an RFC 3339 `date-time`, or `None` if `raw`
/// is not a recognizable `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`.
///
/// Sub-second precision beyond milliseconds is truncated, not rounded: the value is
/// only ever used to measure an interval, and an implied precision the source did not
/// have would be a fabrication of exactly the kind this function exists to avoid.
///
/// Public for the same reason [`epoch_seconds`] is, and for one case that needs the
/// sub-second digits it drops: `cclogger-cli`'s importer decides which lines of a Codex
/// rollout file belong to a single buffered write by comparing their timestamps, and
/// a whole-second reading of "within a second" is a different question from the one
/// being asked. It stays pure -- text in, number out.
pub fn epoch_millis(raw: &str) -> Option<i64> {
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
        let (hms_and_fraction, signed_offset) = rest.split_at(sign_pos);
        let negative = signed_offset.starts_with('-');
        let mut offset_split = signed_offset[1..].splitn(2, ':');
        let offset_hours: i64 = offset_split.next()?.parse().ok()?;
        let offset_mins: i64 = match offset_split.next() {
            Some(s) => s.parse().ok()?,
            None => 0,
        };
        let magnitude = offset_hours * 60 + offset_mins;
        (
            hms_and_fraction,
            if negative { -magnitude } else { magnitude },
        )
    };

    let (hms, fraction) = match main_part.find('.') {
        Some(idx) => (&main_part[..idx], &main_part[idx + 1..]),
        None => (main_part, ""),
    };
    if !fraction.is_empty() && !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut millis_digits: String = fraction.chars().take(3).collect();
    while millis_digits.len() < 3 {
        millis_digits.push('0');
    }
    let fraction_millis: i64 = millis_digits.parse().ok()?;

    let mut hms_split = hms.splitn(3, ':');
    let hour: i64 = hms_split.next()?.parse().ok()?;
    let minute: i64 = hms_split.next()?.parse().ok()?;
    let second: i64 = hms_split.next()?.parse().ok()?;
    if hms_split.next().is_some()
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        // 60 is a leap second, which RFC 3339 permits.
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - offset_minutes * 60;
    Some(seconds * 1_000 + fraction_millis)
}

/// Whole seconds since the Unix epoch for an RFC 3339 `date-time`, or `None` if
/// `raw` is not one.
///
/// Sub-second precision is truncated towards negative infinity, so the value is the
/// second the instant falls *in* -- which is what a clock built on whole-second
/// intervals needs (`cclogger_domain::clock::Span`), and what keeps a timestamp before
/// the epoch from rounding the wrong way.
///
/// Public because `cclogger-cli`'s report has to place a ledger row on that clock, and a
/// fourth hand-rolled copy of this calendar arithmetic in this workspace would be one
/// too many. It stays pure -- text in, number out -- so exposing it does not weaken
/// the "adapters have no clock" rule this module's header states.
pub fn epoch_seconds(raw: &str) -> Option<i64> {
    Some(epoch_millis(raw)?.div_euclid(1_000))
}

/// Elapsed milliseconds from `start` to `end`.
///
/// `None` -- meaning "not measured" -- when either timestamp is unparseable or the
/// interval runs backwards. A backwards interval is a clock anomaly, not a duration:
/// reporting it as `0` (or as its absolute value) would put a fabricated measurement
/// in a field whose whole purpose is to say how long something took.
pub(crate) fn duration_ms(start: &str, end: &str) -> Option<i64> {
    let elapsed = epoch_millis(end)?.checked_sub(epoch_millis(start)?)?;
    (elapsed >= 0).then_some(elapsed)
}

/// Howard Hinnant's days-from-civil conversion. Also implemented in
/// `cclogger-archive`'s `occurred_at` module and `cclogger-cli`'s `clock` module -- see this
/// module's doc comment for why it is not shared.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m as i64;
    let doy = (153 * (if mp > 2 { mp - 3 } else { mp + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_itself_is_zero() {
        assert_eq!(epoch_millis("1970-01-01T00:00:00.000Z"), Some(0));
    }

    #[test]
    fn duration_is_measured_in_milliseconds_across_a_minute_boundary() {
        assert_eq!(
            duration_ms("2026-07-20T02:00:12.000Z", "2026-07-20T02:00:19.500Z"),
            Some(7_500)
        );
        assert_eq!(
            duration_ms("2026-07-20T02:59:59.750Z", "2026-07-20T03:00:00.250Z"),
            Some(500)
        );
    }

    #[test]
    fn duration_is_measured_across_a_day_and_a_month_boundary() {
        assert_eq!(
            duration_ms("2026-07-31T23:59:59.000Z", "2026-08-01T00:00:01.000Z"),
            Some(2_000)
        );
    }

    #[test]
    fn an_offset_timestamp_is_compared_as_the_instant_it_names_not_as_wall_clock_text() {
        // 2026-07-20T11:00:12+09:00 is 2026-07-20T02:00:12Z, so this is the same
        // seven seconds as the Z-suffixed case above -- not nine hours.
        assert_eq!(
            duration_ms("2026-07-20T11:00:12.000+09:00", "2026-07-20T02:00:19.000Z"),
            Some(7_000)
        );
    }

    #[test]
    fn sub_millisecond_precision_is_truncated_rather_than_rounded_up() {
        assert_eq!(
            duration_ms("2026-07-20T02:00:00.000000Z", "2026-07-20T02:00:00.001999Z"),
            Some(1)
        );
    }

    #[test]
    fn an_interval_that_runs_backwards_is_not_measured_rather_than_reported_as_zero() {
        assert_eq!(
            duration_ms("2026-07-20T02:00:19.000Z", "2026-07-20T02:00:12.000Z"),
            None
        );
    }

    #[test]
    fn epoch_seconds_is_the_second_the_instant_falls_in() {
        assert_eq!(epoch_seconds("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            epoch_seconds("1970-01-01T00:00:00.999Z"),
            Some(0),
            "a fraction is truncated to the second it belongs to, not rounded up"
        );
        assert_eq!(
            epoch_seconds("1969-12-31T23:59:59.500Z"),
            Some(-1),
            "and truncation runs towards negative infinity, so it stays in that second"
        );
        assert_eq!(epoch_seconds("not a timestamp"), None);
    }

    #[test]
    fn an_unparseable_endpoint_is_not_measured() {
        assert_eq!(
            duration_ms("not a timestamp", "2026-07-20T02:00:12.000Z"),
            None
        );
        assert_eq!(duration_ms("2026-07-20T02:00:12.000Z", ""), None);
        assert_eq!(
            duration_ms("2026-07-20T25:00:00.000Z", "2026-07-20T02:00:12.000Z"),
            None
        );
    }
}
