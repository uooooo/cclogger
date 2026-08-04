//! `cclogger report`: what a period's ledger says about what was worked on, and for
//! how long -- the question this project exists to answer, derived and never stored.
//! A day is a period of one; a week, a month and an arbitrary range go through the
//! same code, which is what stops the two from drifting apart.
//!
//! Four decisions shape everything here. The first three were settled against the real
//! ledger before this module existed; the measurements are in
//! `docs/superpowers/specs/expected-output-2026-07-26.md` and the plan that cites it.
//!
//! **1. Overlapping shares sum to the union, never past it.** Three repositories can
//! be active inside the same hour, and each one's `[-1m, +5m]` prompt windows unioned
//! *independently* add up to more time than the day actually held. The union is the
//! day's real attention; [`cclogger_domain::clock::partition_by_nearest_anchor`] then
//! splits exactly that union between repositories, so the shares always add back to
//! it (design §13). The per-repository naive sum is reported too, labelled as such:
//! the acceptance document is emphatic that these two numbers differ, and showing only
//! one hides the difference rather than resolving it.
//!
//! **2. The agent clock clusters the global cross-session timeline.** Measured on the
//! real ledger: 82.5% of within-session gaps of 5 minutes or more have *another*
//! session active inside them -- the person switched work, they did not stop. So every
//! observation that does not anchor a human's attention window -- every non-prompt one,
//! and a prompt whose own `data.origin` says it is not the human's own -- goes onto one
//! timeline before clustering, from every repository. Per-session clustering moves 13x
//! between a 5-minute and a 60-minute threshold; the global basis moves 1.58x.
//! `duration_ms` is not summed either: the
//! longest historical tool "duration" in the corpus is 42.8 hours, which is a person
//! walking away from an approval prompt, and a sum has no way to notice.
//!
//! **3. A repository with activity but no prompts is listed with 0 attention, not
//! omitted.** Attention 0 ("there was no prompt to anchor a window") and coverage 0
//! ("nothing was observed") are different statements, and dropping the row conflates
//! them. The same rule governs a repository no `[[rule]]` matches: it appears under
//! `unassigned`, never folded into a neighbouring bucket, and `unassigned` in turn is
//! never confused with a record that carried no repository identity at all.
//!
//! **4. A period is computed over its own observations, never assembled from its
//! days.** The naive-sum trap of decision 1, one level up. Attention windows are
//! anchored to prompts and never clipped at a boundary, so a window opened before
//! local midnight belongs whole to that day *and* overlaps whatever the next day's
//! windows cover -- adding seven daily unions therefore counts those minutes twice.
//! Clusters fail the other way: a stretch of agent work running through midnight is
//! one cluster over the period and two over the two days, which drops the silence
//! between them. Both readings are computed and both are printed
//! ([`PeriodReport::attention_daily_sum_seconds`],
//! [`PeriodReport::agent_daily_sum_seconds`]), because a number a person can arrive at
//! two ways has to show both. The same rule governs the two per-day averages: a period
//! divided by its calendar days and by its *active* days are different answers, so
//! both are printed with their denominator named rather than one being chosen quietly.
//!
//! **5. A completion is spent when a prompt answers it, and reaction time is reported
//! as a shape rather than a total.** The gap from an assistant's completed output to
//! the human prompt that followed it is the person's own reaction, with the model's
//! generation time excluded -- so each completion may answer *one* prompt, and is
//! consumed doing it. Reusing a spent completion measures the second of two consecutive
//! prompts from output that already belongs to the first: not a reaction time at all,
//! and longer than any reaction was. The prompts that find nothing unconsumed are
//! counted separately instead, because on a month of real data roughly a third of
//! prompts are back-to-back, and that is a fact about how a person works rather than a
//! nuisance to hide. What is reported is a count and quartiles -- never a sum, which is
//! decision 1's trap one level down (sessions run concurrently, so their gaps added up
//! exceed the clock they happened on), and never a mean, because the tail runs into the
//! tens of minutes and a mean of it describes no reaction anyone had. The plain gap
//! between consecutive prompts is reported beside it for reference; the difference
//! between the two is roughly what the model spent generating.
//!
//! Nothing here writes: the ledger is read-only input. A `ledger.db` that would have
//! to be created or schema-upgraded to be read is refused rather than written to, the
//! same discipline `cclog import --dry-run` follows.

use crate::tz::TzOffset;
use cclogger_adapters::rfc3339;
use cclogger_archive::{Ledger, LedgerError, ObservationRow, SourceRange};
use cclogger_domain::clock::{Span, cluster, partition_by_nearest_anchor, union_seconds};
use cclogger_domain::workstream::{RuleError, Rules};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// How far before a human prompt its attention window reaches (acceptance §1).
pub const ATTENTION_BEFORE_SECONDS: i64 = 60;
/// How far after it. Together these are an algorithm parameter, not a fact -- which
/// is why [`render`] prints them next to the numbers they produced.
pub const ATTENTION_AFTER_SECONDS: i64 = 300;
/// The largest silence still counted as one continuous stretch of agent activity.
pub const AGENT_GAP_SECONDS: i64 = 300;
/// The fewest measured gaps this report will state quartiles over.
///
/// Three in each of the four quarters. Below that the quartiles are naming individual
/// gaps rather than describing a shape, and a median printed from three points reads on
/// the page exactly like one drawn from three hundred. The count and the back-to-back
/// figure are still reported under this floor -- those are counts of things that
/// happened, and a count of three is a true count of three.
pub const MIN_GAPS_FOR_QUARTILES: usize = 12;

/// The event this report treats as a human prompt: the anchor of every attention
/// window, and the only event kept off the agent clock.
pub(crate) const PROMPT_EVENT_TYPE: &str = "dev.cclog.prompt.submitted.v1";
/// The event this report treats as an assistant's completed output: the instant a
/// reaction time starts running from. An assistant turn that called tools emits
/// several of these, which is why a later one supersedes an earlier unconsumed one --
/// what the person reacted to is the last thing they were shown.
pub(crate) const RESPONSE_EVENT_TYPE: &str = "dev.cclog.response.completed.v1";
/// A commit of the person's own, observed by the git adapter.
///
/// Named here, beside the event types the clocks are built on, because what this
/// constant is *for* is keeping commits out of them. A commit is an instant: it says
/// work reached a durable artefact, not that any span of time was spent. Feeding one to
/// the agent clock would extend a cluster to the moment it landed, and mark the day it
/// landed on as worked -- which is how a `git commit` typed at midnight becomes five
/// minutes of measured work on a day nothing else happened.
pub(crate) const COMMIT_EVENT_TYPE: &str = "dev.cclog.commit.observed.v1";
/// A line the importer could not turn into an observation. Dated to when
/// `cclogger archive` ran when the line was unparseable, so it is evidence about
/// collection, never about activity: kept out of every clock, and said so in the
/// output rather than left to be inferred.
pub(crate) const GAP_EVENT_TYPE: &str = "dev.cclog.source.gap.v1";
/// What a repository with no matching rule is shown as. Distinct from a record with
/// no repository identity at all -- see [`PeriodReport::unattributed`].
pub(crate) const UNASSIGNED: &str = "unassigned";

/// The one `data.time_basis` value that says an observation's `time` is when the
/// thing it records actually happened. See [`time_is_when_it_happened`].
pub(crate) const TIME_BASIS_OCCURRED_AT: &str = "occurred_at";

/// The `data.time_basis` of a hook observation: when cclog's own receiver was invoked.
/// Admitted onto clocks -- see [`time_is_when_it_happened`] for why, and for why the
/// other two qualified bases are not.
pub(crate) const TIME_BASIS_RECEIVED_AT: &str = "received_at";

/// The snapshot `source_kind` the hook spool is stored under -- the capture channel
/// [`Coverage::hook_capture`] reports the start of. Spelled out here rather than taken
/// from `crate::hook`, so this stays a statement about what is in the ledger: a rename
/// on the writing side must show up as coverage that stopped being reported, not be
/// silently followed here.
pub(crate) const HOOK_CHANNEL: &str = "claude-code-hook";

/// Whether a row's `occurred_at` may be put on a clock at all.
///
/// Some observations carry a `time` that is not when their activity happened, and say
/// so on the row: a gap marker dated to when `cclogger archive` ran (`acquired_at`),
/// and a record a Codex fork copied out of its parent's transcript, which carries the
/// copy's write time (`copied_at`). Neither is evidence that anything happened at that
/// instant, so neither may open an attention window, extend an agent cluster, mark a
/// day active, or light a cell on the strip.
///
/// `received_at` **is** admitted, and the difference is not a softening of the rule but
/// the rule applied. The two excluded bases are a *different event's* time standing in
/// for one that was never measured: an archive run days later, or the moment a fork
/// copied someone else's history. `received_at` is a measurement of this event's own
/// instant -- Claude Code invokes the hook synchronously and blocks on it, so the
/// receiver's clock reading differs from the event by one process spawn. That error is
/// milliseconds and bounded, and the alternative is not a better number but no number:
/// no hook event carries a timestamp, so a channel excluded here would contribute
/// nothing to any clock at all.
///
/// **Absence admits the row, and that is not a default standing in for a measurement.**
/// A producer that took the source record's own timestamp has nothing to qualify and
/// writes no `time_basis`; the producers whose `time` means something else state which
/// else. The schema says the same of the field. A future producer that puts a fourth
/// kind of clock on `time` has to state it here too -- which is the point of reading the
/// stated value rather than inferring one from the event type.
///
/// Shared by `report.rs` and `log.rs` so the two day views cannot come to disagree
/// about which observations a clock may read.
pub(crate) fn time_is_when_it_happened(row: &ObservationRow) -> bool {
    match row.time_basis.as_deref() {
        None => true,
        Some(basis) => basis == TIME_BASIS_OCCURRED_AT || basis == TIME_BASIS_RECEIVED_AT,
    }
}

/// Whether a prompt's own `data.origin` says it must not anchor an attention window: a
/// subagent's own prompt to itself, dispatched by a parent that will never be the human
/// reading a report, or -- once cclog acquires a scheduler -- a run nobody typed.
///
/// Denied, never dropped. A row this returns `true` for still reaches every other
/// clock; see [`run_report`]'s prompt branch, which routes it to the same `else` arm
/// every non-anchoring observation already takes, and [`crate::log::run_log`], which
/// does the same for the strip.
///
/// **An explicit deny-list, never an equality test against `"human"`.** `Ledger::ingest`
/// is `INSERT ... ON CONFLICT(cclogdedupekey) DO NOTHING`, and there is no
/// `UPDATE observation` anywhere in this codebase -- so every observation imported
/// before this field existed keeps a `data` object with no `origin` key forever;
/// re-archiving and re-importing the same file is a no-op for a dedupe key the ledger
/// already holds. On a real ledger those rows are the overwhelming majority, and every
/// Claude Code prompt is among them regardless of age, because only the Codex adapter
/// writes `origin` at all. A test of `origin != Some("human")` would deny an anchor to
/// nearly every prompt an existing ledger holds, drive reported attention toward zero,
/// and look like a working feature while doing it.
///
/// Shared by `report.rs` and `log.rs` so the two day views cannot come to disagree
/// about which prompts are the human's own.
pub(crate) fn prompt_origin_denies_anchor(row: &ObservationRow) -> bool {
    matches!(row.origin.as_deref(), Some("subagent") | Some("scheduled"))
}

/// Everything that can stop a report from being produced. Each is a refusal to
/// guess: an unreadable or malformed config, or a ledger that would have to be
/// written to before it could be read.
#[derive(Debug)]
pub enum ReportError {
    Ledger(LedgerError),
    /// `--day`, `--from` or `--to` was not a `YYYY-MM-DD` calendar date.
    InvalidDay(String),
    /// `--month` was not a `YYYY-MM` calendar month. Held to the same rule as
    /// `--day`: a month no calendar has is refused, not rounded into one that does.
    InvalidMonth(String),
    /// `--from` named a day after `--to`. Both ends are inclusive, so equal ends are
    /// a period of one day and allowed; only a backwards pair names nothing.
    InvertedPeriod {
        from: String,
        to: String,
    },
    /// One end of `--from`/`--to` was given without the other. The missing end is not
    /// completed with today, or with the ledger's first or last observation: each
    /// would be a different period, and picking one silently is the guess this
    /// command exists to avoid.
    IncompleteRange {
        given: &'static str,
        missing: &'static str,
    },
    /// `--from` or `--to` was not an `HH:MM` time a clock shows. Held to the same
    /// rule as `--day`: a time that never happened is refused, not saturated to the
    /// nearest one that did.
    InvalidTimeOfDay(String),
    /// `--from` was at or after `--to`. Half-open, so equal bounds hold nothing
    /// either -- and an empty view of them would read as "nothing happened then"
    /// rather than "that names no stretch of the day".
    InvertedRange {
        from: String,
        to: String,
    },
    /// `--from`/`--to` named a calendar date on a command whose day is already
    /// fixed to one -- `log`, always, or `report` once `--day` has named one. A
    /// date sets a day *boundary*, and a boundary only means something across a run
    /// of days: refused plainly, as a date that is out of scope, rather than
    /// reported as malformed the way an unparsable value is.
    DateOnSingleDay {
        flag: &'static str,
        date: String,
        scope: DayScope,
    },
    /// `--from` and `--to` named different kinds of bound -- one a calendar date,
    /// the other a time of day. Two dates name a run of days; a time (with a day in
    /// scope) narrows one day. A date and a time paired up name neither, so this is
    /// refused rather than guessing which end the person meant to anchor a day on.
    MixedBoundKinds {
        date_flag: &'static str,
        date: String,
        time_flag: &'static str,
        time: String,
    },
    /// The reporting offset is not one any timezone uses. Refused rather than used to
    /// cut a day out of the ledger at an hour nowhere observes.
    InvalidOffset(TzOffset),
    /// There is no ledger yet. An error rather than an empty report, because an empty
    /// report for a day with no ledger is indistinguishable from a day with no work.
    NoLedger(PathBuf),
    /// The ledger predates this build's schema. Opening it would upgrade and re-stamp
    /// it, which is a write, and a read-only command must not perform one.
    LedgerNeedsUpgrade(PathBuf),
    /// `workstreams.toml` exists but does not parse. Refused rather than treated as
    /// "no rules": a typo'd rule that silently assigns nothing is worse than a refusal.
    Config {
        path: PathBuf,
        source: RuleError,
    },
    /// `workstreams.toml` exists but could not be read (permissions, an I/O error).
    /// Deliberately not folded into "absent": absent means there is no config to
    /// apply, this means there is one and it was not applied.
    ConfigUnreadable {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Which command refused a calendar date on `--from`/`--to` because its day was
/// already fixed to one -- named in [`ReportError::DateOnSingleDay`] so the message
/// can point at the way out that actually applies to it: `log` never has one, and
/// `report` only needs `--day` dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayScope {
    /// `cclogger log`: always scoped to one day: there is no flag to drop.
    Log,
    /// `cclogger report --day`: scoped to one day because `--day` named it.
    ReportDay,
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReportError::Ledger(e) => write!(f, "{e}"),
            ReportError::InvalidDay(raw) => {
                write!(f, "{raw:?} is not a calendar date -- expected YYYY-MM-DD")
            }
            ReportError::InvalidMonth(raw) => {
                write!(f, "{raw:?} is not a calendar month -- expected YYYY-MM")
            }
            ReportError::InvertedPeriod { from, to } => write!(
                f,
                "--from {from} is after --to {to} -- that names no run of days. \
                 (Both ends are inclusive, so --from {to} --to {from} is what was meant.)"
            ),
            ReportError::IncompleteRange { given, missing } => write!(
                f,
                "--{given} was given without --{missing} -- a range needs both ends. \
                 (Completing it with today, or with the ledger's own first or last day, \
                 would each name a different period.)"
            ),
            ReportError::InvalidTimeOfDay(raw) => write!(
                f,
                "{raw:?} is not a time of day -- expected HH:MM between 00:00 and 23:59. \
                 (To run to the end of the day, leave --to off.)"
            ),
            ReportError::InvertedRange { from, to } => write!(
                f,
                "--from {from} is not before --to {to} -- that names no stretch of the day. \
                 (An empty report for it would look exactly like a stretch with no work.)"
            ),
            ReportError::DateOnSingleDay { flag, date, scope } => match scope {
                DayScope::Log => write!(
                    f,
                    "--{flag} {date} is a calendar date, but `cclogger log` only ever draws one \
                     day -- a range of days is out of scope for it. (Narrow the day with an \
                     HH:MM time instead, or use `cclogger report --from`/`--to` for a range of \
                     days.)"
                ),
                DayScope::ReportDay => write!(
                    f,
                    "--{flag} {date} is a calendar date, but --day already names the one day \
                     this report covers -- narrow it with an HH:MM time instead. (Drop --day \
                     and give --from and --to both as dates for a range of days.)"
                ),
            },
            ReportError::MixedBoundKinds {
                date_flag,
                date,
                time_flag,
                time,
            } => write!(
                f,
                "--{date_flag} {date} is a calendar date and --{time_flag} {time} is a time of \
                 day -- together they name no window. (Two dates name a run of days; a time \
                 narrows one day, --day's or today's. A date and a time paired up name neither.)"
            ),
            ReportError::InvalidOffset(offset) => write!(
                f,
                "UTC{offset} is not an offset any timezone uses -- expected -12:00 to +14:00"
            ),
            ReportError::NoLedger(path) => write!(
                f,
                "no ledger at {} -- run `cclogger archive` and `cclogger import` first. \
                 (Reporting an empty day instead would look exactly like a day with no work.)",
                path.display()
            ),
            ReportError::LedgerNeedsUpgrade(path) => write!(
                f,
                "the ledger at {} needs a schema upgrade -- run `cclogger import` once first; \
                 a report will not write to it",
                path.display()
            ),
            ReportError::Config { path, source } => {
                write!(f, "{}: {source}", path.display())
            }
            ReportError::ConfigUnreadable { path, source } => write!(
                f,
                "{} exists but could not be read: {source} -- refusing to report as though \
                 there were no workstream rules",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ReportError {}

impl From<LedgerError> for ReportError {
    fn from(e: LedgerError) -> Self {
        ReportError::Ledger(e)
    }
}

/// A calendar date in the reporting offset -- what the user names on the command
/// line, before it becomes a range of instants.
///
/// Deliberately not a general date type, and deliberately not a new dependency: the
/// rest of this workspace formats and parses RFC 3339 by hand, and one day's
/// half-open UTC range is all this needs to produce.
///
/// Ordered by the calendar, which the field order already gives: a period refuses a
/// `--from` after its `--to` by comparing two of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Day {
    year: i64,
    month: u32,
    day: u32,
}

impl Day {
    /// Parse `YYYY-MM-DD`, strictly: fixed widths and digits only, so a half-typed
    /// `2026-7-6` or a signed `+026-07-26` is a refusal rather than a silently
    /// different day.
    ///
    /// The date must also be one the calendar has. `days_from_civil` extrapolates
    /// linearly and will happily turn February 30th into March 2nd, which would head
    /// a report with a date that never happened, over another day's instants -- so
    /// the parsed date is required to survive the round trip back through
    /// `civil_from_days` unchanged. That check is what makes February 29th depend on
    /// the year rather than on a hardcoded table of month lengths.
    pub fn parse(text: &str) -> Result<Self, ReportError> {
        let invalid = || ReportError::InvalidDay(text.to_string());
        let parts: Vec<&str> = text.split('-').collect();
        let [y, m, d] = parts[..] else {
            return Err(invalid());
        };
        if (y.len(), m.len(), d.len()) != (4, 2, 2) {
            return Err(invalid());
        }
        // Digits only: Rust's integer parsers accept a leading `+`, which slips a
        // sign past the width check (`+026` is four characters and parses as 26).
        if !text.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
            return Err(invalid());
        }
        let year: i64 = y.parse().map_err(|_| invalid())?;
        let month: u32 = m.parse().map_err(|_| invalid())?;
        let day: u32 = d.parse().map_err(|_| invalid())?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(invalid());
        }
        let round_trip =
            crate::clock::civil_from_days(crate::clock::days_from_civil(year, month, day));
        if round_trip != (year, month, day) {
            return Err(invalid());
        }
        Ok(Day { year, month, day })
    }

    /// The calendar date it is right now in `tz_offset`.
    pub fn today(tz_offset: TzOffset) -> Self {
        let local = crate::clock::now_epoch_seconds() + tz_offset.seconds();
        let (year, month, day) = crate::clock::civil_from_days(local.div_euclid(86_400));
        Day { year, month, day }
    }

    pub fn label(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// The half-open UTC range `[start, end)` this day covers, in epoch seconds.
    pub fn utc_window(&self, tz_offset: TzOffset) -> (i64, i64) {
        let midnight = self.epoch_day() * 86_400 - tz_offset.seconds();
        (midnight, midnight + 86_400)
    }

    /// Days since 1970-01-01. The unit a period counts its length in, and the bucket
    /// an observation's local date resolves to -- one integer, so counting days needs
    /// no month arithmetic and cannot disagree with [`Self::utc_window`].
    pub fn epoch_day(&self) -> i64 {
        crate::clock::days_from_civil(self.year, self.month, self.day)
    }

    /// The inverse of [`Self::epoch_day`]. Private: every `Day` a user can name still
    /// comes through [`Self::parse`], and this only ever moves off one that did.
    fn from_epoch_day(epoch_day: i64) -> Self {
        let (year, month, day) = crate::clock::civil_from_days(epoch_day);
        Day { year, month, day }
    }
}

/// Which flag named a period. Only the header reads it, and only so that it can say
/// what the period *is*: `--week` is a rolling seven days rather than a Monday-to-
/// Sunday calendar week, and a header that printed "7 days" alone would leave that to
/// be guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodKind {
    Day,
    Week,
    Month,
    Range,
}

/// A run of whole calendar days in the reporting offset, both ends inclusive.
///
/// One day is a period of one, not a separate path: every total this module produces
/// is computed over the period's own observations, so a day and a month go through
/// the same code and cannot drift apart. That is also what makes the period's numbers
/// *not* the sum of its days' -- see [`PeriodReport::attention_daily_sum_seconds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    first: Day,
    last: Day,
    kind: PeriodKind,
    /// A time-of-day narrowing of a single day, in the same half-open sense
    /// [`DayWindow`] gives `log` -- `report --day --from/--to` shares that exact
    /// rule rather than growing a second copy of it. Only ever set when
    /// `kind == PeriodKind::Day`: narrowing by time has no single day to resolve
    /// against on a week, a month or a range, which is why those refuse `--from`/
    /// `--to` outright instead of guessing which of their days a time would apply
    /// to.
    from: Option<TimeOfDay>,
    to: Option<TimeOfDay>,
}

impl Period {
    pub fn of_day(day: Day) -> Self {
        Period {
            first: day,
            last: day,
            kind: PeriodKind::Day,
            from: None,
            to: None,
        }
    }

