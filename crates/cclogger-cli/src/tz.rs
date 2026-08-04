//! The UTC offset a day is cut at: what it is on this machine, and how a person names
//! a different one.
//!
//! A fixed offset, not a zone -- `report` and `log` bucket observations into local
//! calendar days, and that needs one number, not a rule. What this module adds is where
//! the number comes from when nobody supplies one: the machine the command is running
//! on, asked freshly on every run, rather than a constant compiled in.

use std::fmt;
use std::process::Command;

/// A fixed UTC offset, held in whole minutes.
///
/// Minutes rather than hours because several zones are not a whole number of hours off
/// UTC -- India is `+05:30`, Nepal `+05:45`, parts of Australia `+08:45` and `+09:30`,
/// Newfoundland `-03:30`. An hours-only offset cannot express any of them, so for
/// everyone living in one it cannot cut a day in the right place either, and the number
/// it cuts at instead looks entirely plausible in the header.
///
/// Deliberately not range-checked at construction. Which offsets a real zone uses is a
/// question `run_report`/`run_log` already answer, at the point they are about to slice a
/// day with one ([`TzOffset::is_real_zone`]) -- keeping it there means an out-of-range
/// value is refused with the same message whether it came from the flag or from anywhere
/// else, rather than by whichever layer happened to see it first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TzOffset {
    minutes: i32,
}

impl TzOffset {
    /// The westernmost offset any zone keeps: Baker Island.
    const WESTERNMOST: TzOffset = TzOffset::from_hours(-12);
    /// And the easternmost: Kiritimati.
    const EASTERNMOST: TzOffset = TzOffset::from_hours(14);

    pub const fn from_minutes(minutes: i32) -> Self {
        TzOffset { minutes }
    }

    pub const fn from_hours(hours: i32) -> Self {
        TzOffset {
            minutes: hours * 60,
        }
    }

    /// How far the local clock is ahead of UTC, in seconds -- the form every day
    /// boundary in `report` and `log` is computed with.
    pub const fn seconds(self) -> i64 {
        self.minutes as i64 * 60
    }

    /// Whether any zone on earth uses this offset. Anything outside that range would
    /// still slice a 24-hour window, just one no clock keeps.
    pub const fn is_real_zone(self) -> bool {
        self.minutes >= Self::WESTERNMOST.minutes && self.minutes <= Self::EASTERNMOST.minutes
    }
}

/// `+09:00`, `-03:30` -- the form the header prints, and the one `--tz-offset-hours`
/// accepts back. A zero offset is `+00:00`: UTC is not west of itself, and `-00:00` in a
/// header would read as a bug.
impl fmt::Display for TzOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.minutes < 0 { '-' } else { '+' };
        // `unsigned_abs`, not `abs`: `i32::MIN` has no positive counterpart, and a header
        // is not the place to discover that.
        let magnitude = self.minutes.unsigned_abs();
        write!(f, "{sign}{:02}:{:02}", magnitude / 60, magnitude % 60)
    }
}