    /// One day, narrowed to a stretch of it by `--from`/`--to` -- the same
    /// half-open narrowing [`DayWindow::new`] gives `log`, reused here (and its
    /// refusal of a `from` at or after `to` with it) so a narrowed single day
    /// cannot resolve to two different windows depending on which command read it.
    pub fn of_day_window(
        day: Day,
        from: Option<TimeOfDay>,
        to: Option<TimeOfDay>,
    ) -> Result<Self, ReportError> {
        DayWindow::new(day, from, to)?;
        Ok(Period {
            first: day,
            last: day,
            kind: PeriodKind::Day,
            from,
            to,
        })
    }

    /// The seven days ending on `last`, inclusive -- a rolling week, not a calendar
    /// one.
    ///
    /// Counted in days rather than by naming a weekday, so it crosses a month or a
    /// year without a special case. Which kind of week it is has to stay visible:
    /// [`Self::label`] prints "7 days ending <date>" rather than a week number,
    /// because an ISO week starts on a Monday and this does not.
    pub fn week_ending(last: Day) -> Self {
        Period {
            first: Day::from_epoch_day(last.epoch_day() - 6),
            last,
            kind: PeriodKind::Week,
            from: None,
            to: None,
        }
    }

    /// One calendar month named `YYYY-MM`, from its first day to its last.
    ///
    /// Parsed by the same discipline [`Day::parse`] uses -- fixed widths, digits only,
    /// a month the calendar has -- for the same reason: `2026-13` is refused rather
    /// than rolled into January, which would head a report with a month that never
    /// happened over another month's instants.
    ///
    /// The last day is the day before the first of the next month, so February's
    /// length comes from the calendar and depends on the year, rather than from a
    /// table of month lengths that a leap year would quietly falsify.
    pub fn month(text: &str) -> Result<Self, ReportError> {
        let invalid = || ReportError::InvalidMonth(text.to_string());
        let parts: Vec<&str> = text.split('-').collect();
        let [y, m] = parts[..] else {
            return Err(invalid());
        };
        if (y.len(), m.len()) != (4, 2) {
            return Err(invalid());
        }
        // Digits only: Rust's integer parsers accept a leading `+`, which slips a sign
        // past the width check -- the same hole `Day::parse` closes.
        if !text.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
            return Err(invalid());
        }
        let year: i64 = y.parse().map_err(|_| invalid())?;
        let month: u32 = m.parse().map_err(|_| invalid())?;
        if !(1..=12).contains(&month) {
            return Err(invalid());
        }
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        Ok(Period {
            first: Day {
                year,
                month,
                day: 1,
            },
            last: Day::from_epoch_day(crate::clock::days_from_civil(next_year, next_month, 1) - 1),
            kind: PeriodKind::Month,
            from: None,
            to: None,
        })
    }

    /// An arbitrary run of days, both ends inclusive.
    ///
    /// Equal ends are one day, not nothing: unlike `--from`/`--to` on a *day*, which
    /// are half-open times, each end here names a whole day. A backwards pair is
    /// refused rather than reordered -- swapping them silently would report a period
    /// the person did not ask for, headed by one they did.
    pub fn range(first: Day, last: Day) -> Result<Self, ReportError> {
        if first > last {
            return Err(ReportError::InvertedPeriod {
                from: first.label(),
                to: last.label(),
            });
        }
        Ok(Period {
            first,
            last,
            kind: PeriodKind::Range,
            from: None,
            to: None,
        })
    }

    /// How many calendar days the period holds, both ends counted.
    pub fn calendar_days(&self) -> i64 {
        self.last.epoch_day() - self.first.epoch_day() + 1
    }

    pub fn is_single_day(&self) -> bool {
        self.first == self.last
    }

    /// The half-open UTC range `[start, end)` the whole period covers -- or, for a
    /// single day narrowed by [`Self::of_day_window`], the narrower stretch of it
    /// `--from`/`--to` named, exactly as [`DayWindow::utc_window`] resolves the same
    /// pair for `log`.
    pub fn utc_window(&self, tz_offset: TzOffset) -> (i64, i64) {
        let midnight = self.first.utc_window(tz_offset).0;
        let period_end = self.last.utc_window(tz_offset).1;
        (
            self.from
                .map_or(midnight, |t| midnight + t.seconds_from_midnight()),
            self.to
                .map_or(period_end, |t| midnight + t.seconds_from_midnight()),
        )
    }

    /// How `--from`/`--to` narrowed this period to a stretch of its one day, or
    /// `None` for a period nothing narrowed -- every multi-day period, and a single
    /// day named without either flag. The same label [`DayWindow::range_label`]
    /// prints for `log`, so a narrowed single day reads identically whichever
    /// command drew it.
    pub fn range_label(&self) -> Option<String> {
        bound_range_label(self.from, self.to)
    }

    /// How the header names the period: a bare date for one day, and otherwise the
    /// days it spans plus how many, so that the header never leaves the extent of the
    /// report to be worked out from the flag that produced it.
    pub fn label(&self) -> String {
        let span = format!("{} .. {}", self.first.label(), self.last.label());
        let days = days_label(self.calendar_days());
        match self.kind {
            PeriodKind::Day => self.first.label(),
            PeriodKind::Week => format!("{span}  ({days} ending {})", self.last.label()),
            PeriodKind::Month => format!(
                "{:04}-{:02}  ({span}, {days})",
                self.first.year, self.first.month
            ),
            PeriodKind::Range => format!("{span}  ({days})"),
        }
    }
}

/// `n days`, or `1 day` -- a count a reader has to be able to read, and one that ends
/// up in a sentence often enough to be worth getting right in one place.
pub(crate) fn days_label(days: i64) -> String {
    if days == 1 {
        format!("{days} day")
    } else {
        format!("{days} days")
    }
}

/// A wall-clock `HH:MM` in the reporting offset -- what `--from` and `--to` name,
/// before they become instants.
///
/// Parsed by the same discipline [`Day::parse`] uses, for the same reason: a bound
/// the clock never shows is a refusal, never a value clamped to the nearest one it
/// does. `25:00` silently becoming 23:59 would narrow a window to somewhere the
/// person did not point, and the header would still print a range that looked right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    hour: u32,
    minute: u32,
}

impl TimeOfDay {
    /// Parse `HH:MM`, strictly: fixed widths, digits only, and a time the clock has.
    ///
    /// `24:00` is refused along with `25:00`. It is a real way to write the end of a
    /// day, but it is not a time of day, and accepting it on `--to` while refusing it
    /// on `--from` would make one flag mean something the other does not. The end of
    /// the day is what leaving `--to` off already means.
    pub fn parse(text: &str) -> Result<Self, ReportError> {
        let invalid = || ReportError::InvalidTimeOfDay(text.to_string());
        let Some((h, m)) = text.split_once(':') else {
            return Err(invalid());
        };
        if (h.len(), m.len()) != (2, 2) {
            return Err(invalid());
        }
        // Digits only. Rust's integer parsers accept a leading `+`, which would slip
        // a sign past the width check -- the same hole `Day::parse` closes.
        if !text.bytes().all(|b| b.is_ascii_digit() || b == b':') {
            return Err(invalid());
        }
        let hour: u32 = h.parse().map_err(|_| invalid())?;
        let minute: u32 = m.parse().map_err(|_| invalid())?;
        if hour > 23 || minute > 59 {
            return Err(invalid());
        }
        Ok(TimeOfDay { hour, minute })
    }

    /// How far into the local day this time is.
    pub fn seconds_from_midnight(&self) -> i64 {
        i64::from(self.hour) * 3600 + i64::from(self.minute) * 60
    }

    pub fn label(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

/// The stretch of one day a command reads: the whole of it, or the part `--from` and
/// `--to` narrowed it to.
///
/// Narrowing is a narrowing of the *query*, not of the drawn axis -- every count,
/// duration and coverage number a command produces comes out of the range this
/// resolves to. That is the honest reading of "show me this stretch of the day", and
/// it is why [`Self::range_label`] exists: a view that quietly reported less would be
/// indistinguishable from a quieter day.
///
/// `report`'s single-day form narrows the same way, through [`Period::of_day_window`]
/// rather than through this type directly -- but it validates the pair by
/// constructing one of these and discarding it, so the two commands cannot come to
/// disagree about which `--from`/`--to` pairs name a stretch of a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayWindow {
    day: Day,
    from: Option<TimeOfDay>,
    to: Option<TimeOfDay>,
}

impl DayWindow {
    /// The day, narrowed by either bound or by neither -- two `None`s are the whole
    /// of it, local midnight to local midnight.
    ///
    /// Refuses a `from` at or after its `to`: the range is half-open, so both of
    /// those hold nothing, and reporting nothing for them would look exactly like a
    /// stretch of the day with no work in it.
    pub fn new(
        day: Day,
        from: Option<TimeOfDay>,
        to: Option<TimeOfDay>,
    ) -> Result<Self, ReportError> {
        if let (Some(from), Some(to)) = (from, to)
            && from >= to
        {
            return Err(ReportError::InvertedRange {
                from: from.label(),
                to: to.label(),
            });
        }
        Ok(DayWindow { day, from, to })
    }

    pub fn day(&self) -> Day {
        self.day
    }

    /// The half-open UTC range `[start, end)` this window covers, in epoch seconds.
    pub fn utc_window(&self, tz_offset: TzOffset) -> (i64, i64) {
        let (midnight, next_midnight) = self.day.utc_window(tz_offset);
        (
            self.from
                .map_or(midnight, |t| midnight + t.seconds_from_midnight()),
            self.to
                .map_or(next_midnight, |t| midnight + t.seconds_from_midnight()),
        )
    }

    /// How the header names the narrowing, or `None` for a whole day.
    ///
    /// A bound that was not given is not completed with one: `--to 24:00` is not a
    /// time this command accepts, so `24:00` is not a time this command prints.
    pub fn range_label(&self) -> Option<String> {
        bound_range_label(self.from, self.to)
    }
}

/// `--from`/`--to` as a header names them: `13:00-15:00`, `from 13:00`, `to 15:00`,
/// or nothing for two absent bounds. Shared by [`DayWindow::range_label`] and
/// [`Period::range_label`] so a day narrowed through `log` and a day narrowed
/// through `report --day` describe the same narrowing the same way.
fn bound_range_label(from: Option<TimeOfDay>, to: Option<TimeOfDay>) -> Option<String> {
    match (from, to) {
        (None, None) => None,
        (Some(from), Some(to)) => Some(format!("{}-{}", from.label(), to.label())),
        (Some(from), None) => Some(format!("from {}", from.label())),
        (None, Some(to)) => Some(format!("to {}", to.label())),
    }
}

/// What `--from` or `--to` named: a calendar date, which sets a day *boundary*, or a
/// time of day, which *narrows* within one. `report` and `log` parse both flags
/// through this one shape so the two commands cannot come to mean two different
/// things by the same names again -- which is exactly how they drifted before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Date(Day),
    Time(TimeOfDay),
}

impl Bound {
    /// `-` says date, `:` says time -- whichever separator `text` uses decides which
    /// parser reads it, so a malformed date is refused as a malformed date rather
    /// than retried as a time it was never shaped like.
    pub fn parse(text: &str) -> Result<Self, ReportError> {
        if text.contains(':') {
            TimeOfDay::parse(text).map(Bound::Time)
        } else {
            Day::parse(text).map(Bound::Date)
        }
    }
}

/// Read one of `--from`/`--to` as a time on a command whose day is already fixed --
/// `log`, always, or `report` once `--day` has named one. A well-formed date is not
/// malformed input here, so it is refused with [`ReportError::DateOnSingleDay`]
/// rather than with the parse error a value the calendar never had would get.
fn bound_time(
    raw: Option<String>,
    flag: &'static str,
    scope: DayScope,
) -> Result<Option<TimeOfDay>, ReportError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match Bound::parse(&raw)? {
        Bound::Time(t) => Ok(Some(t)),
        Bound::Date(d) => Err(ReportError::DateOnSingleDay {
            flag,
            date: d.label(),
            scope,
        }),
    }
}

/// `log`'s day and its narrowing: the day `--day` names, or today, optionally
/// narrowed by `--from`/`--to`.
///
/// `log` is always scoped to one day, so a calendar date on either flag is refused
/// outright by [`bound_time`] rather than accepted and silently reinterpreted. Times
/// are combined through [`DayWindow::new`], the same call `report`'s narrowed single
/// day makes through [`Period::of_day_window`], so the two cannot disagree about
/// what a given `--from`/`--to` pair narrows a day down to.
pub fn day_window(
    day: Option<String>,
    from: Option<String>,
    to: Option<String>,
    tz_offset: TzOffset,
) -> Result<DayWindow, ReportError> {
    let day = match day {
        Some(raw) => Day::parse(&raw)?,
        None => Day::today(tz_offset),
    };
    let from = bound_time(from, "from", DayScope::Log)?;
    let to = bound_time(to, "to", DayScope::Log)?;
    DayWindow::new(day, from, to)
}

/// `report`'s period once `--week`/`--month` have ruled themselves out: a run of
/// days named by `--from`/`--to` as two dates, one day (`--day`'s, or today) narrowed
/// by `--from`/`--to` as times, or a bare day.
///
/// `--from`/`--to` mean here exactly what they mean on `log`: a date sets a day
/// boundary, a time narrows within one. The combinations that name neither -- a time
/// with no day to narrow, a date once `--day` has already fixed the day, a date
/// paired with a time -- are refused with the reason stated rather than guessed
/// past, the same discipline [`DayWindow::new`]'s ordering refusal already follows.
pub fn day_period(
    day: Option<String>,
    from: Option<String>,
    to: Option<String>,
    tz_offset: TzOffset,
) -> Result<Period, ReportError> {
    if let Some(raw) = day {
        // `--day` already names the one day this report covers: `--from`/`--to` can
        // only narrow it, exactly as `log --day` does.
        let day = Day::parse(&raw)?;
        let from = bound_time(from, "from", DayScope::ReportDay)?;
        let to = bound_time(to, "to", DayScope::ReportDay)?;
        return Period::of_day_window(day, from, to);
    }

    let from = from.as_deref().map(Bound::parse).transpose()?;
    let to = to.as_deref().map(Bound::parse).transpose()?;

    match (from, to) {
        (None, None) => Ok(Period::of_day(Day::today(tz_offset))),

        // Two dates name a run of days -- the range `report --from/--to` has always
        // named. Unchanged, and unreachable through `--day` above.
        (Some(Bound::Date(from)), Some(Bound::Date(to))) => Period::range(from, to),
        // One date alone completes to nothing a person did not have to guess at:
        // today, the ledger's last day and a month's end are three different
        // periods, so none of them is chosen quietly.
        (Some(Bound::Date(_)), None) => Err(ReportError::IncompleteRange {
            given: "from",
            missing: "to",
        }),
        (None, Some(Bound::Date(_))) => Err(ReportError::IncompleteRange {
            given: "to",
            missing: "from",
        }),

        // A time (or two) with no `--day` narrows today -- the same default `log`
        // already uses when it is given no day either.
        (Some(Bound::Time(from)), Some(Bound::Time(to))) => {
            Period::of_day_window(Day::today(tz_offset), Some(from), Some(to))
        }
        (Some(Bound::Time(from)), None) => {
            Period::of_day_window(Day::today(tz_offset), Some(from), None)
        }
        (None, Some(Bound::Time(to))) => {
            Period::of_day_window(Day::today(tz_offset), None, Some(to))
        }

        // A date on one end and a time on the other names neither a range (the time
        // is not a day boundary) nor a narrowed day (the date leaves no single day
        // for the time to narrow, since --day was not given either).
        (Some(Bound::Date(date)), Some(Bound::Time(time))) => Err(ReportError::MixedBoundKinds {
            date_flag: "from",
            date: date.label(),
            time_flag: "to",
            time: time.label(),
        }),
        (Some(Bound::Time(time)), Some(Bound::Date(date))) => Err(ReportError::MixedBoundKinds {
            date_flag: "to",
            date: date.label(),
            time_flag: "from",
            time: time.label(),
        }),
    }
}

/// Whether workstream rules were in force, and where they were looked for. Absent is
/// a state to report, not a failure: without rules everything is `unassigned`, and
/// the output says the config is missing rather than implying nothing is assignable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigStatus {
    Loaded { path: PathBuf },
    Absent { path: PathBuf },
}

/// One repository's day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRow {
    /// The normalized identity (`github.com/acme/api`), or the opaque `rep_…` ref if
    /// this ledger never registered a display for it -- never a guessed name.
    pub repository: String,
    /// `None` when no `[[rule]]` matched: unassigned, and shown as such.
    pub workstream: Option<String>,
    /// This repository's share of the day-wide attention union, by nearest anchor.
    /// Shares across all rows sum to [`PeriodReport::attention_union_seconds`].
    pub attention_seconds: i64,
    /// What this repository's own prompt windows union to on their own. Larger than
    /// `attention_seconds` whenever another repository's windows overlapped it; these
    /// added up are the naive sum, which is not the day's total.
    pub attention_alone_seconds: i64,
    /// This repository's records clustered on their own -- the naive basis. The
    /// day's agent clock comes from the global timeline instead; see this module's
    /// header, decision 2.
    pub agent_seconds: i64,
    pub prompts: u64,
    /// Commits of the person's own that landed in this repository in the period.
    ///
    /// Deliberately beside the two clocks rather than inside either: it is a count of
    /// instants, and adding it to a duration is a category error. A repository can have
    /// a row here with commits and no time at all -- work committed from an editor, or
    /// on a day whose session was never observed -- and that is a real thing to be able
    /// to see rather than a row to suppress.
    pub commits: u64,
}

/// One workstream's day: the repositories its rules gathered, and their totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkstreamRow {
    /// `None` is `unassigned` -- no rule matched. It is never "repository unknown",
    /// which is [`PeriodReport::unattributed`].
    pub workstream: Option<String>,
    pub attention_seconds: i64,
    /// Sum of the member repositories' own clusters. May overlap, and is labelled so.
    pub agent_seconds: i64,
    pub prompts: u64,
    pub repositories: Vec<String>,
}

/// Activity whose records carried no repository identity at all: a cwd outside the
/// ghq tree, or none recorded and a session that never named one.
///
/// Kept apart from `unassigned` on purpose. `unassigned` means a rule is missing;
/// this means the observation never had a repository to assign. Conflating them
/// would let a coverage problem hide inside a configuration one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unattributed {
    pub attention_seconds: i64,
    pub agent_seconds: i64,
    pub prompts: u64,
    pub observations: u64,
    /// Always zero in practice: the git adapter refuses to emit a commit it cannot
    /// place in a repository, so an unattributed commit does not exist. The field is
    /// here so that if one ever did, it would be counted rather than fall out of the
    /// totals between two match arms.
    pub commits: u64,
}

/// What the ledger could and could not say, kept in the three separate registers
/// design §13 requires rather than collapsed into one word.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Every observation whose time falls in the day, gap markers included.
    pub observations_in_window: u64,
    /// Lines the importer could not transform. Excluded from every clock.
    pub gap_markers: u64,
    /// Observations whose `time` is not when their activity happened, and which say so
    /// (`data.time_basis`): records a Codex fork copied out of its parent's transcript,
    /// carrying the copy's write time. Real events -- the human turns among them are
    /// human turns -- but at an unknown instant, so they are excluded from every clock
    /// exactly as gap markers are, and counted here rather than made to disappear.
    pub observations_with_inherited_time: u64,
    /// Observations that carried no repository identity (gap markers excluded -- a
    /// gap never had one to lose, which is the same rule `cclogger import` reports by).
    pub observations_without_repository: u64,
    /// Of those, the ones that were human prompts -- attention that could be measured
    /// but not attributed.
    pub prompts_without_repository: u64,
    /// Prompts whose own `data.origin` says they were not a human's: a subagent's own
    /// dispatch to itself, or (once cclog acquires a scheduler) a run nobody typed.
    /// Real activity -- kept on the agent clock, never dropped, see
    /// [`prompt_origin_denies_anchor`] -- but not an anchor for an attention window, and
    /// counted here rather than let the day's prompt total shrink with no explanation.
    pub prompts_not_anchored: u64,
    /// Rows whose `occurred_at` could not be parsed, so they could not be placed on
    /// any clock. Counted rather than dropped silently.
    pub observations_without_usable_time: u64,
    /// What each source can speak about at all, across the whole ledger.
    pub sources: Vec<SourceRange>,
    /// Where hook capture starts, if it has started at all.
    ///
    /// Kept apart from [`Coverage::sources`] because it answers a different question.
    /// `sources` groups by the *vendor*, and a hook observation is a Claude Code
    /// observation, so it falls inside that vendor's range and disappears into it. This
    /// is the *channel*: the first instant this machine has any hook evidence for.
    /// Everything before it was seen only through transcripts, and no re-run can change
    /// that -- hooks record from installation forward and Claude Code replays nothing.
    /// `None` when no hooks are installed, which is the ordinary state and prints
    /// nothing.
    pub hook_capture: Option<SourceRange>,
}

/// The quartiles of a set of measured gaps, in seconds.
///
/// Each is an *observed* gap, picked by nearest rank rather than interpolated between
/// two neighbours: an interpolated quartile is a number no reaction actually took, and
/// this report does not print those.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quartiles {
    pub p25: i64,
    pub median: i64,
    pub p75: i64,
}

/// A set of measured gaps, as this report is willing to state them: how many there
/// were, and what shape they had.
///
/// No sum and no mean, by construction rather than by omission at the render. Summing
/// gaps from sessions that ran concurrently produces a figure larger than the clock
/// they happened on -- the same naive-sum error decision 1 covers for attention, one
/// level down -- and the tail here runs into the tens of minutes, so a mean sits
/// between the way most reactions go and the few long ones, describing neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GapDistribution {
    /// How many gaps were measured. A fact about what happened, reported at any size.
    pub count: u64,
    /// `None` under [`MIN_GAPS_FOR_QUARTILES`]: too few gaps to have a shape, said
    /// rather than computed from the handful that are there.
    pub quartiles: Option<Quartiles>,
}

impl GapDistribution {
    /// The distribution `gaps` has, or the statement that it is too small to have one.
    fn of(mut gaps: Vec<i64>) -> Self {
        gaps.sort_unstable();
        GapDistribution {
            count: gaps.len() as u64,
            quartiles: (gaps.len() >= MIN_GAPS_FOR_QUARTILES).then(|| Quartiles {
                p25: nearest_rank(&gaps, 25),
                median: nearest_rank(&gaps, 50),
                p75: nearest_rank(&gaps, 75),
            }),
        }
    }
}

/// The `percentile`-th value of an ascending `sorted`, by nearest rank: the smallest
/// observed gap that at least `percentile`% of the sample is at or below.
///
/// Nearest rank, not linear interpolation. Every value this returns is a gap that
/// really was measured; interpolating between the two middle values of an even sample
/// would print a duration nothing took, which is the one thing this report may not do.
///
/// Only ever called on a sample of at least [`MIN_GAPS_FOR_QUARTILES`], but the rank is
/// floored at 1 anyway so that a percentile can never index before the slice.
fn nearest_rank(sorted: &[i64], percentile: usize) -> i64 {
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// One repository's reaction times.
///
/// `repository` is `None` for the records that carried no repository identity at all --
/// the same distinction [`PeriodReport::unattributed`] draws, kept rather than folded
/// away so that the rows always add back to the period's own count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryGaps {
    pub repository: Option<String>,
    pub response: GapDistribution,
}

/// How long the person took to answer what the assistant produced, and how far apart
/// their prompts were.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseTimes {
    /// Assistant output completed -> the human prompt that consumed it. The person's
    /// own reaction, with the model's generation time excluded.
    pub response: GapDistribution,
    /// Consecutive human prompts in one session -- the same waiting with the
    /// generation time left in, reported beside the reaction for reference. The
    /// difference between the two is roughly what the model spent.
    pub interval: GapDistribution,
    /// Prompts that found no unconsumed completion: the person sent another before
    /// anything new came back, or answered output that an earlier prompt had already
    /// answered. There is no reaction to measure for these, and there are a great many
    /// of them -- roughly a third, on a month of real data.
    pub back_to_back_prompts: u64,
    /// Every prompt the walk actually saw, and the denominator
    /// [`Self::back_to_back_prompts`] is out of. Equal to [`PeriodReport::prompts`]
    /// unless some prompt's `subject` named no session to pair it inside; the render
    /// states the difference rather than letting the two counts quietly disagree.
    pub prompts_walked: u64,
    /// Reaction times split by the repository the *prompt* resolved to -- the work the
    /// person turned back to, which is also what every other per-repository number
    /// here keys on.
    pub by_repository: Vec<RepositoryGaps>,
}

/// One record the reaction-time walk cares about. Everything else in a session -- tool
/// calls, session boundaries -- is neither, and passes without disturbing it.
enum Turn {
    /// An assistant finished producing output ([`RESPONSE_EVENT_TYPE`]).
    Completed,
    /// A human sent a prompt ([`PROMPT_EVENT_TYPE`]), with the repository it will be
    /// attributed to.
    Prompted(Option<String>),
}

/// The opaque session ref a `subject` names, or `None` when it names no session.
///
/// `subject` is a hierarchical path (`session/ses_2XQ/turn/trn_91M`) and every producer
/// that knows which session a record belongs to writes that ref in the first segment.
/// One that does not -- a gap marker, whose subject is `source/gap/…`, or a Codex tool
/// event whose session could not be resolved -- names none. A record whose session is
/// unknown is left out of the pairing rather than pooled with every other unknown,
/// which would let a prompt in one session consume a completion from another.
fn session_ref(subject: Option<&str>) -> Option<&str> {
    subject?
        .strip_prefix("session/")?
        .split('/')
        .next()
        .filter(|found| !found.is_empty())
}

/// Walk each session's turns in time order and pair completions with the prompts that
/// answered them.
///
/// Per session, never globally: two sessions run side by side all the time, and a
/// prompt that consumed a *different* session's completion would report the person
/// reacting to output they were not looking at.
fn measure_response_times(by_session: BTreeMap<String, Vec<(i64, Turn)>>) -> ResponseTimes {
    let mut response: Vec<i64> = Vec::new();
    let mut interval: Vec<i64> = Vec::new();
    let mut by_repository: BTreeMap<Option<String>, Vec<i64>> = BTreeMap::new();
    let mut back_to_back_prompts = 0;
    let mut prompts_walked = 0;

    for (_session, mut turns) in by_session {
        // Sorted on the parsed instant rather than trusted from the query's ORDER BY:
        // `occurred_at` sorts as text, and `.` (0x2E) precedes `Z` (0x5A), so two
        // records inside one second at different fractional precision can come back in
        // the wrong order. A gap read off a reversed pair would be negative -- a
        // fabricated measurement of a reaction that ran backwards. `sort_by_key` is
        // stable, so records sharing a second keep the order the ledger gave them.
        turns.sort_by_key(|(instant, _)| *instant);

        // At most one unconsumed completion, and it answers at most one prompt.
        let mut unconsumed: Option<i64> = None;
        let mut previous_prompt: Option<i64> = None;

        for (instant, turn) in turns {
            match turn {
                // Supersedes any older unconsumed one: an assistant turn that called
                // tools emits several completions, and what the person reacted to is
                // the last thing they were shown.
                Turn::Completed => unconsumed = Some(instant),
                Turn::Prompted(repository) => {
                    prompts_walked += 1;
                    // `take` is the whole point: the completion is spent answering
                    // this prompt and can never answer another. Leaving it in place
                    // would measure the next prompt from output that already belongs
                    // to this one -- a number that is not a reaction time, and is
                    // longer than any reaction was.
                    match unconsumed.take() {
                        Some(completed_at) => {
                            let gap = instant - completed_at;
                            response.push(gap);
                            by_repository.entry(repository).or_default().push(gap);
                        }
                        None => back_to_back_prompts += 1,
                    }
                    if let Some(previous) = previous_prompt.replace(instant) {
                        interval.push(instant - previous);
                    }
                }
            }
        }
    }

    let mut rows: Vec<RepositoryGaps> = by_repository
        .into_iter()
        .map(|(repository, gaps)| RepositoryGaps {
            repository,
            response: GapDistribution::of(gaps),
        })
        .collect();
    // Most-measured first, then by name; the records that carried no repository last,
    // the same place `unassigned` takes in the workstream table. A stable order, so a
    // row does not move because one day's counts came out differently.
    rows.sort_by(|a, b| match (&a.repository, &b.repository) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => b
            .response
            .count
            .cmp(&a.response.count)
            .then_with(|| x.cmp(y)),
    });

    ResponseTimes {
        response: GapDistribution::of(response),
        interval: GapDistribution::of(interval),
        back_to_back_prompts,
        prompts_walked,
        by_repository: rows,
    }
}

/// One period -- a day, a week, a month, a range -- as the ledger has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodReport {
    /// How the period names itself: a bare date for one day, otherwise the days it
    /// spans and what named them.
    pub period: String,
    /// How `--from`/`--to` narrowed this period to a stretch of its one day, as the
    /// header names it (`13:00-15:00`), or `None` for a whole period. Only ever
    /// `Some` when `single_day` is true -- a week, a month or a range has no single
    /// day for a time to narrow, which is why naming one on those is refused rather
    /// than resolved here. Mirrors `log`'s own `DayLog::range` field exactly, so a
    /// day narrowed through either command's `--from`/`--to` is described the same
    /// way.
    pub range: Option<String>,
    /// Whether the period is one day. A day is not averaged over itself, and it does
    /// not print a second reading of a total that cannot disagree with the first.
    pub single_day: bool,
    /// Every day the period holds, worked or not.
    pub calendar_days: i64,
    /// The days of it the ledger holds an observation of activity for. Gap markers do
    /// not count: they are dated to when `cclogger archive` ran, so a day holding only
    /// those was never observed to have anything happen on it.
    pub active_days: i64,
    pub tz_offset: TzOffset,
    pub window_start_utc: String,
    pub window_end_utc: String,
    pub config: ConfigStatus,
    /// The day's human attention: every prompt window unioned, overlaps counted once.
    /// This is the number the report leads with, and it is an estimate.
    pub attention_union_seconds: i64,
    /// Per-repository unions added together. Larger than the union whenever two
    /// repositories were active in the same minutes. Reported, and labelled, because
    /// hiding it is how the two get conflated.
    pub attention_naive_sum_seconds: i64,
    /// Each day's own union, added up -- what running `report` once per day and
    /// totalling the answers gives. Never smaller than the period's union: a window
    /// is anchored to a prompt and never clipped, so one opened before local midnight
    /// is counted whole in that day and again in whatever the next day's windows
    /// cover. The period's union counts those minutes once.
    pub attention_daily_sum_seconds: i64,
    /// Agent execution: every observation that does not anchor an attention window
    /// (decision 2) -- every non-prompt one, and a prompt denied an anchor by its own
    /// `data.origin` -- clustered on one cross-session timeline, then unioned.
    pub agent_union_seconds: i64,
    /// The same records clustered per repository and added up. Not comparable to the
    /// union in either direction: it double-counts concurrent work, and it splits
    /// stretches that the global timeline keeps whole.
    pub agent_naive_sum_seconds: i64,
    /// Each day's own clustering, added up -- what running `report` once per day and
    /// totalling the answers gives. Never larger than the period's: a stretch of work
    /// running through local midnight is one cluster over the period and two over the
    /// two days, which drops the silence between the last record before midnight and
    /// the first after it.
    pub agent_daily_sum_seconds: i64,
    pub prompts: u64,
    /// Commits of the person's own that landed in the period, across every repository.
    ///
    /// Evidence, not a clock, and part of neither duration above: both unions are
    /// computed from observations that are not commits, and `active_days` does not count
    /// a day whose only observation was one. That last one is the least obvious and the
    /// most consequential -- `active_days` divides both daily averages, so admitting a
    /// commit-only day would spread the same measured time over more days and lower
    /// every average with it.
    pub commits: u64,
    /// The person's own reaction times, and the prompt intervals they sit inside --
    /// computed over the period's own observations, exactly as every clock above is,
    /// so a period's distribution is its own gaps and never an average of its days'
    /// medians.
    pub response_times: ResponseTimes,
    pub repositories: Vec<RepositoryRow>,
    pub workstreams: Vec<WorkstreamRow>,
    pub unattributed: Unattributed,
    /// Whether any source observed anything overlapping this period. `false` means
    /// nothing was collected for it -- which is not the same as a period with no work.
    pub within_observed_range: bool,
    pub coverage: Coverage,
}

/// Read one period out of the ledger at `root`.
///
/// Every clock here is computed over the period's *own* observations: its anchors are
/// unioned once, and its non-prompt records are clustered once. A period is therefore
/// not the sum of the days in it, and must not be assembled that way -- the two
/// readings disagree wherever work runs through local midnight, which is why both are
/// reported side by side (`attention_daily_sum_seconds`, `agent_daily_sum_seconds`).
///
/// `tz_offset` is a fixed offset, not a zone: one number, taken as it stands for every
/// day in the period. The CLI reads it off the machine the command runs on (see
/// [`crate::tz::detect`]), which is the offset in force *now* -- so a period spanning a
/// DST transition is bucketed on one side of it throughout. Following the transition
/// would need a real tz database, and inventing one from a single offset would silently
/// mis-slice the days around it.
pub fn run_report(
    root: &Path,
    period: Period,
    tz_offset: TzOffset,
) -> Result<PeriodReport, ReportError> {
    // -12:00 to +14:00 is the range real zones use (Baker Island to Kiritimati). An
    // offset outside it would still slice a 24-hour window, just one no clock on earth
    // keeps.
    if !tz_offset.is_real_zone() {
        return Err(ReportError::InvalidOffset(tz_offset));
    }
    let ledger_path = root.join("ledger.db");
    if !ledger_path.exists() {
        return Err(ReportError::NoLedger(ledger_path));
    }
    // `Ledger::open` reconciles an out-of-date schema and stamps `user_version`
    // forward as a side effect of opening -- a write, and one an older cclog cannot
    // undo. A read-only command asks first and stops, exactly as `import --dry-run`
    // does.
    if cclogger_archive::needs_schema_upgrade(root)? {
        return Err(ReportError::LedgerNeedsUpgrade(ledger_path));
    }

    let (config, rules) = load_rules(root)?;
    let ledger = Ledger::open(root)?;
    let (start, end) = period.utc_window(tz_offset);

    // The range query is a *prefilter*, and it is deliberately asked for more than the
    // period: the exact half-open test is the `secs < start || secs >= end` comparison
    // below, on the parsed instant.
    //
    // Two reasons for the slack. `occurred_at` is compared lexicographically, and a
    // value normalization could not rewrite is stored verbatim, offset and all (see
    // `cclogger_archive::occurred_at`) -- its text order then says nothing about the
    // instant it names, so a bound drawn exactly at the day can *miss* a record that
    // belongs to it, which no later filtering can recover. A day of margin covers
    // every offset a real timestamp carries (-12 to +14). Second, the bounds are
    // written to the second and without the `Z` because `.` (0x2E) sorts before `Z`
    // (0x5A), so a `Z`-suffixed bound both admits `…T15:00:00.000Z` at the exclusive
    // end and drops it at the inclusive start. With the margin, no result depends on
    // that any more -- the instant filter decides the day either way -- but a bound
    // that is exact at its own boundary second is worth more than one that is quietly
    // off by one.
    //
    // The residual, stated rather than papered over: a verbatim value whose text sorts
    // more than a day away from the instant it names is still unreachable this way.
    // Finding those needs a ledger-wide health query, not a day's range.
    const PREFILTER_MARGIN_SECONDS: i64 = 86_400;
    let rows = ledger.observations_between(
        &prefix_bound(start - PREFILTER_MARGIN_SECONDS),
        &prefix_bound(end + PREFILTER_MARGIN_SECONDS),
    )?;
    let displays: BTreeMap<String, String> = ledger.identities("repository")?.into_iter().collect();

    let mut coverage = Coverage::default();
    // `None` is "this observation carried no repository", carried through the clocks
    // as its own key so its time is never quietly folded into a named repository's.
    let mut anchors: Vec<(i64, Option<String>)> = Vec::new();
    let mut prompts_by_repository: BTreeMap<Option<String>, u64> = BTreeMap::new();
    let mut agent_instants: Vec<i64> = Vec::new();
    let mut agent_by_repository: BTreeMap<Option<String>, Vec<i64>> = BTreeMap::new();
    let mut commits_by_repository: BTreeMap<Option<String>, u64> = BTreeMap::new();
    // The local calendar day each observation fell on, for the two numbers a period
    // has and a day does not: how many of its days were worked at all, and what
    // reporting each of them separately and adding the answers up would have given.
    let mut active_days: BTreeSet<i64> = BTreeSet::new();
    let mut anchors_by_day: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut agent_by_day: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    // The prompts and completions of each session, kept apart by session ref so that a
    // prompt can only ever consume a completion from the session it happened in.
    let mut turns_by_session: BTreeMap<String, Vec<(i64, Turn)>> = BTreeMap::new();

    for row in &rows {
        let instant = rfc3339::epoch_seconds(&row.occurred_at);
        if let Some(secs) = instant
            && (secs < start || secs >= end)
        {
            continue; // sorted into range, but not this day's instant
        }
        coverage.observations_in_window += 1;

        if row.event_type == GAP_EVENT_TYPE {
            // Deliberately before `active_days`: a gap marker is dated to when
            // `cclogger archive` ran, so the day it lands on is a fact about
            // collection. Counting it would make a day nothing was observed on look
            // like a day that was worked.
            coverage.gap_markers += 1;
            continue;
        }
        if !time_is_when_it_happened(row) {
            // Also before `active_days`, and for the same reason a gap marker is: a
            // copied record's timestamp is when the fork was written, so the day it
            // lands on is a fact about copying. Counting it would make a day the
            // parent's work was merely re-serialized on look like a day it was done.
            coverage.observations_with_inherited_time += 1;
            continue;
        }
        let Some(secs) = instant else {
            // No instant means no day either. Counted as coverage above, and left out
            // of the day buckets rather than assigned to a guessed one.
            coverage.observations_without_usable_time += 1;
            continue;
        };
        let local_day = (secs + tz_offset.seconds()).div_euclid(86_400);
        let repository = row.repository_ref.as_ref().map(|opaque| {
            displays
                .get(opaque)
                .cloned()
                // No registered display: show the opaque ref rather than invent a
                // name for it. It is not readable, but it is true.
                .unwrap_or_else(|| opaque.clone())
        });

        if row.event_type == COMMIT_EVENT_TYPE {
            // Counted, and then out -- before `active_days`, before both clocks, and
            // before the coverage counters that describe what those clocks were built
            // from. A commit contributes to exactly two numbers in this whole function:
            // `observations_in_window` above, and this count.
            //
            // The `active_days` ordering is the load-bearing part. It divides both daily
            // averages, so a day whose only evidence is a commit would lower each of
            // them by adding a day with no measurable attention to the denominator.
            // Nothing is lost by leaving it out: the commit is still reported, under the
            // repository it landed in.
            *commits_by_repository.entry(repository).or_insert(0) += 1;
            continue;
        }

        active_days.insert(local_day);
        if repository.is_none() {
            coverage.observations_without_repository += 1;
        }

        if row.event_type == PROMPT_EVENT_TYPE && !prompt_origin_denies_anchor(row) {
            anchors.push((secs, repository.clone()));
            anchors_by_day.entry(local_day).or_default().push(secs);
            *prompts_by_repository.entry(repository.clone()).or_insert(0) += 1;
            if repository.is_none() {
                coverage.prompts_without_repository += 1;
            }
            if let Some(session) = session_ref(row.subject.as_deref()) {
                turns_by_session
                    .entry(session.to_string())
                    .or_default()
                    .push((secs, Turn::Prompted(repository)));
            }
        } else {
            // A prompt denied an anchor is not a prompt of *yours* -- see
            // `prompt_origin_denies_anchor` -- but it is not dropped either: a
            // subagent being dispatched is agent activity, and deleting it from the
            // timeline would understate agent runtime while pretending nothing
            // happened then. It falls through to the same arm every non-prompt
            // observation already takes, joining `agent_instants` below, and is
            // counted here rather than let the day's prompt total shrink unexplained.
            if row.event_type == PROMPT_EVENT_TYPE {
                coverage.prompts_not_anchored += 1;
            }
            if row.event_type == RESPONSE_EVENT_TYPE
                && let Some(session) = session_ref(row.subject.as_deref())
            {
                turns_by_session
                    .entry(session.to_string())
                    .or_default()
                    .push((secs, Turn::Completed));
            }
            agent_instants.push(secs);
            agent_by_day.entry(local_day).or_default().push(secs);
            agent_by_repository
                .entry(repository)
                .or_default()
                .push(secs);
        }
    }
    let response_times = measure_response_times(turns_by_session);

    let attention_union_seconds =
        union_seconds(&attention_windows(anchors.iter().map(|(t, _)| *t)));
    let shares: BTreeMap<Option<String>, i64> =
        partition_by_nearest_anchor(&anchors, ATTENTION_BEFORE_SECONDS, ATTENTION_AFTER_SECONDS)
            .into_iter()
            .collect();

    let mut alone: BTreeMap<Option<String>, Vec<i64>> = BTreeMap::new();
    for (instant, repository) in &anchors {
        alone.entry(repository.clone()).or_default().push(*instant);
    }
    let attention_alone: BTreeMap<Option<String>, i64> = alone
        .into_iter()
        .map(|(repository, instants)| {
            (
                repository,
                union_seconds(&attention_windows(instants.into_iter())),
            )
        })
        .collect();

    // Decision 2: one timeline for every repository, clustered once.
    let agent_union_seconds = union_seconds(&cluster(&agent_instants, AGENT_GAP_SECONDS));
    let agent_alone: BTreeMap<Option<String>, i64> = agent_by_repository
        .into_iter()
        .map(|(repository, instants)| {
            (
                repository,
                union_seconds(&cluster(&instants, AGENT_GAP_SECONDS)),
            )
        })
        .collect();

    // Every repository that appeared at all gets a row, including one with agent
    // activity and no prompts (0 attention, which is not the same as no coverage).
    // Commits earn a row of their own: a repository whose work this period landed as
    // commits, but whose sessions fell outside it or were never observed, would
    // otherwise be missing entirely -- which reads as a repository nothing happened in.
    let keys: BTreeSet<Option<String>> = shares
        .keys()
        .chain(attention_alone.keys())
        .chain(agent_alone.keys())
        .chain(prompts_by_repository.keys())
        .chain(commits_by_repository.keys())
        .cloned()
        .collect();

    let mut repositories = Vec::new();
    let mut unattributed = Unattributed {
        observations: coverage.observations_without_repository,
        ..Default::default()
    };
    for key in keys {
        let attention_seconds = shares.get(&key).copied().unwrap_or(0);
        let attention_alone_seconds = attention_alone.get(&key).copied().unwrap_or(0);
        let agent_seconds = agent_alone.get(&key).copied().unwrap_or(0);
        let prompts = prompts_by_repository.get(&key).copied().unwrap_or(0);
        let commits = commits_by_repository.get(&key).copied().unwrap_or(0);
        match key {
            Some(repository) => repositories.push(RepositoryRow {
                workstream: rules.workstream_for(&repository).map(str::to_string),
                repository,
                attention_seconds,
                attention_alone_seconds,
                agent_seconds,
                prompts,
                commits,
            }),
            None => {
                unattributed.attention_seconds = attention_seconds;
                unattributed.agent_seconds = agent_seconds;
                unattributed.prompts = prompts;
                unattributed.commits = commits;
            }
        }
    }
    repositories.sort_by(|a, b| {
        b.attention_seconds
            .cmp(&a.attention_seconds)
            .then_with(|| b.agent_seconds.cmp(&a.agent_seconds))
            .then_with(|| a.repository.cmp(&b.repository))
    });

    // Both naive sums cover the same activity the unions do, unattributed records
    // included (their key is simply `None`) -- a naive sum over a smaller set than
    // its union would not be a comparison of two ways to count, just of two sets.
    let attention_naive_sum_seconds = attention_alone.values().sum::<i64>();
    let agent_naive_sum_seconds = agent_alone.values().sum::<i64>();

    // The third reading, and the one this command exists to make impossible to reach
    // by accident: each day unioned and clustered on its own, then added up -- which
    // is what running `report --day` across the period and totalling the output gives.
    // Computed here rather than left implicit precisely because it disagrees with the
    // period's own union whenever work runs through local midnight, and a number a
    // person can arrive at two ways must show both (design §13).
    let attention_daily_sum_seconds: i64 = anchors_by_day
        .values()
        .map(|instants| union_seconds(&attention_windows(instants.iter().copied())))
        .sum();
    let agent_daily_sum_seconds: i64 = agent_by_day
        .values()
        .map(|instants| union_seconds(&cluster(instants, AGENT_GAP_SECONDS)))
        .sum();

    coverage.sources = ledger.observed_range_by_source(&[GAP_EVENT_TYPE])?;
    coverage.hook_capture = ledger.capture_channel_range(HOOK_CHANNEL, &[GAP_EVENT_TYPE])?;
    let within_observed_range = coverage.sources.iter().any(|range| {
        match (
            rfc3339::epoch_seconds(&range.earliest),
            rfc3339::epoch_seconds(&range.latest),
        ) {
            (Some(earliest), Some(latest)) => earliest < end && latest >= start,
            _ => false,
        }
    });

    Ok(PeriodReport {
        period: period.label(),
        range: period.range_label(),
        single_day: period.is_single_day(),
        calendar_days: period.calendar_days(),
        active_days: active_days.len() as i64,
        tz_offset,
        window_start_utc: crate::clock::format_utc_seconds(start),
        window_end_utc: crate::clock::format_utc_seconds(end),
        config,
        attention_union_seconds,
        attention_naive_sum_seconds,
        attention_daily_sum_seconds,
        agent_union_seconds,
        agent_naive_sum_seconds,
        agent_daily_sum_seconds,
        prompts: prompts_by_repository.values().sum(),
        commits: commits_by_repository.values().sum(),
        response_times,
        workstreams: group_by_workstream(&repositories),
        repositories,
        unattributed,
        within_observed_range,
        coverage,
    })
}