/// This machine's current UTC offset, or why it could not be read.
///
/// ## Why this shells out
///
/// Answering "what offset is this machine in *right now*" means resolving a zone name to
/// an offset at an instant: `America/New_York` is `-05:00` in January and `-04:00` in
/// July, `Asia/Kolkata` is half an hour off the hour, and `TZ` may name any of them. Only
/// the platform's own time zone database knows that, and Rust's standard library exposes
/// no interface to it. This workspace has no date library on purpose -- it formats and
/// parses RFC 3339 by hand -- and adding one to read a single number would be a poor
/// trade.
///
/// That leaves two ways to reach the database already installed on the machine. The first
/// is to declare libc's `localtime_r` and its `struct tm` by hand and read `tm_gmtoff`:
/// fast, and no dependency, but it is an ABI assumption, and an ABI assumption that turns
/// out to be wrong yields a number rather than an error -- a plausible-looking offset,
/// silently wrong, which is exactly the failure this whole change exists to remove. The
/// second is to ask the POSIX utility that already reads that database. It costs a process
/// spawn per invocation, on a command that opens a SQLite ledger anyway, and every way it
/// can fail (no `date` on `PATH`, a non-zero exit, output that is not an offset) is
/// visible as a failure rather than as a number. That is the trade taken here: a loud
/// failure beats a quiet wrong answer, which is the same reason nothing downstream falls
/// back to a default offset when this returns `Err`.
///
/// `date +%z` prints `+HHMM`, honours `TZ`, and reports the offset in force now -- so a
/// DST-observing zone gives the offset of the day the command is run on, not of January.
pub fn detect() -> Result<TzOffset, DetectError> {
    let output = Command::new("date")
        .arg("+%z")
        // `%z` is numeric, but a locale is one more thing that could reshape the output
        // of a program being read by another program.
        .env("LC_ALL", "C")
        .output()
        .map_err(DetectError::NotRun)?;
    if !output.status.success() {
        return Err(DetectError::Failed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_utc_offset_field(&printed).ok_or(DetectError::Unreadable(printed))
}

/// Why this machine's UTC offset could not be read. Each variant names the step that
/// failed, because the three call for different things: a missing `date`, a `date` that
/// refused, and a `date` that answered with something else.
#[derive(Debug)]
pub enum DetectError {
    /// `date` could not be run at all -- not on `PATH`, or the process could not be
    /// spawned.
    NotRun(std::io::Error),
    /// It ran and exited non-zero.
    Failed { status: String, stderr: String },
    /// It succeeded and printed something that is not a UTC offset -- a `date` too old to
    /// know `%z` echoes the format string back, for instance.
    Unreadable(String),
}

impl fmt::Display for DetectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetectError::NotRun(e) => write!(f, "`date +%z` could not be run: {e}"),
            DetectError::Failed { status, stderr } if stderr.is_empty() => {
                write!(f, "`date +%z` {status}")
            }
            DetectError::Failed { status, stderr } => {
                write!(f, "`date +%z` {status}: {stderr}")
            }
            DetectError::Unreadable(printed) => write!(
                f,
                "`date +%z` printed {printed:?}, which is not a UTC offset"
            ),
        }
    }
}

impl std::error::Error for DetectError {}

/// `--tz-offset-hours`, as clap parses it.
///
/// Whole hours are what the flag has always taken and still mean what they meant: `9`,
/// `+9`, `-5`. `±HH:MM` is added for the zones whole hours cannot name at all, and is
/// spelled the way the header prints them, so a value can be read off one run and given
/// to the next.
///
/// Strict about shape, in the same way [`crate::report::Day::parse`] is and for the same
/// reason: `str::parse` accepts a leading `+` and surrounding shapes this must not, and a
/// value quietly read as a different offset would cut the day somewhere the person did
/// not point and head it with an offset that looked right.
pub fn parse_flag(raw: &str) -> Result<TzOffset, String> {
    parse_hours_or_hhmm(raw).ok_or_else(|| {
        format!(
            "{raw:?} is not a UTC offset -- expected whole hours (`9`, `-5`), or `±HH:MM` \
             for a zone that is not a whole hour (`5:30`, `-3:30`). \
             (`date +%z` prints `+0530`; write that one as `5:30`.)"
        )
    })
}

fn parse_hours_or_hhmm(raw: &str) -> Option<TzOffset> {
    let (negative, magnitude) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw.strip_prefix('+').unwrap_or(raw)),
    };
    match magnitude.split_once(':') {
        Some((hours, minutes)) => {
            if minutes.len() != 2 {
                return None;
            }
            compose(
                negative,
                two_digits_at_most(hours)?,
                minutes_field(minutes)?,
            )
        }
        None => compose(negative, two_digits_at_most(magnitude)?, 0),
    }
}

/// One `+HHMM` field as `date +%z` prints it, or the `+HH:MM` a `date` that prints the
/// separator would. The sign is required: `%z` always prints one, so its absence means
/// what came back was not an offset at all.
fn parse_utc_offset_field(printed: &str) -> Option<TzOffset> {
    let (negative, magnitude) = match printed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, printed.strip_prefix('+')?),
    };
    // Rejected before any indexing below, which is by byte: this is another program's
    // output, and a multi-byte character would put a split in the middle of one.
    if !magnitude.is_ascii() {
        return None;
    }
    let (hours, minutes) = match magnitude.len() {
        4 => magnitude.split_at(2),
        5 if magnitude.as_bytes()[2] == b':' => (&magnitude[..2], &magnitude[3..]),
        _ => return None,
    };
    compose(negative, digits(hours)?, minutes_field(minutes)?)
}