/// Read `<root>/config/workstreams.toml`, or report that it is not there.
pub(crate) fn load_rules(root: &Path) -> Result<(ConfigStatus, Rules), ReportError> {
    let path = root.join("config").join("workstreams.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let rules = Rules::from_toml(&text).map_err(|source| ReportError::Config {
                path: path.clone(),
                source,
            })?;
            Ok((ConfigStatus::Loaded { path }, rules))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Not an error: everything is then unassigned, and the output says the
            // config is absent rather than implying nothing is assignable.
            let empty = Rules::from_toml("").expect("an empty config parses");
            Ok((ConfigStatus::Absent { path }, empty))
        }
        Err(source) => Err(ReportError::ConfigUnreadable { path, source }),
    }
}

/// The `[-ATTENTION_BEFORE, +ATTENTION_AFTER)` window around each prompt.
pub(crate) fn attention_windows(instants: impl Iterator<Item = i64>) -> Vec<Span> {
    instants
        .map(|t| Span {
            start: t - ATTENTION_BEFORE_SECONDS,
            end: t + ATTENTION_AFTER_SECONDS,
        })
        .collect()
}

/// A lexicographic prefix bound for [`cclogger_archive::Ledger::observations_between`]:
/// second precision, no `Z`. See the call site for why the `Z` must not be there.
pub(crate) fn prefix_bound(secs: i64) -> String {
    crate::clock::format_utc_seconds(secs)
        .trim_end_matches('Z')
        .to_string()
}

/// What landed, printed under the two clocks and belonging to neither.
///
/// Placed after them deliberately: a commit is the answer to "what came of this", which
/// is a different question from "how long did it take", and the two numbers must not
/// read as parts of one total. The caveat line says so rather than leaving the layout to
/// imply it.
///
/// Zero is printed rather than suppressed, and it is stated as the ambiguous number it
/// is: nothing landed, *or* nothing could be collected. The two are told apart by
/// `cclogger import`, which reports every repository it could not read, and this says
/// where to look rather than picking one of the readings.
fn write_commits(out: &mut String, report: &PeriodReport, noun: &str) {
    let with_commits: Vec<&RepositoryRow> = report
        .repositories
        .iter()
        .filter(|row| row.commits > 0)
        .collect();
    let _ = writeln!(
        out,
        "commits       {:>8}  landed in this {noun} -- evidence, not time: a commit is an instant, so",
        report.commits
    );
    let _ = writeln!(
        out,
        "                        it is in neither total above and marks no day as worked"
    );
    if report.commits == 0 {
        let _ = writeln!(
            out,
            "                        nothing landed, or nothing could be collected -- `cclogger import`"
        );
        let _ = writeln!(
            out,
            "                        names the repositories it could not read"
        );
    }
    for row in with_commits {
        let _ = writeln!(
            out,
            "                {:>6}  {}",
            row.commits, row.repository
        );
    }
    if report.unattributed.commits > 0 {
        let _ = writeln!(
            out,
            "                {:>6}  (no repository)",
            report.unattributed.commits
        );
    }
    let _ = writeln!(out);
}

/// Gather repository rows into their workstreams, `unassigned` last.
fn group_by_workstream(repositories: &[RepositoryRow]) -> Vec<WorkstreamRow> {
    let mut by_workstream: BTreeMap<Option<String>, WorkstreamRow> = BTreeMap::new();
    for row in repositories {
        let entry = by_workstream
            .entry(row.workstream.clone())
            .or_insert_with(|| WorkstreamRow {
                workstream: row.workstream.clone(),
                attention_seconds: 0,
                agent_seconds: 0,
                prompts: 0,
                repositories: Vec::new(),
            });
        entry.attention_seconds += row.attention_seconds;
        entry.agent_seconds += row.agent_seconds;
        entry.prompts += row.prompts;
        entry.repositories.push(row.repository.clone());
    }
    let mut rows: Vec<WorkstreamRow> = by_workstream.into_values().collect();
    // `unassigned` last, then by name: a stable order that does not move because one
    // day's totals came out differently.
    rows.sort_by(|a, b| match (&a.workstream, &b.workstream) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.cmp(y),
    });
    rows
}

/// Whole minutes, rounded to the nearest, as `2h39m` / `53m` / `0m`.
///
/// Minutes, not seconds: the numbers are estimates produced by a windowing parameter,
/// and a seconds figure would imply a precision the method does not have.
pub(crate) fn hm(seconds: i64) -> String {
    let minutes = (seconds + 30).div_euclid(60);
    let (h, m) = (minutes / 60, minutes % 60);
    if h == 0 {
        format!("{m}m")
    } else {
        format!("{h}h{m:02}m")
    }
}

/// A measured gap, as `1h02m03s` / `5m31s` / `8s`.
///
/// Seconds, unlike [`hm`], and deliberately: an attention figure is what a windowing
/// parameter produced and a seconds place would claim a precision the method does not
/// have, but a reaction time is the difference between two recorded timestamps. On a
/// month of real data almost a tenth of reactions land under ten seconds, and rounding
/// those to `0m` would erase the fastest thing being measured.
pub(crate) fn hms(seconds: i64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h != 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m != 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// `part` as a percentage of `whole`, or `n/a` when there is no whole to be part of
/// -- never `0.0%`, which would claim a share of something that does not exist.
pub(crate) fn share(part: i64, whole: i64) -> String {
    if whole == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", (part as f64) * 100.0 / (whole as f64))
    }
}

fn workstream_label(workstream: &Option<String>) -> &str {
    workstream.as_deref().unwrap_or(UNASSIGNED)
}

/// The two per-day averages of `total`, each printed next to the denominator it used.
///
/// Both, always. A month worked on 21 of its 31 days answers "how much per day?" two
/// ways that differ by half again, and either one alone reads as *the* answer: the
/// calendar average understates a month with a fortnight off in it, and the active-day
/// average understates nothing but silently redefines "day". Naming the divisor on the
/// line is what keeps the reader from having to assume which was meant.
///
/// With no active day there is no average to print. `n/a` rather than `0m`, for the
/// same reason [`share`] refuses `0.0%`: a period nothing was collected for is a
/// coverage statement, and `0m` would make it a claim about how much work was done.
fn write_averages(out: &mut String, report: &PeriodReport, total: i64) {
    if report.active_days == 0 {
        let _ = writeln!(
            out,
            "  per calendar day  {:>8}  no day in this period holds an observation of activity:",
            "n/a"
        );
        let _ = writeln!(
            out,
            "  per active day    {:>8}  that is no coverage to average, not a period holding 0m",
            "n/a"
        );
        return;
    }
    let _ = writeln!(
        out,
        "  per calendar day  {:>8}  over {}",
        hm(total.div_euclid(report.calendar_days)),
        days_label(report.calendar_days)
    );
    let _ = writeln!(
        out,
        "  per active day    {:>8}  over {} with any observation",
        hm(total.div_euclid(report.active_days)),
        days_label(report.active_days)
    );
}

/// A distribution as the report states it: how many gaps, and the quartiles they had
/// -- or, under [`MIN_GAPS_FOR_QUARTILES`], the statement that there is no shape to
/// give, which is why `count` still prints on that line.
///
/// No sum and no mean appear here because [`GapDistribution`] holds neither. That is
/// deliberate: a render is the wrong place to keep a number out of, since the next
/// person to add a line has to know not to.
fn distribution_label(distribution: &GapDistribution) -> String {
    match distribution.quartiles {
        Some(quartiles) => format!(
            "n {:>5}   p25 {:>8}   median {:>8}   p75 {:>8}",
            distribution.count,
            hms(quartiles.p25),
            hms(quartiles.median),
            hms(quartiles.p75)
        ),
        None => format!(
            "n {:>5}   under {MIN_GAPS_FOR_QUARTILES} gaps there is no shape to report, only the \
             gaps themselves",
            distribution.count
        ),
    }
}

/// The report as the CLI prints it.
///
/// The basis line matters as much as the numbers: this is AI-observed attention, it
/// is estimated, and it is a lower bound on presence that says nothing about work
/// done away from the machine. Nothing here should read as a timesheet.
pub fn render(report: &PeriodReport) -> String {
    let mut out = String::new();
    // `range` is only ever `Some` for a single day `--from`/`--to` narrowed (see
    // `PeriodReport::range`), and it prints exactly where `log`'s own window line
    // prints it: a narrowed report must never look like a quieter whole one.
    let range = match &report.range {
        Some(range) => format!("  {range}"),
        None => String::new(),
    };
    // The offset is printed on every run, and it is always the one the window below was
    // actually cut with -- detected from this machine or named on the flag, never a
    // fallback stood in for either. There is nothing here for a reader to have to know.
    let _ = writeln!(
        out,
        "{}{range}  UTC{}  ({} .. {})",
        report.period, report.tz_offset, report.window_start_utc, report.window_end_utc
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "basis         AI-observed attention (estimated; not total work time). Work done away"
    );
    let _ = writeln!(
        out,
        "              from this machine, or without an AI session, leaves no trace in it."
    );
    let _ = writeln!(
        out,
        "parameters    attention window [-{}m, +{}m] around each human prompt; agent gap threshold {}m;",
        ATTENTION_BEFORE_SECONDS / 60,
        ATTENTION_AFTER_SECONDS / 60,
        AGENT_GAP_SECONDS / 60
    );
    let _ = writeln!(
        out,
        "              reaction quartiles stated from {MIN_GAPS_FOR_QUARTILES} measured gaps up"
    );
    // The two definitions a period needs and a day does not: what the averages below
    // divide by, and why the period's totals are not its days' totals added up.
    // Stated here rather than left to the reader, because both are choices, and both
    // give a different number from the one a reasonable person might have assumed.
    if !report.single_day {
        let _ = writeln!(
            out,
            "period        {} calendar days, {} of them with any observation. An active day is a day",
            report.calendar_days, report.active_days
        );
        let _ = writeln!(
            out,
            "              the ledger holds an observation of activity for -- never one whose attention"
        );
        let _ = writeln!(
            out,
            "              passed some threshold, which would be a guess. A gap marker is dated to when"
        );
        let _ = writeln!(
            out,
            "              `cclogger archive` ran rather than to any activity, so a day holding only"
        );
        let _ = writeln!(
            out,
            "              those is not an active one, and neither is one holding only records a"
        );
        let _ = writeln!(
            out,
            "              fork copied out of another transcript. Every union and cluster below is"
        );
        let _ = writeln!(
            out,
            "              computed over the period's own observations, never by adding its days up."
        );
    }
    match &report.config {
        ConfigStatus::Loaded { path } => {
            let _ = writeln!(out, "config        {}", path.display());
        }
        ConfigStatus::Absent { path } => {
            let _ = writeln!(
                out,
                "config        no workstreams.toml at {} -- every repository is unassigned until",
                path.display()
            );
            let _ = writeln!(
                out,
                "              one exists; this is a missing config, not an unassignable day."
            );
        }
    }
    let noun = if report.single_day { "day" } else { "period" };
    if !report.within_observed_range {
        let _ = writeln!(
            out,
            "note          this {noun} is outside the observed range of every source: nothing was"
        );
        let _ = writeln!(
            out,
            "              collected for it, which is not the same as a {noun} with no work."
        );
    }
    let _ = writeln!(out);

    // -- workstreams, with their repositories underneath ------------------------
    let mut labels: Vec<String> = Vec::new();
    for row in &report.workstreams {
        labels.push(workstream_label(&row.workstream).to_string());
        for repository in &row.repositories {
            labels.push(format!("  {repository}"));
        }
    }
    let width = labels
        .iter()
        .map(|l| l.chars().count())
        .chain(std::iter::once("workstream".len()))
        .max()
        .unwrap_or_default();

    let _ = writeln!(
        out,
        "{:width$}  {:>16}  {:>7}  {:>7}",
        "workstream", "attention (est.)", "prompts", "share"
    );
    if report.workstreams.is_empty() {
        let _ = writeln!(
            out,
            "{:width$}  {:>16}  {:>7}  {:>7}",
            format!(
                "(no repository had an observation {})",
                if report.single_day {
                    "on this day"
                } else {
                    "in this period"
                }
            ),
            "-",
            "-",
            "-"
        );
    }
    for row in &report.workstreams {
        let _ = writeln!(
            out,
            "{:width$}  {:>16}  {:>7}  {:>7}",
            workstream_label(&row.workstream),
            hm(row.attention_seconds),
            row.prompts,
            share(row.attention_seconds, report.attention_union_seconds),
        );
        for repository in &row.repositories {
            let member = report
                .repositories
                .iter()
                .find(|r| &r.repository == repository);
            let (attention, prompts) = match member {
                Some(r) => (hm(r.attention_seconds), r.prompts.to_string()),
                None => ("-".to_string(), "-".to_string()),
            };
            let _ = writeln!(
                out,
                "{:width$}  {:>16}  {:>7}",
                format!("  {repository}"),
                attention,
                prompts
            );
        }
    }
    let _ = writeln!(out);

    // -- the attention numbers, each labelled for what it is --------------------
    let _ = writeln!(
        out,
        "attention     {:>8}  {noun}-wide union, overlaps counted once -- the {noun}'s total",
        hm(report.attention_union_seconds)
    );
    let _ = writeln!(
        out,
        "              {:>8}  per-repository naive sum -- counts overlapping windows twice; not a total",
        hm(report.attention_naive_sum_seconds)
    );
    if !report.single_day {
        let _ = writeln!(
            out,
            "              {:>8}  the daily unions added up -- never smaller, and not a total either:",
            hm(report.attention_daily_sum_seconds)
        );
        let _ = writeln!(
            out,
            "                        a window opened before local midnight is counted whole in that"
        );
        let _ = writeln!(
            out,
            "                        day and again inside the next day's own windows"
        );
        write_averages(&mut out, report, report.attention_union_seconds);
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "agent runtime {:>8}  {noun}-wide union of the cross-session timeline ({}m gap threshold)",
        hm(report.agent_union_seconds),
        AGENT_GAP_SECONDS / 60
    );
    let _ = writeln!(
        out,
        "              {:>8}  per-repository naive sum -- may double-count concurrent work, and",
        hm(report.agent_naive_sum_seconds)
    );
    let _ = writeln!(
        out,
        "                        may split a stretch the cross-session timeline keeps whole"
    );
    if !report.single_day {
        let _ = writeln!(
            out,
            "              {:>8}  the daily clusters added up -- never larger: a stretch running",
            hm(report.agent_daily_sum_seconds)
        );
        let _ = writeln!(
            out,
            "                        through local midnight is one cluster over the period and two"
        );
        let _ = writeln!(
            out,
            "                        over the two days, which drops the silence between them"
        );
    }
    // Every repository, including the ones at 0m: a row missing from this list would
    // be indistinguishable from a repository whose agent time really was nothing.
    for row in &report.repositories {
        let _ = writeln!(
            out,
            "                {:>6}  {}",
            hm(row.agent_seconds),
            row.repository
        );
    }
    if !report.single_day {
        write_averages(&mut out, report, report.agent_union_seconds);
    }
    let _ = writeln!(out);

    write_commits(&mut out, report, noun);

    // -- reaction time: the person's own gap, generation time taken out ----------
    let times = &report.response_times;
    let _ = writeln!(out, "response time {}", distribution_label(&times.response));
    let _ = writeln!(
        out,
        "              an assistant's completed output -> the human prompt that consumed it: the"
    );
    let _ = writeln!(
        out,
        "              person's own reaction, with the model's generation time excluded. Each"
    );
    let _ = writeln!(
        out,
        "              completion answers at most one prompt and is spent doing it, so a prompt"
    );
    let _ = writeln!(
        out,
        "              that finds none unconsumed measures no reaction of its own."
    );
    if times.prompts_walked == 0 {
        let _ = writeln!(
            out,
            "              no prompt in this {noun} could be paired at all -- there was nothing here"
        );
        let _ = writeln!(
            out,
            "              to react to, which is not the same as reacting instantly"
        );
    } else {
        let _ = writeln!(
            out,
            "              {} of {} prompt(s) were back-to-back -- sent with no new output to answer",
            times.back_to_back_prompts, times.prompts_walked
        );
    }
    if times.prompts_walked < report.prompts {
        let _ = writeln!(
            out,
            "              {} prompt(s) named no session, so there was none to pair them inside",
            report.prompts - times.prompts_walked
        );
    }
    let _ = writeln!(
        out,
        "              a shape and never a total: sessions run concurrently, so their gaps added"
    );
    let _ = writeln!(
        out,
        "              up exceed the clock they happened on, and the tail is long enough that a"
    );
    let _ = writeln!(
        out,
        "              mean would describe no reaction anyone actually had"
    );
    let _ = writeln!(out, "interval      {}", distribution_label(&times.interval));
    let _ = writeln!(
        out,
        "              consecutive human prompts in one session, for reference -- the same wait"
    );
    let _ = writeln!(
        out,
        "              with the generation time left in; the difference between the two lines is"
    );
    let _ = writeln!(out, "              roughly what the model spent.");
    if !times.by_repository.is_empty() {
        let _ = writeln!(
            out,
            "              by repository, median reaction; n/a where there are too few gaps for one:"
        );
    }
    for row in &times.by_repository {
        let _ = writeln!(
            out,
            "              {:>8}  n {:>5}  {}",
            match row.response.quartiles {
                Some(quartiles) => hms(quartiles.median),
                None => "n/a".to_string(),
            },
            row.response.count,
            match &row.repository {
                Some(repository) => repository.as_str(),
                // Not `unassigned`, which is a missing rule: these prompts carried no
                // repository identity to assign at all.
                None => "(no repository on the prompt)",
            }
        );
    }
    let _ = writeln!(out);

    // -- coverage, in the three registers design §13 keeps separate -------------
    let _ = writeln!(out, "coverage");
    if report.coverage.sources.is_empty() {
        let _ = writeln!(
            out,
            "  source range         no observations in this ledger at all"
        );
    }
    for source in &report.coverage.sources {
        let _ = writeln!(
            out,
            "  source range         {} {} .. {}",
            source.source_kind, source.earliest, source.latest
        );
    }
    // Printed under its own label, not folded into the vendor's range above: a hook
    // observation is a Claude Code observation, so it sits inside that range and the
    // moment capture began would otherwise be invisible.
    if let Some(hook) = &report.coverage.hook_capture {
        let _ = writeln!(
            out,
            "  hook capture         from {} (first hook event ever recorded here)",
            hook.earliest
        );
        let _ = writeln!(
            out,
            "                       days before that were seen only through transcripts"
        );
    }
    let _ = writeln!(
        out,
        "  capture / parse      {} observation(s) in window; {} gap marker(s), excluded from every clock",
        report.coverage.observations_in_window, report.coverage.gap_markers
    );
    if report.coverage.observations_with_inherited_time > 0 {
        let _ = writeln!(
            out,
            "                       {} observation(s) were copied into a forked transcript and carry the",
            report.coverage.observations_with_inherited_time
        );
        let _ = writeln!(
            out,
            "                       copy's write time, not their own -- excluded from every clock"
        );
    }
    if report.coverage.observations_without_usable_time > 0 {
        let _ = writeln!(
            out,
            "                       {} observation(s) carried an unreadable timestamp and reached no clock",
            report.coverage.observations_without_usable_time
        );
    }
    let _ = writeln!(
        out,
        "  allocation           {} of {} prompt(s) resolved to a repository; {} observation(s) carried none",
        report.prompts - report.coverage.prompts_without_repository,
        report.prompts,
        report.coverage.observations_without_repository
    );
    if report.coverage.prompts_not_anchored > 0 {
        let _ = writeln!(
            out,
            "                       {} more prompt(s) came from a subagent's own dispatch, not a human --",
            report.coverage.prompts_not_anchored
        );
        let _ = writeln!(
            out,
            "                       real agent activity, kept on that clock, but not counted as a prompt above"
        );
    }
    if report.unattributed.attention_seconds > 0 || report.unattributed.agent_seconds > 0 {
        let _ = writeln!(
            out,
            "                       {} attention and {} agent time sit on records with no repository --",
            hm(report.unattributed.attention_seconds),
            hm(report.unattributed.agent_seconds)
        );
        let _ = writeln!(
            out,
            "                       not \"unassigned\" (a missing rule), but no identity to assign at all"
        );
    }
    out
}