fn compose(negative: bool, hours: u32, minutes: u32) -> Option<TzOffset> {
    let magnitude = i32::try_from(hours)
        .ok()?
        .checked_mul(60)?
        .checked_add(i32::try_from(minutes).ok()?)?;
    Some(TzOffset::from_minutes(if negative {
        -magnitude
    } else {
        magnitude
    }))
}

fn minutes_field(text: &str) -> Option<u32> {
    let minutes = digits(text)?;
    (minutes <= 59).then_some(minutes)
}

fn two_digits_at_most(text: &str) -> Option<u32> {
    if !(1..=2).contains(&text.len()) {
        return None;
    }
    digits(text)
}

/// ASCII digits only. Rust's integer parsers accept a leading `+`, and `+9` would then
/// slip through a width check as `9` -- the same hole `Day::parse` closes.
fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offset_prints_the_way_the_flag_reads_it_back() {
        assert_eq!(TzOffset::from_hours(9).to_string(), "+09:00");
        assert_eq!(
            TzOffset::from_hours(0).to_string(),
            "+00:00",
            "never -00:00"
        );
        assert_eq!(TzOffset::from_hours(-7).to_string(), "-07:00");
        assert_eq!(TzOffset::from_minutes(5 * 60 + 30).to_string(), "+05:30");
        assert_eq!(TzOffset::from_minutes(5 * 60 + 45).to_string(), "+05:45");
        assert_eq!(TzOffset::from_minutes(-(3 * 60 + 30)).to_string(), "-03:30");

        // Every form printed here is one `--tz-offset-hours` accepts back, so an offset
        // read off one run can be given to the next.
        for offset in [
            TzOffset::from_hours(9),
            TzOffset::from_hours(0),
            TzOffset::from_hours(-7),
            TzOffset::from_minutes(5 * 60 + 30),
            TzOffset::from_minutes(-(3 * 60 + 30)),
        ] {
            let printed = offset.to_string();
            assert_eq!(
                parse_flag(&printed),
                Ok(offset),
                "the header prints {printed}, so the flag must take it"
            );
        }
    }

    #[test]
    fn the_flag_takes_whole_hours_and_the_zones_whole_hours_cannot_name() {
        for (raw, minutes) in [
            ("9", 9 * 60),
            ("+9", 9 * 60),
            ("09", 9 * 60),
            ("0", 0),
            ("-5", -5 * 60),
            ("14", 14 * 60),
            ("-12", -12 * 60),
            ("5:30", 5 * 60 + 30),
            ("+05:45", 5 * 60 + 45),
            ("8:45", 8 * 60 + 45),
            ("-3:30", -(3 * 60 + 30)),
            ("12:45", 12 * 60 + 45),
            ("-0:30", -30),
        ] {
            assert_eq!(
                parse_flag(raw),
                Ok(TzOffset::from_minutes(minutes)),
                "--tz-offset-hours {raw}"
            );
        }
    }

    #[test]
    fn a_value_that_is_not_an_offset_is_refused_rather_than_read_as_a_different_one() {
        for malformed in [
            "", "+", "-", ":", "9:", ":30", "9:3",   // a half-typed minute is not a minute
            "9:300", //
            "9:60",  // no clock has one
            "9.5",   // decimal hours are not one of the two forms
            "0530",  // `date +%z`'s own form: four digits are not 530 hours
            "+0530", //
            "930",   // and neither is this 9:30
            "nine", " 9", "9 ", "++9", "9+", "0x9",
        ] {
            let refused = parse_flag(malformed);
            assert!(
                refused.is_err(),
                "{malformed:?} is not an offset and must be refused, not read as \
                 {refused:?}"
            );
        }

        // The refusal has to say what the two forms are: the flag is named for hours and
        // now takes something else as well, so "invalid value" alone leaves the reader
        // exactly where they were.
        let message = parse_flag("0530").expect_err("four digits are not a form this takes");
        assert!(
            message.contains("`9`") && message.contains("`5:30`"),
            "the refusal must name both forms: {message:?}"
        );
        assert!(
            message.contains("+0530"),
            "and point `date +%z` output at the form that takes it: {message:?}"
        );
    }

    #[test]
    fn what_date_prints_is_read_as_the_offset_it_names() {
        for (printed, minutes) in [
            ("+0900", 9 * 60),
            ("+0000", 0),
            ("-0700", -7 * 60),
            ("+0530", 5 * 60 + 30),
            ("+0845", 8 * 60 + 45),
            ("-0330", -(3 * 60 + 30)),
            ("+1400", 14 * 60),
            ("-1200", -12 * 60),
            // A `date` that prints the separator is read the same way.
            ("+05:30", 5 * 60 + 30),
            ("-03:30", -(3 * 60 + 30)),
        ] {
            assert_eq!(
                parse_utc_offset_field(printed),
                Some(TzOffset::from_minutes(minutes)),
                "`date +%z` printed {printed}"
            );
        }
    }

    #[test]
    fn output_that_is_not_an_offset_is_not_read_as_one() {
        // The one that matters most: a `date` with no `%z` echoes the format string back.
        // Read as an offset it would be `+0` -- UTC, plausible, and wrong everywhere but
        // one meridian. It has to fail instead.
        assert_eq!(parse_utc_offset_field("+%z"), None);
        for other in [
            "",
            "+",
            "0900", // the sign is not optional: `%z` always prints one
            "+09000",
            "+090",
            "+09:0",
            "+ab00",
            "+0960", // no clock has :60
            "JST",
            "Tue Aug  4 09:00:00 JST 2026",
            // Four *bytes*, not four characters: a split at byte 2 would land inside the
            // second one, and this has to refuse rather than panic on another program's
            // output.
            "+aébc",
            "+é:30",
        ] {
            assert_eq!(
                parse_utc_offset_field(other),
                None,
                "{other:?} is not a UTC offset"
            );
        }
    }

    #[test]
    fn only_the_offsets_a_zone_actually_uses_are_treated_as_real() {
        assert!(TzOffset::from_hours(14).is_real_zone(), "Kiritimati");
        assert!(TzOffset::from_hours(-12).is_real_zone(), "Baker Island");
        assert!(TzOffset::from_minutes(5 * 60 + 45).is_real_zone(), "Nepal");
        assert!(!TzOffset::from_minutes(14 * 60 + 1).is_real_zone());
        assert!(!TzOffset::from_minutes(-(12 * 60 + 1)).is_real_zone());
        assert!(!TzOffset::from_hours(99).is_real_zone());
    }

    #[test]
    fn an_offset_carries_the_seconds_a_day_boundary_is_computed_from() {
        assert_eq!(TzOffset::from_hours(9).seconds(), 9 * 3600);
        assert_eq!(TzOffset::from_minutes(5 * 60 + 30).seconds(), 19_800);
        assert_eq!(TzOffset::from_hours(-7).seconds(), -7 * 3600);
    }

    #[test]
    fn this_machine_has_an_offset_and_it_is_one_a_zone_uses() {
        // What is deliberately *not* asserted here is which offset that is: the machine
        // running the suite is not part of the contract, and an assertion naming its
        // offset would pass identically against a hardcoded constant -- the defect this
        // module replaced. What is asserted is that the detection path works end to end
        // on the machine at hand: `date` is reachable, it understands `%z`, and what it
        // printed parsed. A build where any of that is untrue must fail here rather than
        // in front of a user.
        //
        // That the answer *tracks the environment* rather than being a constant is the
        // other half, and it is tested in `tests/tz_offset_default.rs`, which sets `TZ`
        // on child processes. It cannot be tested here: `detect` reads this process's
        // environment, and mutating that mid-suite is a data race against every other
        // test thread reading it.
        let detected = detect().unwrap_or_else(|e| panic!("this machine has a UTC offset: {e}"));
        assert!(
            detected.is_real_zone(),
            "the detected offset {detected} is not one any zone uses -- something other \
             than an offset was read"
        );
    }
}