// `pub(crate)` for the same reason `import.rs`'s test module is: `log.rs`'s tests
// build their synthetic ledgers from these helpers rather than growing a second copy
// that could drift out of step. Two day views whose fixtures disagreed about what a
// day looks like would not be comparable, which is most of the point of having both.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::git::tests::{TempHome, commit_at, init_repository};
    use crate::import::run_import;
    use crate::import::tests::{
        TempRoot, codex_deferred_first_flush, codex_forked_rollout,
        codex_parented_deferred_first_flush, codex_subagent_dispatch, codex_subagent_rollout,
    };
    use serde_json::json;
    use std::path::Path;

    /// The offset every fixture is written and read against.
    ///
    /// A fixture's offset is part of the fixture, not a fact about the machine running
    /// it: the timestamps below are written relative to local midnight in *this* offset,
    /// so both ends have to agree on one, and it must be the same one on every host. The
    /// value is arbitrary -- `+09:00` is what these fixtures were written in -- and
    /// deliberately unrelated to whatever offset the CLI detects at run time.
    pub(crate) const TZ: TzOffset = TzOffset::from_hours(9);

    /// When `cclogger archive` is pretended to have run. Later than every record in the
    /// fixtures that use it, as a real acquisition is.
    pub(crate) const ACQUIRED_AT: &str = "2026-07-27T00:00:00Z";

    pub(crate) fn day() -> Day {
        Day::parse("2026-07-26").expect("a well-formed day")
    }

    /// The fixture day, whole -- what a command reads when nothing narrowed it.
    pub(crate) fn window() -> DayWindow {
        DayWindow::new(day(), None, None).expect("the whole of a day is a stretch of it")
    }

    /// `HH:MM` in the fixture offset, for the tests that narrow a window.
    pub(crate) fn time(hhmm: &str) -> TimeOfDay {
        TimeOfDay::parse(hhmm).unwrap_or_else(|e| panic!("{hhmm:?} is a time of day: {e}"))
    }

    pub(crate) fn day_start() -> i64 {
        day().utc_window(TZ).0
    }

    /// An RFC 3339 UTC timestamp `offset_seconds` after the fixture day's local
    /// midnight, with the millisecond precision real Claude Code transcripts carry --
    /// which is load-bearing for the day-boundary test, where `.` sorts before `Z`.
    pub(crate) fn at(offset_seconds: i64) -> String {
        let whole = crate::clock::format_utc_seconds(day_start() + offset_seconds);
        format!("{}.000Z", whole.trim_end_matches('Z'))
    }

    /// A cwd under the running machine's `$HOME`, built at run time rather than
    /// written down: the importer resolves repository identity against the home it is
    /// running under, and nothing derived from a real machine may be committed. The
    /// repository names themselves are synthetic.
    pub(crate) fn cwd_for(repository: &str) -> String {
        format!(
            "{}/ghq/{repository}",
            std::env::var("HOME").unwrap_or_default()
        )
    }

    pub(crate) fn prompt_line(session: &str, uuid: &str, time: &str, cwd: &str) -> String {
        json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": session,
            "timestamp": time,
            "cwd": cwd,
            "isSidechain": false,
            "isMeta": false,
            "message": { "role": "user", "content": "SYNTHETIC prompt" },
        })
        .to_string()
    }

    pub(crate) fn assistant_line(session: &str, uuid: &str, time: &str, cwd: &str) -> String {
        json!({
            "type": "assistant",
            "uuid": uuid,
            "sessionId": session,
            "timestamp": time,
            "cwd": cwd,
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "SYNTHETIC response" }],
            },
        })
        .to_string()
    }

    pub(crate) fn archive(root: &Path, locator: &str, lines: &[String], acquired_at: &str) {
        archive_from("claude-code", root, locator, lines, acquired_at);
    }

    /// [`archive`] with the vendor named, for the fixtures that need a ledger holding
    /// more than one. The wire name is passed in rather than derived, so a test can
    /// still catch the code under test renaming a vendor.
    pub(crate) fn archive_from(
        source_kind: &str,
        root: &Path,
        locator: &str,
        lines: &[String],
        acquired_at: &str,
    ) {
        let mut bytes = lines.join("\n");
        bytes.push('\n');
        let mut ledger = cclogger_archive::Ledger::open(root).expect("open ledger");
        ledger
            .archive_file(source_kind, locator, bytes.as_bytes(), acquired_at, None)
            .expect("archive the synthetic transcript");
    }

    /// One synthetic transcript file: a session that happened in one repository, with
    /// human prompts at some offsets from local midnight and agent records at others.
    pub(crate) struct Session<'a> {
        pub(crate) repository: &'a str,
        pub(crate) prompts: &'a [i64],
        pub(crate) agent: &'a [i64],
    }

    /// Archive one transcript per session and import them, leaving `root` holding a
    /// ledger of exactly these observations.
    pub(crate) fn build(root: &Path, sessions: &[Session]) {
        for (i, session) in sessions.iter().enumerate() {
            let id = format!("{:08}-1111-4111-8111-111111111111", i + 1);
            let cwd = cwd_for(session.repository);
            let mut lines = Vec::new();
            for (n, offset) in session.prompts.iter().enumerate() {
                lines.push(prompt_line(&id, &format!("p-{i}-{n}"), &at(*offset), &cwd));
            }
            for (n, offset) in session.agent.iter().enumerate() {
                lines.push(assistant_line(
                    &id,
                    &format!("a-{i}-{n}"),
                    &at(*offset),
                    &cwd,
                ));
            }
            archive(
                root,
                &format!(".claude/projects/synthetic/{id}.jsonl"),
                &lines,
                ACQUIRED_AT,
            );
        }
        run_import(root, false).expect("import the synthetic ledger");
    }

    pub(crate) fn write_config(root: &Path, toml: &str) {
        std::fs::create_dir_all(root.join("config")).expect("create the config directory");
        std::fs::write(root.join("config/workstreams.toml"), toml).expect("write the config");
    }

    fn repository_row<'a>(report: &'a PeriodReport, repository: &str) -> &'a RepositoryRow {
        report
            .repositories
            .iter()
            .find(|row| row.repository == repository)
            .unwrap_or_else(|| {
                panic!(
                    "{repository} is missing from the report; it holds {:?}",
                    report
                        .repositories
                        .iter()
                        .map(|r| r.repository.as_str())
                        .collect::<Vec<_>>()
                )
            })
    }

    fn workstream_row<'a>(report: &'a PeriodReport, workstream: Option<&str>) -> &'a WorkstreamRow {
        report
            .workstreams
            .iter()
            .find(|row| row.workstream.as_deref() == workstream)
            .unwrap_or_else(|| {
                panic!(
                    "workstream {workstream:?} is missing from the report; it holds {:?}",
                    report
                        .workstreams
                        .iter()
                        .map(|r| r.workstream.clone())
                        .collect::<Vec<_>>()
                )
            })
    }

    // -- commits: evidence beside the clocks, never inside them ---------------------

    /// A synthetic git repository under a throwaway home, with commits at the named
    /// offsets from the fixture day's local midnight, imported into `root`'s ledger.
    ///
    /// Real `git init` repositories, because what is being imported is what `git log`
    /// actually prints. Nothing of the person running the tests is read: the repository
    /// lives in the temporary home this returns.
    pub(crate) fn commits_in(root: &Path, repository: &str, offsets: &[i64]) -> TempHome {
        use crate::import::tests::{GIT_AUTHOR, import_commits};

        let home = TempHome::new("commits");
        let repo = init_repository(home.path(), repository, GIT_AUTHOR);
        for (n, offset) in offsets.iter().enumerate() {
            commit_at(
                &repo,
                GIT_AUTHOR,
                &format!("f{n}.txt"),
                n + 1,
                "SYNTHETIC commit",
                &crate::clock::format_utc_seconds(day_start() + offset),
            );
        }
        import_commits(root, home.path(), &[repository], false);
        home
    }

    #[test]
    fn commits_are_counted_and_change_neither_clock() {
        // The rule the whole feature turns on: a commit is an instant, so it must not
        // add a second to either duration. Asserted as a before/after over the same
        // ledger, so it cannot pass by the commits merely being small.
        let root = TempRoot::new("report-commits-clocks");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600, 7200],
                // Two records inside each cluster: one instant on its own is a
                // zero-width span, and a clock that starts at zero cannot show that a
                // commit failed to move it.
                agent: &[3660, 3720, 7260, 7320],
            }],
        );
        let before = run_report(root.path(), Period::of_day(day()), TZ).expect("report");
        assert!(before.attention_union_seconds > 0 && before.agent_union_seconds > 0);
        assert_eq!(before.commits, 0);

        // Placed where a clock would notice. 3900 is 180s past the last record of the
        // first cluster and so *inside* the 5m gap threshold: admitted to the agent
        // timeline it would stretch that cluster from 60s to 240s. 50000 sits in an
        // empty hour, where it would mark a day and a repository as having had activity
        // in it. A commit landing between two records of an existing cluster would move
        // nothing and prove nothing.
        let _home = commits_in(root.path(), "github.com/acme/api", &[3900, 50_000]);
        let after = run_report(root.path(), Period::of_day(day()), TZ).expect("report");

        assert_eq!(after.commits, 2, "the commits are counted");
        assert_eq!(
            after.attention_union_seconds, before.attention_union_seconds,
            "a commit must not open an attention window"
        );
        assert_eq!(
            after.agent_union_seconds, before.agent_union_seconds,
            "a commit must not extend an agent cluster"
        );
        assert_eq!(
            after.agent_naive_sum_seconds, before.agent_naive_sum_seconds,
            "nor a per-repository one"
        );
        assert_eq!(after.prompts, before.prompts);
        assert_eq!(
            repository_row(&after, "github.com/acme/api").commits,
            2,
            "and they are attributed to the repository they landed in"
        );
        assert_eq!(
            after.coverage.observations_in_window,
            before.coverage.observations_in_window + 2,
            "they are observations in the window, and counted as such"
        );
    }

    #[test]
    fn a_day_whose_only_evidence_is_a_commit_is_not_an_active_day() {
        // `active_days` divides both daily averages. A day with a commit and no session
        // has no measurable attention to average, so admitting it would lower every
        // average by adding an empty day to the denominator -- while the commit itself
        // is still reported.
        let root = TempRoot::new("report-commit-only-day");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[3660],
            }],
        );
        // Two days after the fixture day, where nothing else happened at all.
        let _home = commits_in(root.path(), "github.com/acme/api", &[2 * 86_400 + 3600]);

        // A week ending two days after the fixture day, so it holds both the session
        // day and the commit-only day.
        let week = Period::week_ending(Day::from_epoch_day(day().epoch_day() + 2));
        let report = run_report(root.path(), week, TZ).expect("report the week");
        assert_eq!(report.commits, 1, "the commit is reported");
        assert_eq!(
            report.active_days, 1,
            "but the day it landed on is not an observed day of work"
        );
    }

    #[test]
    fn a_repository_whose_only_evidence_is_commits_still_gets_a_row() {
        // Work committed from an editor, or on a day whose session was never observed.
        // Without a row it would look like a repository nothing happened in.
        let root = TempRoot::new("report-commit-only-repo");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[3660],
            }],
        );
        let _home = commits_in(root.path(), "github.com/acme/web", &[7200]);

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report");
        let web = repository_row(&report, "github.com/acme/web");
        assert_eq!(web.commits, 1);
        assert_eq!(
            web.attention_seconds, 0,
            "with no attention, which is the truth rather than a reason to hide the row"
        );
        assert_eq!(web.agent_seconds, 0);
        assert!(
            render(&report).contains("github.com/acme/web"),
            "and it is printed"
        );
    }

    #[test]
    fn the_commit_line_states_that_a_zero_may_mean_uncollected() {
        // Design §13: "0" and "the source could not be read" are different statements,
        // and this is the one source whose reading can fail per repository.
        let root = TempRoot::new("report-commit-zero");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[],
            }],
        );
        let rendered = render(&run_report(root.path(), Period::of_day(day()), TZ).expect("report"));
        assert!(
            rendered.contains("nothing landed, or nothing could be collected"),
            "a zero must not read as a measured absence of work: {rendered}"
        );
    }

    #[test]
    fn overlapping_attention_shares_sum_to_the_day_union_rather_than_exceeding_it() {
        // Three repositories active inside the same hour, as on the acceptance day.
        // Their [-1m, +5m] windows overlap, so the sum of per-repository unions
        // (3 x 360s = 1080s) overshoots the day: the union is only [-60, +540) = 600s.
        let root = TempRoot::new("report-overlap");
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[3600],
                    agent: &[],
                },
                Session {
                    repository: "github.com/acme/web",
                    prompts: &[3720],
                    agent: &[],
                },
                Session {
                    repository: "github.com/acme/docs",
                    prompts: &[3840],
                    agent: &[],
                },
            ],
        );

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.attention_union_seconds, 600,
            "the day's attention is the union of the three windows, overlaps counted once"
        );
        let allocated: i64 = report
            .repositories
            .iter()
            .map(|row| row.attention_seconds)
            .sum::<i64>()
            + report.unattributed.attention_seconds;
        assert_eq!(
            allocated, report.attention_union_seconds,
            "every second of the union goes to exactly one repository (design §13)"
        );
        assert_eq!(
            report.attention_naive_sum_seconds, 1080,
            "the per-repository naive sum is reported too, and it is the larger number"
        );
        assert!(
            report.attention_naive_sum_seconds > report.attention_union_seconds,
            "conflating the two is the recurring trap; the report must be able to show both"
        );

        // Nearest anchor, not earliest: the middle of the overlap belongs to whichever
        // prompt is closest to it, so the first repository keeps only the 120s no later
        // window reaches.
        assert_eq!(
            repository_row(&report, "github.com/acme/api").attention_seconds,
            120
        );
        assert_eq!(
            repository_row(&report, "github.com/acme/web").attention_seconds,
            120
        );
        assert_eq!(
            repository_row(&report, "github.com/acme/docs").attention_seconds,
            360
        );
        assert_eq!(report.prompts, 3);
    }

    #[test]
    fn a_repository_with_agent_activity_but_no_prompts_is_listed_with_zero_attention() {
        // A repository the agent worked in while the human never typed: attention 0 and
        // coverage 0 are different statements, and dropping the row conflates them.
        let root = TempRoot::new("report-zero-attention");
        write_config(
            root.path(),
            "[[rule]]\nmatch = \"github.com/acme/*\"\nworkstream = \"acme\"\n",
        );
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[3600],
                    agent: &[3600, 3660],
                },
                Session {
                    repository: "github.com/acme/notes",
                    prompts: &[],
                    agent: &[7200, 7260],
                },
            ],
        );

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        let notes = repository_row(&report, "github.com/acme/notes");
        assert_eq!(
            notes.attention_seconds, 0,
            "no prompt to anchor a window means no attention to allocate"
        );
        assert_eq!(notes.prompts, 0);
        assert_eq!(
            notes.agent_seconds, 60,
            "but its agent activity is real and must still be reported"
        );
        assert!(
            workstream_row(&report, Some("acme"))
                .repositories
                .iter()
                .any(|r| r == "github.com/acme/notes"),
            "the row must survive grouping into a workstream, not be filtered out there"
        );
    }

    #[test]
    fn a_repository_no_rule_matches_is_reported_under_unassigned_not_folded_into_another() {
        let root = TempRoot::new("report-unassigned");
        write_config(
            root.path(),
            "[[rule]]\nmatch = \"github.com/acme/*\"\nworkstream = \"acme\"\n",
        );
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[3600],
                    agent: &[],
                },
                Session {
                    repository: "github.com/other/thing",
                    prompts: &[7200],
                    agent: &[],
                },
            ],
        );

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        let unassigned = workstream_row(&report, None);
        assert_eq!(
            unassigned.repositories,
            vec!["github.com/other/thing".to_string()],
            "a repository no rule covers stands on its own, never inside another bucket"
        );
        assert_eq!(unassigned.attention_seconds, 360);
        assert_eq!(
            workstream_row(&report, Some("acme")).repositories,
            vec!["github.com/acme/api".to_string()],
            "and it must not have been absorbed by the one rule that does match something"
        );
        assert_eq!(
            repository_row(&report, "github.com/other/thing").workstream,
            None
        );
    }

    #[test]
    fn a_missing_config_leaves_every_repository_unassigned_and_says_the_config_is_absent() {
        let root = TempRoot::new("report-no-config");
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[3600],
                    agent: &[],
                },
                Session {
                    repository: "github.com/other/thing",
                    prompts: &[7200],
                    agent: &[],
                },
            ],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ)
            .expect("a missing config is not an error");

        assert!(
            matches!(report.config, ConfigStatus::Absent { .. }),
            "no workstreams.toml is a state to report, not a failure: {:?}",
            report.config
        );
        assert_eq!(
            report.repositories.len(),
            2,
            "both repositories are still reported, with their time intact"
        );
        assert_eq!(
            report.workstreams.len(),
            1,
            "and all of it sits under one bucket while there is no rule to sort it by"
        );
        assert_eq!(report.workstreams[0].workstream, None);
        let rendered = render(&report);
        assert!(
            rendered.contains("no workstreams.toml"),
            "the output must say the config is absent rather than implying nothing is \
             assignable:\n{rendered}"
        );
    }

    #[test]
    fn the_agent_clock_clusters_the_cross_session_timeline_rather_than_each_repository_alone() {
        // Measured on the real ledger: 82.5% of long within-session gaps have another
        // session active inside them -- the person switched work, they did not stop.
        // Repository A's 600s internal gap is filled by repository B, so the global
        // timeline has no gap over the 5-minute threshold and stays one cluster (800s).
        // Clustering each repository alone splits A in two and yields 200 + 100 = 300s.
        let root = TempRoot::new("report-agent-basis");
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[],
                    agent: &[3600, 3700, 4300, 4400],
                },
                Session {
                    repository: "github.com/acme/web",
                    prompts: &[],
                    agent: &[3950, 4050],
                },
            ],
        );

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.agent_union_seconds, 800,
            "the agent clock clusters every repository's records on one timeline"
        );
        assert_eq!(
            report.agent_naive_sum_seconds, 300,
            "the per-repository sum is a different number, and is labelled as one"
        );
        assert_eq!(
            repository_row(&report, "github.com/acme/api").agent_seconds,
            200
        );
        assert_eq!(
            repository_row(&report, "github.com/acme/web").agent_seconds,
            100
        );
    }

    #[test]
    fn gap_markers_are_counted_as_coverage_and_kept_out_of_every_clock() {
        // A gap marker for an unparseable line is dated to when `cclogger archive` ran,
        // not to any activity. Two of them sit between the two repositories' clusters,
        // close enough to bridge them: on the clock they would turn 200s of agent time
        // into 600s, which is why they are excluded from it rather than merely ignored.
        let root = TempRoot::new("report-gaps");
        let api = cwd_for("github.com/acme/api");
        let web = cwd_for("github.com/acme/web");
        let session_a = "0000000a-1111-4111-8111-111111111111";
        let session_b = "0000000b-1111-4111-8111-111111111111";
        archive(
            root.path(),
            ".claude/projects/synthetic/a.jsonl",
            &[
                assistant_line(session_a, "a-0", &at(3600), &api),
                assistant_line(session_a, "a-1", &at(3700), &api),
                "{ not json".to_string(),
            ],
            &at(4000),
        );
        archive(
            root.path(),
            ".claude/projects/synthetic/b.jsonl",
            &[
                assistant_line(session_b, "b-0", &at(4600), &web),
                assistant_line(session_b, "b-1", &at(4700), &web),
                "{ not json".to_string(),
            ],
            &at(4800),
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.coverage.gap_markers, 2,
            "both unparseable lines must be visible as coverage"
        );
        assert_eq!(
            report.agent_union_seconds, 200,
            "and neither may reach the clock: counting them would bridge the two clusters"
        );
        assert_eq!(
            report.coverage.observations_without_repository, 0,
            "a gap never had a repository to lose, so it is not an unattributed observation \
             either -- the four records that do carry one all resolved"
        );
    }

    #[test]
    fn a_forked_codex_rollouts_copied_prompts_are_kept_out_of_every_clock() {
        // A Codex fork re-writes the parent's whole history into the child's file in
        // one call, and every record it writes is stamped with the *copy's* write
        // time. The copied prompt below is a real human turn that happened hours
        // earlier in another file; the timestamp on this copy says only when the fork
        // ran. On the clock it opens a second attention window that no human ever
        // spent, and it counts as a second prompt.
        let root = TempRoot::new("report-codex-inherited");
        let cwd = cwd_for("github.com/acme/api");
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/forked.jsonl",
            &codex_forked_rollout(&cwd, &at(3600), &at(10_800)),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.prompts, 1,
            "only the live turn is a prompt this day held; the copy is the parent's, \
             re-stamped"
        );
        assert_eq!(
            report.attention_union_seconds, 360,
            "one prompt window, not two: the copy's write time must reach no clock"
        );
        assert_eq!(
            report.coverage.observations_with_inherited_time, 1,
            "the copied prompt is counted, not made to disappear -- silence about it is \
             the failure this whole path exists to avoid"
        );
        assert_eq!(
            report.coverage.gap_markers, 1,
            "the embedded parent's own session_meta is a gap first: this file registers \
             no identity for the parent's session, so there is nothing to place it in. A \
             row is reported in one register, and its gap is the stronger claim"
        );
        assert_eq!(
            report.coverage.observations_in_window, 4,
            "and all of them are still observations the day held: excluded from the \
             clocks, not from the ledger"
        );
        assert_eq!(
            report.agent_union_seconds, 0,
            "the fork's own session start is live and on the agent clock, but a single \
             instant spans no seconds -- there is no second record for it to cluster with"
        );

        let rendered = render(&report);
        assert!(
            rendered.contains("copied into a forked transcript"),
            "and the exclusion is stated in the output rather than left to be inferred:\n\
             {rendered}"
        );
    }

    #[test]
    fn a_benign_deferred_first_flush_still_clocks_its_first_prompt() {
        // The half of the pair that a careless implementation destroys. Codex defers
        // creating the rollout file until the first user message, so an ordinary
        // session's `session_meta`, `turn_context` and first prompt share one
        // millisecond -- the same timestamp shape a fork's copy flush has. Excluding
        // those would silently take the opening prompt off the clock in every ordinary
        // Codex session, which is a larger error than the one being fixed.
        let root = TempRoot::new("report-codex-deferred-flush");
        let cwd = cwd_for("github.com/acme/api");
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/ordinary.jsonl",
            &codex_deferred_first_flush(&cwd, &at(3600)),
            ACQUIRED_AT,
        );
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/subagent.jsonl",
            &codex_parented_deferred_first_flush(&cwd, &at(36_000)),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.prompts, 2,
            "both first prompts are real human turns at the instants they claim"
        );
        assert_eq!(
            report.attention_union_seconds, 720,
            "two windows, neither of them suppressed"
        );
        assert_eq!(
            report.coverage.observations_with_inherited_time, 0,
            "nothing here was copied -- not even the one whose session_meta names a parent"
        );
    }

    #[test]
    fn a_day_holding_only_copied_records_is_not_a_day_that_was_worked() {
        // A period counts its active days, and averages over them. A day whose only
        // records were copied out of another transcript holds no evidence that anything
        // happened on it -- exactly as a day holding only gap markers does. Counting it
        // would divide the period's totals by a larger number than the days worked.
        //
        // Built from a *subagent* rollout rather than a fork, because a fork can no
        // longer produce such a day: its own `session_meta` is live and lands in the
        // same instant as the copies. A subagent states its boundary by ordinal, and
        // Codex puts the child's own metadata inside the projected history, so this
        // shape has a day of pure copy and a live turn two days later.
        let root = TempRoot::new("report-codex-inherited-active-days");
        let cwd = cwd_for("github.com/acme/api");
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/subagent.jsonl",
            &codex_subagent_rollout(&cwd, &at(3600), &at(2 * 86_400 + 3600)),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let period = Period::range(day(), Day::parse("2026-07-28").expect("a day"))
            .expect("a three-day range");
        let report = run_report(root.path(), period, TZ).expect("report the synthetic range");

        assert_eq!(report.calendar_days, 3);
        assert_eq!(
            report.active_days, 1,
            "one day was worked; the other two hold nothing and a copy respectively"
        );
        assert_eq!(report.prompts, 1);
        assert_eq!(report.coverage.observations_with_inherited_time, 2);
    }

    #[test]
    fn only_subagent_and_scheduled_origins_are_denied_an_anchor() {
        // The polarity is the whole task, pinned directly against the pure predicate
        // rather than only through a full import: absence must anchor exactly as a
        // stated "human" does, because `Ledger::ingest`'s `ON CONFLICT DO NOTHING`
        // means every observation imported before this field existed keeps `data`
        // with no `origin` key forever, and every Claude Code prompt is among them
        // regardless of age. "scheduled" has no producer yet -- there is nothing to
        // import that would ever create one -- so it is pinned here or nowhere.
        let row = |origin: Option<&str>| ObservationRow {
            source_kind: "codex".to_string(),
            event_type: PROMPT_EVENT_TYPE.to_string(),
            occurred_at: at(0),
            repository_ref: None,
            subject: Some("session/ses_test".to_string()),
            tool_family: None,
            time_basis: None,
            origin: origin.map(str::to_string),
        };
        assert!(
            !prompt_origin_denies_anchor(&row(None)),
            "no reimport can ever add the key to an existing row -- absence must anchor"
        );
        assert!(
            !prompt_origin_denies_anchor(&row(Some("human"))),
            "a stated human origin anchors"
        );
        assert!(
            prompt_origin_denies_anchor(&row(Some("subagent"))),
            "a subagent's own dispatch to itself is not a human's attention"
        );
        assert!(
            prompt_origin_denies_anchor(&row(Some("scheduled"))),
            "nothing produces this yet, but a scheduled run would not be a human's \
             attention either"
        );
    }

    #[test]
    fn a_subagent_prompt_does_not_anchor_but_human_and_legacy_rows_do() {
        // An hour apart, so no two windows can merge and each contributes its full
        // [-1m, +5m] or nothing at all. The legacy row is a Claude Code prompt --
        // never stamped with `origin` at all, by any version of that adapter -- which
        // is exactly what makes it representative of "most of a real ledger" rather
        // than an edge case: `ON CONFLICT(cclogdedupekey) DO NOTHING` means no
        // pre-Task-2 Codex row can ever gain the field either, but nothing needs to
        // fake that shape when Claude Code produces the same one every day.
        let root = TempRoot::new("report-subagent-origin-polarity");
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/legacy.jsonl",
            &[prompt_line(
                "00000099-1111-4111-8111-111111111111",
                "p-legacy",
                &at(3_600),
                &cwd,
            )],
            ACQUIRED_AT,
        );
        let (parent, child) = codex_subagent_dispatch(&cwd, &at(7_200), &at(10_770), &at(10_800));
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/parent.jsonl",
            &parent,
            ACQUIRED_AT,
        );
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/child.jsonl",
            &child,
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.prompts, 2,
            "the subagent's dispatch is not a prompt of yours"
        );
        assert_eq!(
            report.attention_union_seconds,
            2 * 360,
            "the human's and the legacy row's windows both count; the subagent's does not"
        );
        assert_eq!(
            report.coverage.prompts_not_anchored, 1,
            "excluded is said, not silently dropped from the count"
        );

        let rendered = render(&report);
        assert!(
            rendered.contains("subagent's own dispatch"),
            "and the exclusion is stated in the output rather than left to be inferred:\n\
             {rendered}"
        );
    }

    #[test]
    fn a_subagent_prompt_still_joins_the_agent_clock() {
        // Not dropped: `run_report`'s prompt branch routes a denied prompt to the same
        // `else` arm every non-anchoring observation already takes, so it clusters
        // onto the agent clock instead of vanishing from every clock at once. The
        // child's own session start, 30 seconds before its own prompt, is undisputed
        // agent activity on its own; if the prompt joins it, the two cluster into one
        // 30-second span. Routed to `continue` instead of that `else` arm, only the
        // lone session start would remain -- a single instant spans no seconds, so
        // the day's agent clock would read 0 (see `cclogger_domain::clock`'s
        // `a_single_instant_is_a_zero_length_cluster`).
        //
        // The parent's own activity sits far away, at local midnight, so its isolated
        // session start clusters to its own zero-length span and cannot pad this
        // number either way.
        let root = TempRoot::new("report-subagent-agent-clock");
        let cwd = cwd_for("github.com/acme/api");
        let (parent, child) = codex_subagent_dispatch(&cwd, &at(0), &at(7_200), &at(7_230));
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/parent.jsonl",
            &parent,
            ACQUIRED_AT,
        );
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/child.jsonl",
            &child,
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.agent_union_seconds, 30,
            "the subagent's own dispatch joined the session start it followed, not the void"
        );
    }

    #[test]
    fn a_forks_own_session_start_makes_its_day_active_because_the_fork_happened_on_it() {
        // The other side of the test above, and the reason it had to change shape. A
        // fork's own `session_meta` is written at the moment the fork runs, so that day
        // *was* worked -- however little of what the file then contains happened on it.
        // Suppressing it along with the copies would trade one wrong number for another.
        let root = TempRoot::new("report-codex-fork-active-day");
        let cwd = cwd_for("github.com/acme/api");
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/forked.jsonl",
            &codex_forked_rollout(&cwd, &at(3600), &at(2 * 86_400 + 3600)),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let period = Period::range(day(), Day::parse("2026-07-28").expect("a day"))
            .expect("a three-day range");
        let report = run_report(root.path(), period, TZ).expect("report the synthetic range");

        assert_eq!(
            report.active_days, 2,
            "the day the fork ran and the day its own turn happened -- not the middle one"
        );
        assert_eq!(
            report.prompts, 1,
            "but the copied prompt is still no prompt of this period's"
        );
        assert_eq!(report.coverage.observations_with_inherited_time, 1);
    }

    #[test]
    fn the_day_boundary_is_half_open_in_the_configured_offset() {
        // Local midnight belongs to the day; the next local midnight does not. The
        // ledger stores `2026-07-26T15:00:00.000Z` for the second one, and `.` sorts
        // before `Z`, so a bound written as `< …T15:00:00Z` would silently let it in.
        let root = TempRoot::new("report-boundary");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[0, 86_400],
                agent: &[],
            }],
        );

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.prompts, 1,
            "the prompt at the next local midnight belongs to the next day"
        );
        assert_eq!(report.attention_union_seconds, 360, "one window, not two");
    }

    #[test]
    fn a_timestamp_whose_text_order_lies_about_its_instant_is_excluded_by_the_instant() {
        // `occurred_at` is normalized to UTC precisely so a lexicographic range query
        // can be trusted -- except for a value normalization cannot parse, which is
        // stored verbatim, offset and all. A leap second is that case: `:60` is legal
        // RFC 3339 and the normalizer rejects it. The three records below therefore
        // keep their offsets in the column the query sorts on, where their text falls
        // inside the day while the instants they name do not: one before it began, one
        // exactly at the instant it ended (which a half-open day excludes), and one
        // after. Only a comparison of parsed instants can tell.
        let root = TempRoot::new("report-lying-order");
        let session = "0000000d-1111-4111-8111-111111111111";
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/offsets.jsonl",
            &[
                prompt_line(session, "u-0", &at(3600), &cwd),
                // 2026-07-25T11:01:00Z -- four hours before the day began.
                prompt_line(session, "u-1", "2026-07-25T20:00:60.000+09:00", &cwd),
                // 2026-07-26T15:00:00Z exactly -- the first instant of the next day.
                prompt_line(session, "u-2", "2026-07-26T09:59:60.000-05:00", &cwd),
                // 2026-07-26T15:01:00Z -- a minute past it.
                prompt_line(session, "u-3", "2026-07-26T10:00:60.000-05:00", &cwd),
            ],
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.prompts, 1,
            "no neighbouring prompt belongs to this day, whatever its text sorts as"
        );
        assert_eq!(
            report.coverage.observations_in_window, 1,
            "and none of them is counted as observed in it"
        );
        assert_eq!(report.attention_union_seconds, 360);
    }

    #[test]
    fn an_observation_sorting_past_the_day_but_falling_inside_it_is_still_counted() {
        // The mirror of the test above, and the reason the range query is asked for
        // more than the day. This record's text sorts *after* the day's end -- its
        // verbatim `+09:00` offset puts `20:00` in a string the query compares
        // literally -- while the instant it names, 11:01:00Z, is squarely inside the
        // day. Bounded exactly at the day, no query would ever fetch it: an
        // observation of real, in-range activity that no report anywhere would count.
        let root = TempRoot::new("report-sorts-late");
        let session = "0000000e-1111-4111-8111-111111111111";
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/late-sort.jsonl",
            &[
                prompt_line(session, "u-0", &at(3600), &cwd),
                prompt_line(session, "u-1", "2026-07-26T20:00:60.000+09:00", &cwd),
            ],
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.prompts, 2,
            "the record naming an instant inside the day belongs to it, whatever it sorts as"
        );
        assert_eq!(report.coverage.observations_in_window, 2);
        assert_eq!(
            report.attention_union_seconds, 720,
            "two windows, hours apart, neither of them lost"
        );
    }

    #[test]
    fn an_attention_window_reaching_past_midnight_is_not_clipped_at_the_day_boundary() {
        // Deliberate, and what reproduces the acceptance document: a window is built
        // from its anchor and never clamped, so a prompt a minute before midnight
        // carries four of its five minutes into the next day. The consequence is
        // stated rather than quietly fixed -- a week's days do not sum to the week, so
        // a period is unioned over its own anchors instead, and the sum-of-days
        // reading is printed beside it rather than silently replacing it
        // (`attention_daily_sum_seconds`). Clipping here instead would change the
        // number the acceptance document was measured against.
        let root = TempRoot::new("report-end-spill");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[86_340],
                agent: &[],
            }],
        );

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.attention_union_seconds, 360,
            "the whole window, not the 120s of it that fits inside the day"
        );
        assert_eq!(
            repository_row(&report, "github.com/acme/api").attention_seconds,
            360,
            "and the repository's share is the whole window too"
        );
    }

    #[test]
    fn a_day_that_never_happened_is_refused_rather_than_reported_as_a_neighbouring_one() {
        // `days_from_civil` extrapolates linearly, so February 30th is a perfectly
        // good input to it and comes back as March 2nd -- a report headed with a date
        // that never existed, over another day's instants.
        assert!(
            Day::parse("2024-02-29").is_ok(),
            "a leap day in a leap year is a day"
        );
        for absent in [
            "2026-02-29",
            "2026-02-30",
            "2026-04-31",
            "2026-13-01",
            "2026-00-10",
            "2026-01-00",
        ] {
            assert!(
                Day::parse(absent).is_err(),
                "{absent} is not a date on any calendar"
            );
        }
        for malformed in [
            "+026-07-26",
            "2026-+7-26",
            "2026-07-2 ",
            "20a6-07-26",
            "-026-07-26",
        ] {
            assert!(
                Day::parse(malformed).is_err(),
                "{malformed} is not YYYY-MM-DD"
            );
        }
    }

    #[test]
    fn an_offset_no_timezone_uses_is_refused_rather_than_slicing_an_impossible_day() {
        let root = TempRoot::new("report-offset");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[],
            }],
        );

        assert!(
            matches!(
                run_report(
                    root.path(),
                    Period::of_day(day()),
                    TzOffset::from_hours(999)
                ),
                Err(ReportError::InvalidOffset(_))
            ),
            "an offset no zone on earth uses would cut the day somewhere meaningless"
        );
        assert!(
            matches!(
                run_report(
                    root.path(),
                    Period::of_day(day()),
                    TzOffset::from_minutes(14 * 60 + 1)
                ),
                Err(ReportError::InvalidOffset(_))
            ),
            "one minute past the last real offset is past it"
        );
        assert!(
            run_report(root.path(), Period::of_day(day()), TzOffset::from_hours(14)).is_ok(),
            "+14:00 is a real offset (Kiritimati) and must still work"
        );
        assert!(
            run_report(
                root.path(),
                Period::of_day(day()),
                TzOffset::from_hours(-12)
            )
            .is_ok(),
            "and so is -12:00"
        );
        assert!(
            run_report(
                root.path(),
                Period::of_day(day()),
                TzOffset::from_minutes(5 * 60 + 45)
            )
            .is_ok(),
            "and so is +05:45 (Nepal), which whole hours could not name at all"
        );
    }

    #[test]
    fn hm_rounds_to_the_nearest_minute_and_claims_no_finer_precision() {
        assert_eq!(hm(0), "0m");
        assert_eq!(hm(29), "0m", "under half a minute is not a minute");
        assert_eq!(hm(30), "1m", "half of one rounds up");
        assert_eq!(hm(60), "1m");
        assert_eq!(hm(3600), "1h00m");
        // 158.80 minutes: the acceptance document's largest attention share.
        assert_eq!(hm(9528), "2h39m");
    }

    #[test]
    fn a_malformed_config_is_refused_rather_than_reported_as_no_rules() {
        let root = TempRoot::new("report-bad-config");
        write_config(
            root.path(),
            "[[rule]]\nmatchh = \"github.com/acme/*\"\nworkstream = \"acme\"\n",
        );
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[],
            }],
        );

        let result = run_report(root.path(), Period::of_day(day()), TZ);

        assert!(
            matches!(result, Err(ReportError::Config { .. })),
            "a typo'd rule that silently assigns nothing is worse than a refusal"
        );
    }

    #[test]
    fn a_day_outside_the_observed_range_is_said_to_be_outside_it_rather_than_zero() {
        let root = TempRoot::new("report-out-of-range");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[],
            }],
        );

        let earlier = Day::parse("2026-07-20").expect("a well-formed day");
        let report =
            run_report(root.path(), Period::of_day(earlier), TZ).expect("report the earlier day");

        assert!(
            !report.within_observed_range,
            "a day before anything was ever observed is not a day with no work"
        );
        assert_eq!(report.attention_union_seconds, 0);
        let rendered = render(&report);
        assert!(
            rendered.contains("outside the observed range"),
            "and the output has to say so:\n{rendered}"
        );

        // Without this, the test would pass just as well against an empty ledger --
        // where every day is "outside the observed range" for the trivial reason that
        // there is no range at all, and nothing above would notice.
        let observed = run_report(root.path(), Period::of_day(day()), TZ)
            .expect("report the fixture's own day");
        assert!(
            observed.within_observed_range,
            "the same ledger must place its own day inside the range it observed"
        );
        assert!(observed.attention_union_seconds > 0);
    }

    #[test]
    fn an_observation_whose_timestamp_cannot_be_placed_is_counted_rather_than_dropped() {
        // A `time` that normalization could not rewrite is stored verbatim, so it can
        // sort into the day's range while naming no instant this clock can use. It
        // must not silently become part of a clock, and it must not silently vanish
        // either: absence is stated, and here that means counted.
        let root = TempRoot::new("report-unplaceable-time");
        let session = "0000000c-1111-4111-8111-111111111111";
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/unplaceable.jsonl",
            &[
                prompt_line(session, "u-0", &at(3600), &cwd),
                // No zone at all: sorts between the day's bounds, names no instant.
                prompt_line(session, "u-1", "2026-07-26T05:00:00", &cwd),
            ],
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");

        assert_eq!(
            report.coverage.observations_without_usable_time, 1,
            "the unplaceable record is reported, not passed over in silence"
        );
        assert_eq!(
            report.prompts, 1,
            "and it is not counted as a prompt whose window could be measured"
        );
        assert_eq!(
            report.attention_union_seconds, 360,
            "one window, from the one prompt that named an instant"
        );
    }

    #[test]
    fn the_rendered_report_states_its_basis_and_labels_both_attention_numbers() {
        let root = TempRoot::new("report-render");
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[3600],
                    agent: &[3600],
                },
                Session {
                    repository: "github.com/acme/web",
                    prompts: &[3720],
                    agent: &[3720],
                },
            ],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let rendered = render(&report);

        assert!(
            rendered.contains("not total work time"),
            "the basis line matters as much as the numbers:\n{rendered}"
        );
        assert!(
            rendered.contains("estimated"),
            "this is an estimate of observed attention, not a timesheet:\n{rendered}"
        );

        // Pin each number to its own label. Asserting only that the words appear
        // somewhere would be satisfied by the agent rows, which carry the same two
        // labels -- so the two prompt windows must genuinely overlap here, or one
        // value could stand in for both.
        assert_ne!(
            report.attention_union_seconds, report.attention_naive_sum_seconds,
            "the fixture has to overlap, or this test cannot tell the two numbers apart"
        );
        let union = hm(report.attention_union_seconds);
        let naive = hm(report.attention_naive_sum_seconds);
        assert!(
            rendered
                .lines()
                .any(|line| line.contains("union") && line.contains(&union)),
            "the day-wide union appears against its own label ({union}):\n{rendered}"
        );
        assert!(
            rendered
                .lines()
                .any(|line| line.contains("naive sum") && line.contains(&naive)),
            "so does the per-repository naive sum ({naive}):\n{rendered}"
        );
        assert!(
            rendered.contains("gap marker"),
            "and gap markers are stated as excluded rather than left to be inferred:\n{rendered}"
        );
    }

    #[test]
    fn a_time_of_day_no_clock_ever_shows_is_refused_the_way_an_impossible_date_is() {
        // `--day` refuses `2026-02-30` rather than sliding it into March. The hours
        // are held to the same rule: a time that never happened is a refusal, not a
        // value quietly saturated to the nearest one that did.
        for refused in [
            "25:00", "24:00", "09:60", "99:99", "9:00", "+9:00", "0900", "09.00", "09:0",
            "09:00:00", "-1:00", "", "noon",
        ] {
            assert!(
                matches!(
                    TimeOfDay::parse(refused),
                    Err(ReportError::InvalidTimeOfDay(_))
                ),
                "{refused:?} is not a time of day and must be refused rather than interpreted, \
                 but it parsed as {:?}",
                TimeOfDay::parse(refused).ok()
            );
        }
        // The other half, or "refuse everything" would pass: real times parse, and
        // parse to the minute they name rather than to some constant.
        for (accepted, seconds) in [
            ("00:00", 0),
            ("09:30", 9 * 3600 + 30 * 60),
            ("23:59", 23 * 3600 + 59 * 60),
        ] {
            let parsed = TimeOfDay::parse(accepted).unwrap_or_else(|e| panic!("{accepted:?}: {e}"));
            assert_eq!(
                parsed.seconds_from_midnight(),
                seconds,
                "{accepted:?} names {seconds}s after midnight"
            );
        }
    }

    #[test]
    fn a_from_at_or_after_its_to_is_refused_rather_than_reported_as_an_empty_stretch() {
        let noon = TimeOfDay::parse("12:00").expect("a time of day");
        let nine = TimeOfDay::parse("09:00").expect("a time of day");

        assert!(
            matches!(
                DayWindow::new(day(), Some(noon), Some(nine)),
                Err(ReportError::InvertedRange { .. })
            ),
            "a window that runs backwards names no stretch of the day"
        );
        // Equal bounds name no stretch either, and an empty view of one would read as
        // "nothing happened then" rather than "you asked for nothing".
        assert!(
            matches!(
                DayWindow::new(day(), Some(noon), Some(noon)),
                Err(ReportError::InvertedRange { .. })
            ),
            "`--from 12:00 --to 12:00` is half-open and therefore empty by construction"
        );

        // A minute apart is a real stretch, or "refuse every pair" would pass here.
        let after = TimeOfDay::parse("12:01").expect("a time of day");
        let window = DayWindow::new(day(), Some(noon), Some(after)).expect("a minute is a stretch");
        let (start, end) = window.utc_window(TZ);
        assert_eq!(end - start, 60);
    }

    #[test]
    fn either_bound_alone_narrows_only_its_own_end_of_the_day() {
        let (midnight, next_midnight) = day().utc_window(TZ);
        let nine = TimeOfDay::parse("09:00").expect("a time of day");

        assert_eq!(
            DayWindow::new(day(), Some(nine), None)
                .expect("a from with no to")
                .utc_window(TZ),
            (midnight + 9 * 3600, next_midnight),
            "`--from 09:00` alone means 09:00 to the end of the day"
        );
        assert_eq!(
            DayWindow::new(day(), None, Some(nine))
                .expect("a to with no from")
                .utc_window(TZ),
            (midnight, midnight + 9 * 3600),
            "and `--to 09:00` alone means the start of the day to 09:00"
        );
        assert_eq!(
            window().utc_window(TZ),
            (midnight, next_midnight),
            "and neither of them is the whole day, unchanged"
        );
    }

    #[test]
    fn the_narrowed_bounds_are_read_in_the_reporting_offset_rather_than_in_utc() {
        // The rest of the command reads `--day` in `--tz-offset-hours`. A `--from`
        // read in UTC would land nine hours from where the person pointed, and the
        // header would still print a window that looked right.
        let nine = TimeOfDay::parse("09:00").expect("a time of day");
        let window = DayWindow::new(day(), Some(nine), None).expect("a stretch of the day");

        assert_eq!(
            crate::clock::format_utc_seconds(window.utc_window(TZ).0),
            "2026-07-26T00:00:00Z",
            "09:00 in UTC+09:00 is midnight UTC"
        );
        assert_eq!(
            crate::clock::format_utc_seconds(window.utc_window(TzOffset::from_hours(0)).0),
            "2026-07-26T09:00:00Z",
            "and 09:00 in UTC is 09:00 UTC -- whichever offset the command was run with"
        );
    }

    /// The block a heading opens: its own line and everything under it, up to the
    /// blank line that ends it.
    ///
    /// Scoped rather than whole-output on purpose. `per calendar day` heads a line
    /// under both `attention` and `agent runtime`, and an assertion against the whole
    /// render could be satisfied by the other block's number -- the exact way a
    /// rendering test comes to assert less than its name claims.
    fn block<'a>(rendered: &'a str, heading: &str) -> Vec<&'a str> {
        let found: Vec<&str> = rendered
            .lines()
            .skip_while(|line| !line.starts_with(heading))
            .take_while(|line| !line.trim().is_empty())
            .collect();
        assert!(!found.is_empty(), "no {heading:?} block in:\n{rendered}");
        found
    }

    /// The one line of `block` whose label is `label`, as its whitespace-separated
    /// fields with the label removed. Panics if the label heads no line or more than
    /// one, so an assertion can never quietly land on a neighbour.
    fn fields<'a>(block: &[&'a str], label: &str) -> Vec<&'a str> {
        let mut matching = block
            .iter()
            .filter(|line| line.trim_start().starts_with(label));
        let line = matching
            .next()
            .unwrap_or_else(|| panic!("no {label:?} line in:\n{}", block.join("\n")));
        assert!(
            matching.next().is_none(),
            "{label:?} heads more than one line in:\n{}",
            block.join("\n")
        );
        line.trim_start()
            .strip_prefix(label)
            .expect("the line starts with the label")
            .split_whitespace()
            .collect()
    }

    #[test]
    fn a_periods_union_is_computed_over_its_own_anchors_not_summed_from_its_days() {
        // The naive-sum error one level up. Work either side of local midnight:
        // two prompts 4 minutes apart, and two agent records 2 minutes apart, all
        // straddling the boundary between 2026-07-26 and 2026-07-27.
        //
        // Attention: the windows [-1m, +5m) around 23:58 and 00:02 overlap, so the
        // period unions them to 600s. Each day on its own sees one window and reports
        // the whole 360s of it (windows are never clipped at the boundary), so the two
        // daily reports add up to 720s -- 2 minutes of the same clock counted twice.
        //
        // Agent: the two records are 120s apart, inside the 300s threshold, so the
        // period has one 120s cluster. Split across two daily reports each record is
        // alone and clusters to a zero-length span, so the days add up to 0s.
        let root = TempRoot::new("report-period-not-sum-of-days");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[86_280, 86_520],
                agent: &[86_340, 86_460],
            }],
        );
        let first = day();
        let second = Day::parse("2026-07-27").expect("a well-formed day");
        let period = Period::range(first, second).expect("a two-day range");

        let report = run_report(root.path(), period, TZ).expect("report the two days");
        let one = run_report(root.path(), Period::of_day(first), TZ).expect("report the first");
        let two = run_report(root.path(), Period::of_day(second), TZ).expect("report the second");

        assert_eq!(
            report.attention_union_seconds, 600,
            "the period unions the windows its own anchors produced"
        );
        assert_eq!(
            one.attention_union_seconds + two.attention_union_seconds,
            720,
            "while the two days, each unioning only its own, add up to more"
        );
        assert_ne!(
            report.attention_union_seconds,
            one.attention_union_seconds + two.attention_union_seconds,
            "if these agreed the fixture would not straddle midnight and this test would \
             pass against an implementation that summed the days"
        );

        assert_eq!(
            report.agent_union_seconds, 120,
            "one cluster over the period, because the two records are inside the threshold"
        );
        assert_eq!(
            one.agent_union_seconds + two.agent_union_seconds,
            0,
            "and two zero-length ones day by day, because each record is alone in its day"
        );

        // The disagreeing second reading is reported rather than hidden, and it is
        // exactly what summing the daily reports would have produced.
        assert_eq!(
            report.attention_daily_sum_seconds,
            one.attention_union_seconds + two.attention_union_seconds
        );
        assert_eq!(
            report.agent_daily_sum_seconds,
            one.agent_union_seconds + two.agent_union_seconds
        );

        let rendered = render(&report);
        assert_eq!(
            fields(&block(&rendered, "attention"), "attention")[0],
            hm(report.attention_union_seconds),
            "the period's own union is the number the block leads with"
        );
        assert!(
            block(&rendered, "attention")
                .iter()
                .any(|line| line.contains("daily unions added up")
                    && line.contains(&hm(report.attention_daily_sum_seconds))),
            "and the daily sum appears against its own label:\n{rendered}"
        );
        assert!(
            block(&rendered, "agent runtime")
                .iter()
                .any(|line| line.contains("daily clusters added up")
                    && line.contains(&hm(report.agent_daily_sum_seconds))),
            "so does the agent one:\n{rendered}"
        );
    }

    #[test]
    fn the_two_averages_divide_by_the_days_they_each_name() {
        // Two days of work inside a seven-day week. 720s of attention over 7 calendar
        // days is 102s (2m); over the 2 days that hold an observation it is 360s (6m).
        // A single denominator would have to pick one of those and call it "per day".
        let root = TempRoot::new("report-averages");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3_600, -165_600],
                agent: &[],
            }],
        );
        let week = Period::week_ending(day());

        let report = run_report(root.path(), week, TZ).expect("report the week");

        assert_eq!(report.calendar_days, 7);
        assert_eq!(report.active_days, 2);
        assert_eq!(report.attention_union_seconds, 720);

        let rendered = render(&report);
        let attention = block(&rendered, "attention");
        assert_eq!(
            fields(&attention, "per calendar day"),
            ["2m", "over", "7", "days"],
            "720s over 7 calendar days, and the denominator is named on the line"
        );
        assert_eq!(
            fields(&attention, "per active day"),
            ["6m", "over", "2", "days", "with", "any", "observation"],
            "720s over the 2 days that hold an observation, likewise named"
        );
        // The two must genuinely differ here, or one denominator could stand in for
        // the other and this test would not notice.
        assert_ne!(
            fields(&attention, "per calendar day")[0],
            fields(&attention, "per active day")[0]
        );
    }

    #[test]
    fn a_day_with_no_observation_is_a_calendar_day_but_never_an_active_one() {
        // The week holds one day of work. Four days earlier a gap marker landed --
        // dated to when `cclogger archive` ran, so it is evidence about collection and
        // says nothing about that day. Counting it would make an untouched day active.
        let root = TempRoot::new("report-active-days");
        let session = "0000000f-1111-4111-8111-111111111111";
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/worked.jsonl",
            &[prompt_line(session, "u-0", &at(3_600), &cwd)],
            ACQUIRED_AT,
        );
        archive(
            root.path(),
            ".claude/projects/synthetic/unparseable.jsonl",
            &["{ not json".to_string()],
            &at(-342_000),
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report = run_report(root.path(), Period::week_ending(day()), TZ).expect("the week");

        assert_eq!(
            report.calendar_days, 7,
            "the week has seven days either way"
        );
        assert_eq!(
            report.active_days, 1,
            "only the day holding an observation of activity is active"
        );
        assert_eq!(
            report.coverage.gap_markers, 1,
            "and the gap marker is still counted as coverage, not discarded"
        );

        // Without this the test would pass just as well if the marker had landed
        // outside the week entirely, and nothing above would notice.
        let marker_day = run_report(
            root.path(),
            Period::of_day(Day::parse("2026-07-22").unwrap()),
            TZ,
        )
        .expect("the marker's own day");
        assert_eq!(
            marker_day.coverage.gap_markers, 1,
            "the marker really does sit on a day inside the week"
        );
        assert_eq!(
            marker_day.active_days, 0,
            "and that day, holding nothing else, is not an active day of its own"
        );
        assert_eq!(marker_day.calendar_days, 1);
    }

    #[test]
    fn a_week_is_the_seven_days_ending_on_the_one_it_names() {
        let week = Period::week_ending(day());

        // The label is the header, so both ends and the count are pinned where a
        // reader actually sees them -- and a week that reached six or eight days back
        // would print a first date this does not accept.
        assert_eq!(
            week.label(),
            "2026-07-20 .. 2026-07-26  (7 days ending 2026-07-26)"
        );
        assert_eq!(week.calendar_days(), 7);
        let (start, end) = week.utc_window(TZ);
        assert_eq!(
            crate::clock::format_utc_seconds(start),
            "2026-07-19T15:00:00Z"
        );
        assert_eq!(
            crate::clock::format_utc_seconds(end),
            "2026-07-26T15:00:00Z"
        );
        assert_eq!(end - start, 7 * 86_400);

        // It is a rolling seven days, not a calendar week, and it crosses a month
        // boundary by counting days rather than by decrementing a month number.
        let across = Period::week_ending(Day::parse("2026-03-02").expect("a well-formed day"));
        assert!(
            across.label().starts_with("2026-02-24 .. 2026-03-02"),
            "seven days back from 2026-03-02 is 2026-02-24, not 2026-02-23 or -25: {}",
            across.label()
        );

        // And the boundary is inclusive at both ends: seven days, not six or eight.
        let root = TempRoot::new("report-week-bounds");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[-6 * 86_400 + 3_600, -7 * 86_400 + 3_600],
                agent: &[],
            }],
        );

        let report = run_report(root.path(), week, TZ).expect("report the week");
        assert_eq!(
            report.prompts, 1,
            "the prompt on the week's first day is in it and the one a day earlier is not"
        );
    }

    #[test]
    fn a_month_covers_exactly_the_days_its_own_calendar_gives_it() {
        for (text, first, last, days) in [
            ("2026-07", "2026-07-01", "2026-07-31", 31),
            ("2026-02", "2026-02-01", "2026-02-28", 28),
            ("2024-02", "2024-02-01", "2024-02-29", 29),
            ("2026-04", "2026-04-01", "2026-04-30", 30),
            // December, where the next month is in the next year.
            ("2026-12", "2026-12-01", "2026-12-31", 31),
            ("2026-01", "2026-01-01", "2026-01-31", 31),
        ] {
            let month = Period::month(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(
                month.label(),
                format!("{text}  ({first} .. {last}, {days} days)"),
                "{text} runs {first} .. {last} and holds {days} days"
            );
            assert_eq!(month.calendar_days(), days, "{text} holds {days} days");
            assert_eq!(
                month.utc_window(TZ).1 - month.utc_window(TZ).0,
                days * 86_400,
                "and the window it queries is that many days long, not a nominal 30"
            );
        }
        for refused in [
            "2026-13",
            "2026-00",
            "2026-1",
            "202607",
            "+026-07",
            "2026-07-01",
            "20a6-07",
            "",
            "2026",
            "-026-07",
        ] {
            assert!(
                matches!(Period::month(refused), Err(ReportError::InvalidMonth(_))),
                "{refused:?} is not a calendar month and must be refused rather than \
                 interpreted, but it parsed as {:?}",
                Period::month(refused).ok().map(|p| p.label())
            );
        }
    }

    #[test]
    fn a_range_whose_end_precedes_its_start_is_refused_rather_than_quietly_reordered() {
        let first = Day::parse("2026-07-03").expect("a well-formed day");
        let last = Day::parse("2026-07-09").expect("a well-formed day");

        assert!(
            matches!(
                Period::range(last, first),
                Err(ReportError::InvertedPeriod { .. })
            ),
            "a range that runs backwards names no stretch of the calendar"
        );
        let range = Period::range(first, last).expect("a week's worth of days");
        assert_eq!(range.calendar_days(), 7, "both ends are inside the range");
        // Unlike `--from`/`--to` on a day, which are half-open times, both ends here
        // name a whole day -- so equal ends are one day, not nothing.
        let same = Period::range(first, first).expect("one day is a range of one");
        assert_eq!(same.calendar_days(), 1);
        assert_eq!(same.utc_window(TZ), first.utc_window(TZ));
    }

    #[test]
    fn a_period_with_no_observation_reports_no_coverage_rather_than_zero_hours() {
        let root = TempRoot::new("report-empty-period");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3_600],
                agent: &[3_600],
            }],
        );

        let empty = run_report(root.path(), Period::month("2026-05").unwrap(), TZ)
            .expect("report a month with nothing in it");

        assert_eq!(empty.active_days, 0);
        assert_eq!(empty.calendar_days, 31);
        assert!(
            !empty.within_observed_range,
            "nothing was ever collected for this month"
        );
        let rendered = render(&empty);
        for heading in ["attention", "agent runtime"] {
            let lines = block(&rendered, heading);
            assert_eq!(
                fields(&lines, "per calendar day")[0],
                "n/a",
                "an average over a month nothing was collected for is not 0m of work:\n{rendered}"
            );
            assert_eq!(fields(&lines, "per active day")[0], "n/a");
        }
        assert!(
            rendered.contains("no coverage"),
            "and the difference is stated rather than left to be inferred:\n{rendered}"
        );

        // The same ledger's own month must print real averages, or an implementation
        // that always said `n/a` would pass everything above.
        let worked = run_report(root.path(), Period::month("2026-07").unwrap(), TZ)
            .expect("report the month the fixture worked in");
        assert_eq!(worked.active_days, 1);
        let rendered = render(&worked);
        let attention = block(&rendered, "attention");
        assert_eq!(fields(&attention, "per calendar day")[0], "0m");
        assert_eq!(
            fields(&attention, "per active day"),
            ["6m", "over", "1", "day", "with", "any", "observation"],
            "one day, singular -- the denominator is a count a reader has to be able to read"
        );
    }

    #[test]
    fn one_day_is_reported_as_a_day_rather_than_as_a_period_averaged_over_itself() {
        let root = TempRoot::new("report-single-day-shape");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3_600],
                agent: &[3_600, 3_660],
            }],
        );

        let one = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let rendered = render(&one);

        assert_eq!(one.calendar_days, 1);
        assert!(
            rendered.starts_with("2026-07-26  UTC+09:00  ("),
            "a day is headed by its own date, unchanged:\n{rendered}"
        );
        for absent in [
            "per calendar day",
            "per active day",
            "daily unions added up",
            "daily clusters added up",
            "calendar days",
            "An active day is",
        ] {
            assert!(
                !rendered.contains(absent),
                "{absent:?} says nothing about one day averaged over itself:\n{rendered}"
            );
        }
        assert_eq!(
            one.attention_daily_sum_seconds, one.attention_union_seconds,
            "there is only one day to sum, so the two readings cannot disagree"
        );

        // The mirror, or "never print the period lines" would pass the whole test.
        let two = run_report(
            root.path(),
            Period::range(day(), Day::parse("2026-07-27").unwrap()).unwrap(),
            TZ,
        )
        .expect("report two days");
        let rendered = render(&two);
        assert!(
            rendered.starts_with("2026-07-26 .. 2026-07-27  (2 days)  UTC+09:00  ("),
            "and a period is headed by the days it holds:\n{rendered}"
        );
        for present in [
            "per calendar day",
            "per active day",
            "2 calendar days",
            "An active day is",
        ] {
            assert!(
                rendered.contains(present),
                "{present:?} belongs to a period of more than one day:\n{rendered}"
            );
        }
    }

    #[test]
    fn a_narrowed_window_says_which_hours_it_kept_and_a_whole_day_says_nothing() {
        let nine = TimeOfDay::parse("09:00").expect("a time of day");
        let noon = TimeOfDay::parse("12:00").expect("a time of day");

        assert_eq!(window().range_label(), None);
        assert_eq!(
            DayWindow::new(day(), Some(nine), Some(noon))
                .expect("a stretch of the day")
                .range_label()
                .as_deref(),
            Some("09:00-12:00")
        );
        // A bound that was never given is never invented: `24:00` is not a time this
        // command accepts, so it is not a time this command prints either.
        assert_eq!(
            DayWindow::new(day(), Some(nine), None)
                .expect("a from with no to")
                .range_label()
                .as_deref(),
            Some("from 09:00")
        );
        assert_eq!(
            DayWindow::new(day(), None, Some(noon))
                .expect("a to with no from")
                .range_label()
                .as_deref(),
            Some("to 12:00")
        );
    }

    #[test]
    fn a_bound_is_read_as_a_date_or_a_time_by_its_separator() {
        assert_eq!(
            Bound::parse("2026-07-26").expect("a well-formed date"),
            Bound::Date(day())
        );
        assert_eq!(
            Bound::parse("09:00").expect("a well-formed time"),
            Bound::Time(time("09:00"))
        );

        // A value shaped like an attempted date is refused the way `Day::parse`
        // always refused it -- the separator decides which parser reads a malformed
        // value, not whether a malformed value is refused.
        assert!(matches!(
            Bound::parse("2026-02-30"),
            Err(ReportError::InvalidDay(_))
        ));
        assert!(matches!(
            Bound::parse("25:00"),
            Err(ReportError::InvalidTimeOfDay(_))
        ));
        assert!(matches!(
            Bound::parse("banana"),
            Err(ReportError::InvalidDay(_))
        ));
    }

    #[test]
    fn a_report_narrowed_by_from_to_computes_over_only_that_stretch_of_the_day() {
        // Proves the narrowing reaches `run_report`'s own query, not just the header
        // that describes it: a prompt outside the window must not be counted, the
        // way it would not be if an implementation quietly ignored `--from`/`--to`
        // and reported the whole day regardless.
        let root = TempRoot::new("report-narrowed-window");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[8 * 3600, 20 * 3600], // 08:00 and 20:00 local
                agent: &[8 * 3600, 20 * 3600],
            }],
        );

        let nine = time("09:00");
        let eighteen = time("18:00");
        let narrowed = Period::of_day_window(day(), Some(nine), Some(eighteen))
            .expect("09:00-18:00 is a real stretch of the day");
        let report = run_report(root.path(), narrowed, TZ).expect("report the narrowed window");

        assert_eq!(
            report.prompts, 0,
            "both prompts (08:00 and 20:00) fall outside 09:00-18:00, so the narrowed \
             report must not count either of them"
        );
        assert_eq!(report.range.as_deref(), Some("09:00-18:00"));
        assert!(
            report.single_day,
            "a narrowed day is still one day, not a period"
        );

        let rendered = render(&report);
        assert!(
            rendered.starts_with("2026-07-26  09:00-18:00  UTC+09:00  ("),
            "the header must say the report was narrowed, or a narrowed report would \
             look exactly like a quiet whole day:\n{rendered}"
        );

        // The mirror, or an implementation that always excluded everything would
        // pass the assertion above for the wrong reason.
        let whole_day =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the whole day");
        assert_eq!(
            whole_day.prompts, 2,
            "the unnarrowed day holds both prompts"
        );
        assert_eq!(
            whole_day.range, None,
            "a whole day was never narrowed, and must not claim it was"
        );
    }

    // -- reaction time ------------------------------------------------------------

    /// One session's worth of alternating turns: a completion, then the prompts that
    /// answer (or fail to answer) it, repeated often enough to clear
    /// [`MIN_GAPS_FOR_QUARTILES`].
    ///
    /// `completion_offsets` and `prompt_offsets` are built from the same `k` so a test
    /// can state the shape it wants once and read the expected gap straight off it.
    fn cycles(count: i64, step: i64, at_offsets: &[i64]) -> Vec<i64> {
        (1..=count)
            .flat_map(|k| at_offsets.iter().map(move |offset| k * step + offset))
            .collect()
    }

    /// The distribution's quartiles, or a panic naming what was there instead --
    /// `unwrap()` on a `None` here says nothing about how many gaps were measured.
    fn quartiles(distribution: &GapDistribution) -> Quartiles {
        distribution.quartiles.unwrap_or_else(|| {
            panic!(
                "expected quartiles over {} gap(s), but the distribution has none",
                distribution.count
            )
        })
    }

    fn repository_gaps<'a>(report: &'a PeriodReport, repository: &str) -> &'a RepositoryGaps {
        report
            .response_times
            .by_repository
            .iter()
            .find(|row| row.repository.as_deref() == Some(repository))
            .unwrap_or_else(|| {
                panic!(
                    "{repository} measured no reaction time; the rows are {:?}",
                    report
                        .response_times
                        .by_repository
                        .iter()
                        .map(|row| (row.repository.clone(), row.response.count))
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn a_completion_is_spent_by_the_prompt_that_answers_it_and_never_answers_a_second() {
        // The requirement this metric exists for, and the mistake that is easy to make.
        // Each cycle is one completion followed by two prompts: the first answers the
        // completion (100s later), the second finds nothing unconsumed and is
        // back-to-back. An implementation that keeps the completion around instead of
        // consuming it measures the second prompt from output that already belongs to
        // the first -- a second gap of 900s that is not a reaction to anything.
        let root = TempRoot::new("report-response-consumed");
        let completions = cycles(12, 2000, &[0]);
        let prompts = cycles(12, 2000, &[100, 900]);
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &prompts,
                agent: &completions,
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let times = &report.response_times;

        assert_eq!(
            times.response.count, 12,
            "twelve completions answered once each -- not twenty-four, which is what \
             reusing a spent completion produces"
        );
        assert_eq!(
            times.back_to_back_prompts, 12,
            "and the twelve prompts that found nothing unconsumed are counted as \
             back-to-back rather than measured against a completion already spent"
        );
        assert_eq!(
            times.prompts_walked, 24,
            "every prompt was walked; half of them simply had no reaction to measure"
        );
        assert_eq!(
            times.response.count + times.back_to_back_prompts,
            times.prompts_walked,
            "a prompt either answers a completion or is back-to-back, never both and \
             never neither"
        );

        let measured = quartiles(&times.response);
        assert_eq!(
            (measured.p25, measured.median, measured.p75),
            (100, 100, 100),
            "every measured reaction is the 100s one; the 900s gap to the second prompt \
             of each cycle is not a reaction and must appear in no quartile"
        );
    }

    #[test]
    fn a_prompt_never_consumes_a_completion_from_another_session() {
        // Sessions run side by side constantly. `api` produced completions and was
        // never answered; `web` sent prompts a minute after each of them and had no
        // completion of its own to answer. Pairing on one global timeline would report
        // twelve 60s reactions in `web` to output from a session the person was not
        // even looking at.
        //
        // `docs` is the control: its own completions, its own prompts, 30s apart. An
        // implementation that measured nothing at all would pass the first half of this
        // test and fail here.
        let root = TempRoot::new("report-response-per-session");
        let api_completions = cycles(12, 1000, &[0]);
        let web_prompts = cycles(12, 1000, &[60]);
        let docs_completions = cycles(12, 1000, &[40_000]);
        let docs_prompts = cycles(12, 1000, &[40_030]);
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[],
                    agent: &api_completions,
                },
                Session {
                    repository: "github.com/acme/web",
                    prompts: &web_prompts,
                    agent: &[],
                },
                Session {
                    repository: "github.com/acme/docs",
                    prompts: &docs_prompts,
                    agent: &docs_completions,
                },
            ],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let times = &report.response_times;

        assert_eq!(
            times.response.count, 12,
            "only the control session's twelve pairs are reactions; a global pairing \
             would find twenty-four"
        );
        assert_eq!(
            times.back_to_back_prompts, 12,
            "and `web`'s prompts had nothing of their own to answer"
        );
        assert_eq!(
            times.interval.count, 22,
            "the reference interval is scoped to a session too: eleven gaps inside each \
             of the two sessions that prompted, never twenty-three across the pair"
        );
        assert_eq!(
            quartiles(&times.response).median,
            30,
            "the measured reactions are the control's 30s ones, never `web`'s 60s gap \
             to another session's output"
        );
        assert_eq!(
            repository_gaps(&report, "github.com/acme/docs")
                .response
                .count,
            12
        );
        assert!(
            times
                .by_repository
                .iter()
                .all(|row| row.repository.as_deref() != Some("github.com/acme/web")),
            "`web` measured no reaction at all, so it has no row: {:?}",
            times
                .by_repository
                .iter()
                .map(|row| row.repository.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_later_completion_supersedes_an_older_one_the_person_never_answered() {
        // An assistant turn that calls tools emits a completion per assistant record,
        // so several arrive before the person types. What they reacted to is the last
        // thing they were shown: measuring from the first would report a reaction that
        // began while the assistant was still working, and would grow with every tool
        // the turn happened to call.
        let root = TempRoot::new("report-response-supersede");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &cycles(12, 3000, &[700]),
                agent: &cycles(12, 3000, &[0, 600]),
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let times = &report.response_times;

        assert_eq!(
            times.response.count, 12,
            "two completions and one prompt per cycle is one reaction, not two"
        );
        assert_eq!(
            quartiles(&times.response).median,
            100,
            "measured from the later completion, which is what the person was looking \
             at -- not the 700s back to the first one, which they never answered"
        );
        assert_eq!(
            times.back_to_back_prompts, 0,
            "and the superseded completion does not leave a prompt unanswered"
        );
    }

    #[test]
    fn turns_are_paired_in_instant_order_even_when_the_stored_text_sorts_them_backwards() {
        // `occurred_at` is normalized to UTC so that a lexicographic range query can be
        // trusted -- except for a value the normalizer cannot parse, which is stored
        // verbatim, offset and all. A leap second is that case: `:60` is legal RFC 3339
        // and the normalizer rejects it. So the twelve completions below keep their
        // `+09:00` text and sort *after* every prompt, an hour of text order that lies
        // about the instants. Walking the query's order would find each prompt with
        // nothing unconsumed and report twelve back-to-back prompts and no reaction at
        // all; the pair is really a minute apart, every time.
        let root = TempRoot::new("report-response-lying-order");
        let session = "0000000e-1111-4111-8111-111111111111";
        let cwd = cwd_for("github.com/acme/api");
        let mut lines = Vec::new();
        for k in 0..12 {
            let minute = k * 5;
            lines.push(assistant_line(
                session,
                &format!("a-{k}"),
                // 18:{minute}:60 +09:00 is 09:{minute + 1}:00Z.
                &format!("2026-07-26T18:{minute:02}:60.000+09:00"),
                &cwd,
            ));
            lines.push(prompt_line(
                session,
                &format!("u-{k}"),
                &format!("2026-07-26T09:{:02}:00.000Z", minute + 2),
                &cwd,
            ));
        }
        archive(
            root.path(),
            ".claude/projects/synthetic/leap.jsonl",
            &lines,
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let times = &report.response_times;

        assert_eq!(
            report.prompts, 12,
            "the fixture's prompts all fall in the day, whatever their text sorts as"
        );
        assert_eq!(
            times.response.count, 12,
            "each completion is answered by the prompt a minute after it -- the order \
             the instants give, not the order the text does"
        );
        assert_eq!(
            times.back_to_back_prompts, 0,
            "and none of them is back-to-back, which is what the text order would claim"
        );
        assert_eq!(
            quartiles(&times.response).median,
            60,
            "a minute, measured forwards; reading the pair backwards would measure a \
             negative reaction, which is not a duration at all"
        );
    }

    #[test]
    fn a_subject_that_names_no_session_is_left_out_rather_than_pooled_with_the_others() {
        // `subject` is a hierarchical path, and only its first segment names the
        // session. Reading the whole path would make every turn its own session, so no
        // completion would ever meet the prompt that answered it; treating a subject
        // that names none as a session of its own would let unrelated records pair.
        assert_eq!(session_ref(Some("session/ses_2XQ")), Some("ses_2XQ"));
        assert_eq!(
            session_ref(Some("session/ses_2XQ/turn/trn_91M")),
            Some("ses_2XQ"),
            "the session is the first segment; the turn under it is the same session"
        );
        assert_eq!(
            session_ref(Some("session/ses_2XQ/tool/tol_7")),
            Some("ses_2XQ")
        );
        for names_none in ["source/gap/line-4", "tool/tol_7", "session/", "session", ""] {
            assert_eq!(
                session_ref(Some(names_none)),
                None,
                "{names_none:?} names no session, and must not become one"
            );
        }
        assert_eq!(session_ref(None), None);
    }

    #[test]
    fn a_completion_copied_out_of_another_transcript_measures_no_reaction() {
        // A Codex fork re-writes the parent's history into the child's file in one
        // flush, stamping every record with the copy's write time. The assistant turn
        // in that flush is real output the person saw -- hours earlier, in the parent's
        // session, and answered there. Measuring the child's own live prompt against it
        // reports a two-hour reaction that says only when the fork ran. Before the
        // ledger distinguished these, 83.5% of Codex measurements came out at exactly
        // zero seconds.
        let root = TempRoot::new("report-response-copied");
        let cwd = cwd_for("github.com/acme/api");
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/forked.jsonl",
            &crate::import::tests::codex_forked_rollout_with_copied_response(
                &cwd,
                &at(3600),
                &at(10_800),
            ),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let times = &report.response_times;

        assert_eq!(
            report.coverage.observations_with_inherited_time, 2,
            "the copied prompt and the copied completion are both counted, not made to \
             disappear"
        );
        assert_eq!(
            times.response.count, 0,
            "and neither reaches the pairing: the live prompt has no completion of its \
             own to answer"
        );
        assert_eq!(
            times.back_to_back_prompts, 1,
            "so the one live prompt is back-to-back, which is what it truly was"
        );

        // The other half. The same 7,200s apart, both records live: that pair is a
        // reaction and must be measured, or "exclude everything" would pass above.
        let live = TempRoot::new("report-response-copied-control");
        build(
            live.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[10_800],
                agent: &[3600],
            }],
        );
        let control = run_report(live.path(), Period::of_day(day()), TZ).expect("report the day");
        assert_eq!(
            control.response_times.response.count, 1,
            "the identical timing, live, is one measured reaction"
        );
        assert_eq!(control.response_times.back_to_back_prompts, 0);
    }

    #[test]
    fn no_sum_and_no_mean_of_the_reaction_times_reaches_the_output() {
        // Sessions overlap, so gaps added up exceed the clock they happened on -- the
        // same naive-sum error the attention block already warns about, one level down.
        // A day of the real corpus summed this way came to 62 hours. The mean is barred
        // for a different reason: the tail runs into the tens of minutes, so a mean sits
        // between the usual reaction and the few long ones and describes neither.
        let root = TempRoot::new("report-response-no-total");
        let gaps: Vec<i64> = (1..=12).map(|k| k * 60).collect();
        let completions = cycles(12, 5_000, &[0]);
        let prompts: Vec<i64> = gaps
            .iter()
            .enumerate()
            .map(|(i, gap)| (i as i64 + 1) * 5_000 + gap)
            .collect();
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &prompts,
                agent: &completions,
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let times = &report.response_times;
        assert_eq!(times.response.count, 12);

        let total: i64 = gaps.iter().sum();
        let mean = total / gaps.len() as i64;
        let measured = quartiles(&times.response);
        assert_eq!(
            (measured.p25, measured.median, measured.p75),
            (180, 360, 540),
            "the shape is stated: the 3rd, 6th and 9th of twelve gaps"
        );
        // Distinct from every quartile, so neither could be mistaken for one that is
        // legitimately printed.
        assert_eq!((total, mean), (4680, 390));

        let block = block(&render(&report), "response time").join("\n");
        assert!(
            !block.contains(&hms(total)),
            "the gaps sum to {}, and that number must appear nowhere:\n{block}",
            hms(total)
        );
        assert!(
            !block.contains(&hms(mean)),
            "their mean is {}, and that number must appear nowhere either:\n{block}",
            hms(mean)
        );
        assert!(
            block.contains(&hms(measured.median)),
            "while the median ({}) is exactly what the block is for:\n{block}",
            hms(measured.median)
        );
    }

    #[test]
    fn too_few_gaps_to_have_a_shape_is_said_rather_than_computed_from_the_handful_there_are() {
        // Three points have a median in the arithmetic sense and none in any sense a
        // reader would want. Printed, it reads on the page exactly like one drawn from
        // three hundred.
        let root = TempRoot::new("report-response-too-few");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[1100, 2100, 3100],
                agent: &[1000, 2000, 3000],
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        assert_eq!(
            report.response_times.response.count, 3,
            "three gaps were measured, and that count is a true count of three"
        );
        assert_eq!(
            report.response_times.response.quartiles, None,
            "but three of them have no shape to report"
        );

        let rendered = render(&report);
        let stated = fields(&block(&rendered, "response time"), "response time");
        assert_eq!(
            stated[0..2],
            ["n", "3"],
            "the count is still stated -- withholding it too would hide that anything \
             was measured at all:\n{rendered}"
        );
        assert!(
            !stated.contains(&"median"),
            "and no median is offered:\n{rendered}"
        );

        // The mirror: enough gaps, and the shape appears. Without this, an
        // implementation that never computed quartiles would pass.
        let enough = TempRoot::new("report-response-enough");
        build(
            enough.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &cycles(12, 1000, &[100]),
                agent: &cycles(12, 1000, &[0]),
            }],
        );
        let report = run_report(enough.path(), Period::of_day(day()), TZ).expect("report the day");
        assert_eq!(report.response_times.response.count, 12);
        assert_eq!(quartiles(&report.response_times.response).median, 100);
        assert!(
            fields(&block(&render(&report), "response time"), "response time").contains(&"median")
        );
    }

    #[test]
    fn a_quartile_is_an_observed_gap_rather_than_an_interpolation_between_two() {
        // Twelve gaps: six short, six long. The nearest-rank median is the 6th, an 8s
        // reaction that really happened. Interpolating between the 6th and the 7th --
        // what most quantile helpers do on an even sample -- would print 504s, which
        // nothing took and no one waited.
        let root = TempRoot::new("report-response-nearest-rank");
        let short: Vec<i64> = (3..=8).collect();
        let long: Vec<i64> = (1000..=1005).collect();
        let mut prompts = Vec::new();
        let mut completions = Vec::new();
        for (k, gap) in short.iter().chain(long.iter()).enumerate() {
            let base = (k as i64 + 1) * 5_000;
            completions.push(base);
            prompts.push(base + gap);
        }
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &prompts,
                agent: &completions,
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let measured = quartiles(&report.response_times.response);

        assert_eq!(
            measured.median, 8,
            "the 6th of twelve, an interval that was really measured"
        );
        assert_ne!(
            measured.median,
            (8 + 1000) / 2,
            "and not the midpoint of the 6th and the 7th, which nothing took"
        );
        assert_eq!(measured.p25, 5, "the 3rd of twelve");
        assert_eq!(measured.p75, 1002, "and the 9th");
    }

    #[test]
    fn the_reference_interval_is_the_plain_gap_between_prompts_generation_time_included() {
        // The owner's second question. Every cycle here is the same: prompt, 600s of
        // generation, completion, 400s of reaction, next prompt. The reaction is what
        // the person spent; the interval is 1,000s, and the 600s between the two lines
        // is what the model spent. Measured on a real corpus the difference runs two to
        // three minutes.
        let root = TempRoot::new("report-response-interval");
        let prompts = cycles(14, 1000, &[0]);
        let completions = cycles(13, 1000, &[600]);
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &prompts,
                agent: &completions,
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let times = &report.response_times;

        assert_eq!(
            quartiles(&times.response).median,
            400,
            "the reaction excludes the 600s the model spent generating"
        );
        assert_eq!(
            times.interval.count, 13,
            "fourteen prompts leave thirteen intervals between them"
        );
        assert_eq!(
            quartiles(&times.interval).median,
            1000,
            "while the interval is the whole wait, generation included"
        );
        assert_eq!(
            quartiles(&times.interval).median - quartiles(&times.response).median,
            600,
            "and the difference between the two is what the model spent"
        );
        assert_eq!(
            times.back_to_back_prompts, 1,
            "only the opening prompt had no output of its own to answer"
        );
    }

    #[test]
    fn a_periods_reaction_times_are_its_own_gaps_not_an_average_of_its_days_medians() {
        // The naive-average error the daily/period pair already guards against for
        // attention. A slow day and a fast day: averaging the two daily medians gives
        // 505s, a number neither day had and no reaction took. The period's own
        // distribution is all twenty-four gaps together, whose 12th is 10s.
        let root = TempRoot::new("report-response-period");
        let first = day();
        let second = Day::parse("2026-07-27").expect("a well-formed day");
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &cycles(12, 1000, &[10]),
                    agent: &cycles(12, 1000, &[0]),
                },
                Session {
                    repository: "github.com/acme/api",
                    prompts: &cycles(12, 1000, &[86_400 + 1000]),
                    agent: &cycles(12, 1000, &[86_400]),
                },
            ],
        );

        let period = Period::range(first, second).expect("a two-day range");
        let report = run_report(root.path(), period, TZ).expect("report the range");
        let one = run_report(root.path(), Period::of_day(first), TZ).expect("report the first");
        let two = run_report(root.path(), Period::of_day(second), TZ).expect("report the second");

        assert_eq!(quartiles(&one.response_times.response).median, 10);
        assert_eq!(quartiles(&two.response_times.response).median, 1000);
        assert_eq!(
            report.response_times.response.count, 24,
            "the period's distribution holds both days' gaps"
        );
        assert_eq!(
            quartiles(&report.response_times.response).median,
            10,
            "the 12th of the twenty-four, which is a gap one of the days really had"
        );
        assert_ne!(
            quartiles(&report.response_times.response).median,
            (quartiles(&one.response_times.response).median
                + quartiles(&two.response_times.response).median)
                / 2,
            "never the average of the daily medians, which is 505s -- a duration \
             neither day held"
        );
        assert_eq!(
            report.response_times.response.count,
            one.response_times.response.count + two.response_times.response.count,
            "counts do add up, and this one does, so the median differing is about the \
             median rather than about the period holding different gaps"
        );
    }

    #[test]
    fn reaction_times_are_broken_down_by_repository_and_the_rows_add_back_to_the_period() {
        // Per repository as well as overall, the way attention already breaks down.
        // Two repositories with plainly different reactions, and a third with too few
        // gaps to have a median -- which says so rather than reporting one from three.
        let root = TempRoot::new("report-response-by-repository");
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &cycles(12, 1000, &[20]),
                    agent: &cycles(12, 1000, &[0]),
                },
                Session {
                    repository: "github.com/acme/web",
                    prompts: &cycles(12, 1000, &[40_500]),
                    agent: &cycles(12, 1000, &[40_000]),
                },
                Session {
                    repository: "github.com/acme/docs",
                    prompts: &[70_030, 70_130, 70_230],
                    agent: &[70_000, 70_100, 70_200],
                },
            ],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");

        assert_eq!(
            quartiles(&repository_gaps(&report, "github.com/acme/api").response).median,
            20
        );
        assert_eq!(
            quartiles(&repository_gaps(&report, "github.com/acme/web").response).median,
            500,
            "a repository's own reactions, not the period's -- these two differ by 25x"
        );
        let docs = repository_gaps(&report, "github.com/acme/docs");
        assert_eq!(docs.response.count, 3);
        assert_eq!(
            docs.response.quartiles, None,
            "three gaps is too few for a median, per repository exactly as overall"
        );

        let rows: u64 = report
            .response_times
            .by_repository
            .iter()
            .map(|row| row.response.count)
            .sum();
        assert_eq!(
            rows, report.response_times.response.count,
            "every measured reaction sits in exactly one repository's row"
        );

        let rendered = render(&report);
        let block = block(&rendered, "response time");
        assert!(
            block
                .iter()
                .any(|line| line.contains("github.com/acme/docs") && line.contains("n/a")),
            "and the repository with too few gaps says so rather than showing a \
             number:\n{}",
            block.join("\n")
        );
    }

    #[test]
    fn the_rendered_reaction_block_states_the_pairing_rule_and_both_distributions() {
        let root = TempRoot::new("report-response-render");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &cycles(14, 1000, &[0]),
                agent: &cycles(13, 1000, &[600]),
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        let rendered = render(&report);
        let block = block(&rendered, "response time");

        let response = fields(&block, "response time");
        assert_eq!(
            response,
            [
                "n",
                "13",
                "p25",
                &hms(400),
                "median",
                &hms(400),
                "p75",
                &hms(400)
            ],
            "the reaction distribution leads the block, count first:\n{rendered}"
        );
        assert_eq!(
            fields(&block, "interval"),
            [
                "n",
                "13",
                "p25",
                &hms(1000),
                "median",
                &hms(1000),
                "p75",
                &hms(1000)
            ],
            "and the prompt interval is stated beside it, for reference:\n{rendered}"
        );
        assert!(
            block
                .iter()
                .any(|line| line.contains("1 of 14 prompt(s) were back-to-back")),
            "how many prompts had no output of their own to answer is a fact about how \
             the person works, and is reported:\n{rendered}"
        );
        assert!(
            block.iter().any(|line| line.contains("spent doing it")),
            "and the rule that makes the number a reaction time -- one completion, one \
             prompt -- is stated rather than left to be inferred:\n{rendered}"
        );
    }

    #[test]
    fn hms_keeps_the_seconds_a_measured_gap_actually_had() {
        // `hm` rounds to the minute because attention is what a windowing parameter
        // produced. A reaction is the difference between two recorded timestamps, and
        // almost a tenth of real ones are under ten seconds: rounded to `0m` they would
        // read as no wait at all.
        assert_eq!(hms(8), "8s");
        assert_eq!(hms(0), "0s");
        assert_eq!(hms(60), "1m00s");
        assert_eq!(hms(331), "5m31s");
        assert_eq!(hms(3600), "1h00m00s");
        assert_eq!(hms(4523), "1h15m23s");
        assert_ne!(
            hms(8),
            hm(8),
            "which is exactly where the two formats differ"
        );
    }

    #[test]
    fn a_prompt_in_a_period_with_no_prompts_at_all_says_so_rather_than_reporting_zero() {
        // Absence, stated. A period whose reaction count is 0 because nothing was ever
        // typed reads identically to one where every reaction was instant, unless the
        // output says which it is.
        let root = TempRoot::new("report-response-none");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[],
                agent: &[3600, 3700],
            }],
        );

        let report = run_report(root.path(), Period::of_day(day()), TZ).expect("report the day");
        assert_eq!(report.response_times.response.count, 0);
        assert_eq!(report.response_times.prompts_walked, 0);

        let rendered = render(&report);
        assert!(
            block(&rendered, "response time")
                .iter()
                .any(|line| line.contains("not the same as reacting instantly")),
            "a period with nothing to react to must not read as one reacted to \
             instantly:\n{rendered}"
        );
    }

    // -- the hook channel on the clock -------------------------------------------

    /// Spool one hook payload at `offset_seconds` past local midnight, through the
    /// real receiver, so what these tests read back is what the real path writes.
    fn spool_hook(root: &Path, offset_seconds: i64, payload: serde_json::Value) {
        assert_eq!(
            crate::hook::run_hook(root, payload.to_string().as_bytes(), &at(offset_seconds)),
            0,
            "the receiver never fails"
        );
    }

    fn hook_payload(event: &str, cwd: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut record = json!({
            "session_id": "44444444-4444-4444-8444-444444444444",
            "prompt_id": "prm-1",
            "cwd": cwd,
            "hook_event_name": event,
        });
        for (k, v) in extra.as_object().expect("an object") {
            record[k] = v.clone();
        }
        record
    }

    #[test]
    fn a_hook_observation_reaches_the_clock_even_though_its_time_is_an_arrival_stamp() {
        // `received_at` is a *measurement* of the event's instant -- Claude Code invokes
        // the hook synchronously -- unlike `acquired_at` and `copied_at`, which are
        // other events' times standing in for one that was never measured. Excluding it
        // would leave the whole channel contributing nothing to any number this tool
        // prints, which is the failure this test exists to catch.
        let root = TempRoot::new("report-hook-clock");
        let cwd = cwd_for("github.com/acme/api");
        spool_hook(
            root.path(),
            3600,
            hook_payload("UserPromptSubmit", &cwd, json!({})),
        );
        spool_hook(
            root.path(),
            3660,
            hook_payload(
                "PreToolUse",
                &cwd,
                json!({ "tool_use_id": "toolu-1", "tool_name": "Bash" }),
            ),
        );
        spool_hook(
            root.path(),
            3720,
            hook_payload(
                "PostToolUse",
                &cwd,
                json!({ "tool_use_id": "toolu-1", "tool_name": "Bash", "duration_ms": 60_000 }),
            ),
        );
        run_import(root.path(), false).expect("import the spool");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");
        assert_eq!(
            report.prompts, 1,
            "the hook prompt is a human turn and must be counted"
        );
        assert!(
            report.attention_union_seconds > 0,
            "and must open an attention window: {report:?}"
        );
        assert_eq!(
            report.agent_union_seconds, 60,
            "the tool call's start and finish are one minute apart, and both reach the \
             agent clock: {report:?}"
        );
        assert_eq!(report.active_days, 1, "so the day was worked");
        assert_eq!(
            report.coverage.observations_with_inherited_time, 0,
            "an arrival stamp is not a copied one"
        );
    }

    #[test]
    fn coverage_says_where_hook_capture_begins_rather_than_folding_it_into_the_vendor() {
        // A hook observation *is* a Claude Code observation, so it sits inside that
        // vendor's range and the instant capture began would otherwise be invisible --
        // a ledger holding years of transcript and one day of hooks would read as if
        // both covered the same period.
        let root = TempRoot::new("report-hook-coverage");
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/history.jsonl",
            &[prompt_line(
                "55555555-5555-4555-8555-555555555555",
                "p-1",
                &at(600),
                &cwd,
            )],
            ACQUIRED_AT,
        );
        spool_hook(
            root.path(),
            40_000,
            hook_payload("UserPromptSubmit", &cwd, json!({})),
        );
        run_import(root.path(), false).expect("import both channels");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");
        let hook = report
            .coverage
            .hook_capture
            .as_ref()
            .expect("hook capture has started");
        assert_eq!(hook.earliest, at(40_000));
        assert_eq!(hook.source_kind, HOOK_CHANNEL);

        // The vendor range still starts at the transcript prompt, so the two really are
        // saying different things.
        let vendor = report
            .coverage
            .sources
            .iter()
            .find(|s| s.source_kind == "claude-code")
            .expect("a claude-code range");
        assert_eq!(vendor.earliest, at(600));

        let rendered = render(&report);
        assert!(
            rendered.contains("hook capture         from"),
            "the coverage block must print it:\n{rendered}"
        );
        assert!(
            rendered.contains("days before that were seen only through transcripts"),
            "and must say what the earlier days rest on:\n{rendered}"
        );
    }

    #[test]
    fn a_ledger_with_no_hook_capture_says_nothing_about_it() {
        let root = TempRoot::new("report-hook-absent");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[3660],
            }],
        );
        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");
        assert!(report.coverage.hook_capture.is_none());
        assert!(
            !render(&report).contains("hook capture"),
            "an installation with no hooks must not be told about a channel it does not use"
        );
    }

    #[test]
    fn a_hook_captured_turn_measures_a_reaction_time_like_any_other() {
        // The reaction-time walk gathers its turns inside the same loop that applies
        // `time_is_when_it_happened`, so admitting `received_at` onto clocks admits it
        // here too -- automatically, from one policy in one place. That is the right
        // arrangement and it is also the kind of thing that is true by accident until
        // someone adds a second filter, so it is pinned rather than reasoned about.
        //
        // A hook-captured reaction is in fact the best-measured one this tool has: both
        // ends are stamped as they happen, rather than reconstructed from a transcript
        // written asynchronously.
        let root = TempRoot::new("report-hook-reaction");
        let cwd = cwd_for("github.com/acme/api");
        let reaction = 100;
        for i in 0..MIN_GAPS_FOR_QUARTILES as i64 {
            let cycle = 3600 + i * 3000;
            let mut completed = hook_payload("Stop", &cwd, json!({}));
            completed["prompt_id"] = json!(format!("prm-{i}"));
            spool_hook(root.path(), cycle, completed);

            let mut answered = hook_payload("UserPromptSubmit", &cwd, json!({}));
            answered["prompt_id"] = json!(format!("prm-{}", i + 1));
            spool_hook(root.path(), cycle + reaction, answered);
        }
        run_import(root.path(), false).expect("import the spool");

        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the synthetic day");
        let times = &report.response_times;
        assert_eq!(
            times.prompts_walked, MIN_GAPS_FOR_QUARTILES as u64,
            "every hook prompt must reach the walk: {times:?}"
        );
        assert_eq!(
            times.response.count, MIN_GAPS_FOR_QUARTILES as u64,
            "each prompt answered the completion before it, so each is a measured \
             reaction -- a channel excluded from the clock would measure none: {times:?}"
        );
        assert_eq!(
            times.back_to_back_prompts, 0,
            "no prompt here found the completion already spent: {times:?}"
        );
        // The value, not merely the count: a walk that paired the wrong records would
        // still count twelve.
        let quartiles = times
            .response
            .quartiles
            .as_ref()
            .expect("twelve gaps is exactly the threshold");
        assert_eq!(
            (quartiles.p25, quartiles.median, quartiles.p75),
            (reaction, reaction, reaction),
            "every reaction was {reaction}s: {times:?}"
        );
        assert_eq!(
            times
                .by_repository
                .iter()
                .find(|row| row.repository.as_deref() == Some("github.com/acme/api"))
                .map(|row| row.response.count),
            Some(MIN_GAPS_FOR_QUARTILES as u64),
            "and they are attributed to the repository the prompt resolved to: {times:?}"
        );
    }
}
