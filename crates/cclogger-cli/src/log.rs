//! `cclogger log`: a day as a shape rather than a table.
//!
//! `cclogger report` answers "how much, on what". It cannot answer "at once, or one
//! after the other" -- two rows reading `2h36m` and `17m` look identical whether the
//! work interleaved all afternoon or sat in separate hours. This module draws
//! the axis that distinguishes them: one row per repository across the day, then the
//! blocks of work underneath it, each marked when it spans more than one repository.
//!
//! Overlap is the structure, not noise: 82.5% of within-session gaps of five minutes
//! or more have *another* session active inside them (issue #22). That measurement is
//! why the agent clock clusters the global cross-session timeline, and it is why a
//! block here is never split per repository -- splitting it would report the same
//! minutes twice, which is the trap design §13 exists to keep this project out of.
//!
//! **What this cannot show.** The ledger is metadata-only by design: it
//! holds timestamps, opaque refs, event types and tool families, and no prompt text
//! at all. So there is no title here, no summary of what a block was *about*, and
//! nothing synthesized from a tool name or a repository name to stand in for one --
//! a row reading `cclog · edit` invites being read as "edited cclog", which the data
//! does not establish. Everything printed is either measured or named as absent.
//!
//! Nothing here writes, and nothing here re-derives what `report.rs` already settled:
//! the day parse and its refusal of dates that never happened, the day's UTC window,
//! the workstream config, the attention window constants and the duration format all
//! come from that module, so the two commands cannot drift apart about the same day.
//!
//! **The header is one line by default.** Everything below it is an estimate derived
//! from a named algorithm, and every one of those derivations has a caveat worth
//! reading -- but fourteen lines of them before the first row of content is a wall in
//! front of a command that gets run daily, and a wall is not read either. `--explain`
//! prints all of it, unchanged. The two caveats that stayed are the two a reader
//! reaches a wrong conclusion without: the window the numbers were computed over, and
//! that the blocks are not a column to add up.

use crate::report::{
    AGENT_GAP_SECONDS, ATTENTION_AFTER_SECONDS, ATTENTION_BEFORE_SECONDS, COMMIT_EVENT_TYPE,
    ConfigStatus, DayWindow, GAP_EVENT_TYPE, PROMPT_EVENT_TYPE, ReportError, UNASSIGNED,
    attention_windows, hm, load_rules, prefix_bound, share, time_is_when_it_happened,
};
use crate::tz::TzOffset;
use cclogger_adapters::rfc3339;
use cclogger_archive::Ledger;
use cclogger_domain::clock::{Span, partition_by_nearest_anchor, union_seconds};
use cclogger_domain::workstream::Rules;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

/// The event type whose `data.tool_family` this view tallies. One per tool call --
/// the matching `tool.finished` event carries the same family, and counting both
/// would double every number.
const TOOL_STARTED_EVENT_TYPE: &str = "dev.cclog.tool.started.v1";

/// The cell sizes the strip may use, finest first. Every one divides an hour, so the
/// hour ruler always lands on a cell boundary, and every one divides a day, so the
/// last cell of the day is a whole cell.
///
/// The strip widens along this ladder rather than truncating the axis: a strip that
/// silently drops hours is worse than a coarser one, because a reader cannot see that
/// it did.
const BUCKET_LADDER_SECONDS: [i64; 5] = [600, 900, 1200, 1800, 3600];

/// The width the strip tries to fit in. Approximate by intent -- the legend and the
/// block list may exceed it; what must not happen is the *axis* being cut to fit.
const TARGET_COLUMNS: usize = 100;

/// Density ramp, lightest first. Index 0 is "nothing observed in this cell", and is
/// the only cell that renders as a space: one observation is never drawn as nothing.
const DENSITY_RAMP: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// What the union row is labelled. Parenthesized so it cannot be mistaken for one of
/// the repository labels above it, which are path suffixes.
const UNION_LABEL: &str = "(all)";

/// What a row of observations that carried no repository is labelled. Distinct from
/// `unassigned` (a missing workstream rule) -- see [`crate::report::Unattributed`].
const NO_REPOSITORY_LABEL: &str = "(none)";

/// One repository's day across the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryStrip {
    /// The normalized identity, or `None` when these observations carried none.
    pub repository: Option<String>,
    /// The short label the strip and the block list print. A trailing run of the
    /// identity's `/`-separated segments, lengthened until it is unique among the
    /// day's repositories -- never a name invented for display.
    pub label: String,
    /// `None` is `unassigned`: no `[[rule]]` matched.
    pub workstream: Option<String>,
    /// Observations per cell across the whole day, `bucket_seconds` each.
    pub observations: Vec<u64>,
    /// Human prompts per cell, same indexing.
    pub prompts: Vec<u64>,
    pub total_observations: u64,
    pub total_prompts: u64,
    /// The source kinds that observed this repository today, as the ledger stored
    /// them (`claude-code`, `codex`) -- not shortened, which would be a rename.
    pub vendors: Vec<String>,
}

/// One repository's share of a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPartRow {
    pub repository: Option<String>,
    pub label: String,
    /// This repository's share of the block's attention, by nearest anchor. Shares
    /// across a block's parts sum to [`BlockRow::attention_seconds`].
    pub attention_seconds: i64,
    pub prompts: u64,
    pub observations: u64,
}

/// One block: a stretch of continuous work, and what it was made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRow {
    pub span: Span,
    /// Ordered by observation count descending, then named repositories
    /// alphabetically, with the no-repository part last (see `block.rs`).
    pub parts: Vec<BlockPartRow>,
    /// How many *named* repositories this block covers. More than one is the
    /// observation the strip exists to make, and the render says so on its own line.
    pub named_repositories: usize,
    pub prompts: u64,
    /// The union of this block's prompt windows, which its parts' shares sum to.
    pub attention_seconds: i64,
    /// Tool families started inside the block, count descending then name. A family
    /// of `None` (a tool event whose adapter recorded none) is carried as its own
    /// entry rather than folded into `other`, which is a family an adapter chose.
    pub tools: Vec<(Option<String>, u64)>,
    pub vendors: Vec<String>,
    /// Commits whose author time falls inside this block's span, by repository, count
    /// descending then name.
    ///
    /// The one thing this view could never say before: what came of the block. It is
    /// containment and nothing more -- a commit inside the span landed while the block
    /// was running -- and that is deliberately weaker than "this block produced it".
    /// Which prompt earned which commit is a linking claim (data-model §8) that needs
    /// evidence this ledger does not hold, and a heuristic that guessed would be read
    /// as a measurement.
    ///
    /// A commit contributes nothing to the block's span, its attention, its prompts or
    /// its tools: a block is a stretch of observed activity, and a commit is an instant
    /// that happened to fall in one. A block never exists *because* of a commit.
    ///
    /// Labelled the same way [`BlockPartRow::label`] is -- the shortest unique suffix of
    /// the identity -- so a commit's repository reads as the same thing the part above
    /// it does.
    pub commits: Vec<(String, u64)>,
}

impl BlockRow {
    pub fn elapsed_seconds(&self) -> i64 {
        self.span.end - self.span.start
    }
}

/// One day, as a shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayLog {
    pub day: String,
    /// How `--from`/`--to` narrowed the day, as the header names it (`13:00-15:00`),
    /// or `None` for the whole of it. Everything else in this struct was computed
    /// over that narrowing, which is why the header prints it: fewer blocks over a
    /// stretch of the day looks exactly like a quieter day otherwise.
    pub range: Option<String>,
    pub tz_offset: TzOffset,
    pub window_start_utc: String,
    pub window_end_utc: String,
    pub config: ConfigStatus,
    /// How much time one strip cell covers. Chosen so the axis fits; stated in the
    /// header either way, because a cell size the reader has to guess at makes every
    /// distance on the strip unreadable.
    pub bucket_seconds: i64,
    pub buckets_per_day: usize,
    /// The inclusive cell range the strip draws: the active span, snapped out to
    /// whole hours. Empty days leave it at `0..=0` and draw nothing.
    pub first_bucket: usize,
    pub last_bucket: usize,
    pub rows: Vec<RepositoryStrip>,
    /// Every repository at once, per cell.
    pub union_row: Vec<u64>,
    /// The busiest single cell of any single repository -- what the density ramp is
    /// scaled to, and printed, since the ramp means nothing without it.
    pub peak_cell_observations: u64,
    pub blocks: Vec<BlockRow>,
    pub prompts: u64,
    /// Commits that landed anywhere in the window, whether or not a block was running.
    pub commits: u64,
    /// Of those, the ones no block was running for: work that landed while this machine
    /// observed no AI session at all.
    ///
    /// Reported rather than folded into the blocks: a commit outside every block is the
    /// clearest evidence the ledger holds that its own picture of a day is partial, and
    /// attaching it to the nearest block would be a guess dressed as an observation.
    pub commits_outside_blocks: u64,
    pub observations_in_window: u64,
    /// Lines the importer could not transform, dated to when `cclogger archive` ran.
    /// Evidence about collection, never about activity: kept off the strip entirely.
    pub gap_markers: u64,
    /// Observations a Codex fork copied out of its parent's transcript, carrying the
    /// copy's write time rather than their own. Real events at an unknown instant, so
    /// they are kept off the strip for the same reason gap markers are -- a cell they
    /// lit would say work happened in a minute nothing did.
    pub observations_with_inherited_time: u64,
    pub observations_without_usable_time: u64,
    pub within_observed_range: bool,
}

/// Everything one observation contributes to this view.
struct DayEvent {
    instant: i64,
    repository: Option<String>,
    is_prompt: bool,
    source_kind: String,
    /// `Some` only on a tool-start event; the inner `Option` is the family the
    /// adapter recorded, which may itself be absent.
    tool: Option<Option<String>>,
}

/// Read one stretch of one day out of the ledger at `root` and project it onto a
/// time axis.
///
/// Same inputs, same day and same refusals as [`crate::report::run_report`] --
/// deliberately, since two views of one day that disagreed about which observations
/// belong to it would be worse than either alone.
///
/// A `window` narrower than the day narrows the query itself: every count, duration
/// and coverage number below is computed over `window`, not over the day it sits in.
/// Only the strip's cell *indexing* stays keyed to local midnight, so that an hour
/// mark still names the hour it names.
pub fn run_log(root: &Path, window: DayWindow, tz_offset: TzOffset) -> Result<DayLog, ReportError> {
    if !tz_offset.is_real_zone() {
        return Err(ReportError::InvalidOffset(tz_offset));
    }
    let ledger_path = root.join("ledger.db");
    if !ledger_path.exists() {
        return Err(ReportError::NoLedger(ledger_path));
    }
    // Opening a ledger below this build's schema upgrades and re-stamps it, which is
    // a write. A read-only command asks first and stops.
    if cclogger_archive::needs_schema_upgrade(root)? {
        return Err(ReportError::LedgerNeedsUpgrade(ledger_path));
    }

    let (config, rules) = load_rules(root)?;
    let ledger = Ledger::open(root)?;
    let (start, end) = window.utc_window(tz_offset);
    // The cell grid is the day's, even when the query is narrower: a cell index is
    // "how far into the day", so keying it to `start` instead would slide every hour
    // mark by the narrowing and label 14:00 over an hour that is not 14:00.
    let day_start = window.day().utc_window(tz_offset).0;

    // The same prefilter margin `report.rs` uses, for the same reason: `occurred_at`
    // is compared lexicographically and a value normalization could not rewrite is
    // stored verbatim, so its text order says nothing about the instant it names.
    // See that module's call site for the full argument.
    const PREFILTER_MARGIN_SECONDS: i64 = 86_400;
    let rows = ledger.observations_between(
        &prefix_bound(start - PREFILTER_MARGIN_SECONDS),
        &prefix_bound(end + PREFILTER_MARGIN_SECONDS),
    )?;
    let displays: BTreeMap<String, String> = ledger.identities("repository")?.into_iter().collect();

    let mut observations_in_window = 0u64;
    let mut gap_markers = 0u64;
    let mut observations_with_inherited_time = 0u64;
    let mut observations_without_usable_time = 0u64;
    let mut events: Vec<DayEvent> = Vec::new();
    // Kept out of `events` on purpose, and that is the whole of the "commits are not a
    // clock" rule as it applies here: `events` is what the strip's density cells, the
    // block clustering and the attention partition are all built from. A commit in there
    // would light a cell, extend a block's span past its last observation, and let a
    // block exist that is one instant long.
    let mut commits: Vec<(i64, Option<String>)> = Vec::new();

    for row in &rows {
        let instant = rfc3339::epoch_seconds(&row.occurred_at);
        if let Some(secs) = instant
            && (secs < start || secs >= end)
        {
            continue; // sorted into range, but not this day's instant
        }
        observations_in_window += 1;

        if row.event_type == GAP_EVENT_TYPE {
            gap_markers += 1;
            continue;
        }
        if !time_is_when_it_happened(row) {
            // A copied record's timestamp is when the fork re-serialized it, so a cell
            // it lit would claim work in a minute that held only the copying.
            observations_with_inherited_time += 1;
            continue;
        }
        let Some(secs) = instant else {
            observations_without_usable_time += 1;
            continue;
        };

        let repository = row.repository_ref.as_ref().map(|opaque| {
            displays
                .get(opaque)
                .cloned()
                // No registered display: show the opaque ref rather than invent a
                // name for it. It is not readable, but it is true.
                .unwrap_or_else(|| opaque.clone())
        });
        if row.event_type == COMMIT_EVENT_TYPE {
            commits.push((secs, repository));
            continue;
        }
        events.push(DayEvent {
            instant: secs,
            repository,
            is_prompt: row.event_type == PROMPT_EVENT_TYPE,
            source_kind: row.source_kind.clone(),
            tool: (row.event_type == TOOL_STARTED_EVENT_TYPE).then(|| row.tool_family.clone()),
        });
    }

    let sources = ledger.observed_range_by_source(&[GAP_EVENT_TYPE])?;
    let within_observed_range = sources.iter().any(|range| {
        match (
            rfc3339::epoch_seconds(&range.earliest),
            rfc3339::epoch_seconds(&range.latest),
        ) {
            (Some(earliest), Some(latest)) => earliest < end && latest >= start,
            _ => false,
        }
    });

    let strip = Strip::build(&events, day_start, &rules);
    let mut blocks = block_rows(&events, &strip.labels);
    let commits_outside_blocks = place_commits(&mut blocks, &commits, &strip.labels);
    let prompts = events.iter().filter(|e| e.is_prompt).count() as u64;

    Ok(DayLog {
        day: window.day().label(),
        range: window.range_label(),
        tz_offset,
        window_start_utc: crate::clock::format_utc_seconds(start),
        window_end_utc: crate::clock::format_utc_seconds(end),
        config,
        bucket_seconds: strip.bucket_seconds,
        buckets_per_day: strip.buckets_per_day,
        first_bucket: strip.first_bucket,
        last_bucket: strip.last_bucket,
        union_row: strip.union_row,
        peak_cell_observations: strip.peak_cell_observations,
        rows: strip.rows,
        blocks,
        prompts,
        commits: commits.len() as u64,
        commits_outside_blocks,
        observations_in_window,
        gap_markers,
        observations_with_inherited_time,
        observations_without_usable_time,
        within_observed_range,
    })
}

// -- the strip -------------------------------------------------------------------

/// What one pass over the day's events produces for the axis.
struct Strip {
    bucket_seconds: i64,
    buckets_per_day: usize,
    first_bucket: usize,
    last_bucket: usize,
    rows: Vec<RepositoryStrip>,
    union_row: Vec<u64>,
    peak_cell_observations: u64,
    labels: BTreeMap<Option<String>, String>,
}

/// One repository's raw instants, before a cell size has been chosen.
#[derive(Default)]
struct Accumulated {
    instants: Vec<i64>,
    prompt_instants: Vec<i64>,
    vendors: BTreeSet<String>,
}

impl Strip {
    fn build(events: &[DayEvent], day_start: i64, rules: &Rules) -> Self {
        let mut per_repository: BTreeMap<Option<String>, Accumulated> = BTreeMap::new();
        for event in events {
            let entry = per_repository.entry(event.repository.clone()).or_default();
            entry.instants.push(event.instant);
            if event.is_prompt {
                entry.prompt_instants.push(event.instant);
            }
            entry.vendors.insert(event.source_kind.clone());
        }

        let labels = short_labels(per_repository.keys());

        // Ordered heaviest first, so the day's main thread of work is the top row.
        // The no-repository row goes last among equals and ties break by label, the
        // same rule `block.rs` orders a block's parts by: the residual is not what a
        // strip is about, and an order that moved between runs would make two
        // printouts of one day look like two different days.
        let mut ordered: Vec<(Option<String>, Accumulated)> = per_repository.into_iter().collect();
        ordered.sort_by(|(a_repo, a), (b_repo, b)| {
            b.instants
                .len()
                .cmp(&a.instants.len())
                .then_with(|| a_repo.is_none().cmp(&b_repo.is_none()))
                .then_with(|| labels[a_repo].cmp(&labels[b_repo]))
        });

        let Some((min, max)) = span_of(&ordered) else {
            return Strip {
                bucket_seconds: BUCKET_LADDER_SECONDS[0],
                buckets_per_day: (86_400 / BUCKET_LADDER_SECONDS[0]) as usize,
                first_bucket: 0,
                last_bucket: 0,
                rows: Vec::new(),
                union_row: Vec::new(),
                peak_cell_observations: 0,
                labels,
            };
        };

        let label_width = ordered
            .iter()
            .map(|(repo, _)| labels[repo].chars().count())
            .chain(std::iter::once(UNION_LABEL.chars().count()))
            .max()
            .unwrap_or_default();
        // Measured on the strings the renderer will actually print, the union row's
        // included: its vendor tag lists every vendor of the day and is therefore the
        // longest of them, so a budget taken from the repository rows alone would let
        // exactly one line overrun.
        let all_vendors: BTreeSet<&str> = ordered
            .iter()
            .flat_map(|(_, acc)| acc.vendors.iter().map(String::as_str))
            .collect();
        let tail_width = ordered
            .iter()
            .map(|(_, acc)| {
                row_tail(acc.prompt_instants.len() as u64, &vendor_tag(&acc.vendors))
                    .chars()
                    .count()
            })
            .chain(std::iter::once(
                row_tail(
                    ordered
                        .iter()
                        .map(|(_, acc)| acc.prompt_instants.len() as u64)
                        .sum(),
                    &vendor_tag(&all_vendors),
                )
                .chars()
                .count(),
            ))
            .max()
            .unwrap_or_default();

        let bucket_seconds = choose_bucket(min, max, day_start, label_width + tail_width);
        let buckets_per_day = (86_400 / bucket_seconds) as usize;
        let (first_bucket, last_bucket) = active_cells(min, max, day_start, bucket_seconds);

        let mut rows: Vec<RepositoryStrip> = Vec::new();
        let mut union_row = vec![0u64; buckets_per_day];
        for (repository, acc) in ordered {
            let mut observations = vec![0u64; buckets_per_day];
            let mut prompts = vec![0u64; buckets_per_day];
            for instant in &acc.instants {
                let cell = ((instant - day_start) / bucket_seconds) as usize;
                observations[cell] += 1;
                union_row[cell] += 1;
            }
            for instant in &acc.prompt_instants {
                prompts[((instant - day_start) / bucket_seconds) as usize] += 1;
            }
            rows.push(RepositoryStrip {
                label: labels[&repository].clone(),
                workstream: repository
                    .as_ref()
                    .and_then(|r| rules.workstream_for(r))
                    .map(str::to_string),
                repository,
                total_observations: acc.instants.len() as u64,
                total_prompts: acc.prompt_instants.len() as u64,
                observations,
                prompts,
                vendors: acc.vendors.into_iter().collect(),
            });
        }

        let peak_cell_observations = rows
            .iter()
            .flat_map(|row| row.observations.iter().copied())
            .max()
            .unwrap_or_default();

        Strip {
            bucket_seconds,
            buckets_per_day,
            first_bucket,
            last_bucket,
            rows,
            union_row,
            peak_cell_observations,
            labels,
        }
    }
}

/// What follows a strip row's cells: how many human prompts fell in it, and which
/// vendors observed it.
///
/// One function for both the renderer and the width budget that chooses the cell
/// size, so the two cannot disagree about how wide a row is -- a budget computed from
/// a format string that a later edit changed would silently start overrunning.
fn row_tail(prompts: u64, vendors: &str) -> String {
    format!("  {prompts:>4} prompts  {vendors}")
}

/// The earliest and latest instant anything was observed at, or `None` for a day the
/// ledger holds nothing in.
fn span_of(ordered: &[(Option<String>, Accumulated)]) -> Option<(i64, i64)> {
    let min = ordered
        .iter()
        .filter_map(|(_, acc)| acc.instants.iter().min())
        .min()?;
    let max = ordered
        .iter()
        .filter_map(|(_, acc)| acc.instants.iter().max())
        .max()?;
    Some((*min, *max))
}

/// The inclusive cell range covering `[min, max]`, snapped out to whole hours so the
/// ruler's hour marks are the range's own edges.
fn active_cells(min: i64, max: i64, day_start: i64, bucket_seconds: i64) -> (usize, usize) {
    let per_hour = (3600 / bucket_seconds) as usize;
    let per_day = (86_400 / bucket_seconds) as usize;
    let first = ((min - day_start) / bucket_seconds) as usize;
    let last = ((max - day_start) / bucket_seconds) as usize;
    (
        first - first % per_hour,
        (last + (per_hour - 1 - last % per_hour)).min(per_day - 1),
    )
}

/// The finest cell size whose axis still fits [`TARGET_COLUMNS`] once `fixed` columns
/// of label and tail are accounted for -- or, if none does, the coarsest available.
///
/// Widening is the deliberate answer to a day that does not fit: the alternative,
/// cutting the axis short, drops hours of the day without saying it did.
fn choose_bucket(min: i64, max: i64, day_start: i64, fixed: usize) -> i64 {
    for bucket in BUCKET_LADDER_SECONDS {
        let (first, last) = active_cells(min, max, day_start, bucket);
        if fixed + 2 + (last - first + 1) <= TARGET_COLUMNS {
            return bucket;
        }
    }
    BUCKET_LADDER_SECONDS[BUCKET_LADDER_SECONDS.len() - 1]
}

/// The shortest trailing run of `/`-separated segments that tells each repository
/// apart from every other one in the day.
///
/// A shortening, never a rename: every label is a literal suffix of the identity it
/// stands for, and the legend under the strip prints the identity in full next to it.
/// Two repositories whose chosen suffixes collide both fall back to their full
/// identity, which is unique by construction, so no two rows can ever share a label.
fn short_labels<'a>(
    repositories: impl Iterator<Item = &'a Option<String>>,
) -> BTreeMap<Option<String>, String> {
    let identities: Vec<Option<String>> = repositories.cloned().collect();
    let named: Vec<&str> = identities
        .iter()
        .filter_map(|r| r.as_deref())
        .collect::<Vec<_>>();

    let mut labels: BTreeMap<Option<String>, String> = BTreeMap::new();
    for identity in &identities {
        let Some(full) = identity.as_deref() else {
            labels.insert(None, NO_REPOSITORY_LABEL.to_string());
            continue;
        };
        let depth = full.split('/').count();
        let chosen = (1..=depth)
            .map(|n| suffix(full, n))
            .find(|candidate| {
                let n = candidate.split('/').count();
                named
                    .iter()
                    .all(|other| *other == full || suffix(other, n) != *candidate)
            })
            .unwrap_or_else(|| full.to_string());
        labels.insert(identity.clone(), chosen);
    }

    // A suffix long enough to be unique may not exist -- `api` and
    // `github.com/acme/api` share every suffix `api` has. Both then fall back to
    // their full identity rather than one silently shadowing the other.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for label in labels.values() {
        *seen.entry(label.clone()).or_insert(0) += 1;
    }
    for (identity, label) in labels.iter_mut() {
        if seen[&*label] > 1
            && let Some(full) = identity.as_deref()
        {
            *label = full.to_string();
        }
    }
    labels
}

/// The last `n` `/`-separated segments of `path`, or all of it if it has fewer.
fn suffix(path: &str, n: usize) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    segments[segments.len().saturating_sub(n)..].join("/")
}

// -- the blocks ------------------------------------------------------------------

/// Project the day's events into blocks, and describe each one.
fn block_rows(events: &[DayEvent], labels: &BTreeMap<Option<String>, String>) -> Vec<BlockRow> {
    let clustered = cclogger_domain::block::blocks(
        &events
            .iter()
            .map(|e| (e.instant, e.repository.clone(), e.is_prompt))
            .collect::<Vec<_>>(),
        AGENT_GAP_SECONDS,
    );
    let spans: Vec<Span> = clustered.iter().map(|b| b.span).collect();

    // Spans are disjoint, ordered, and built from exactly these instants, so the
    // first span whose `end` has not yet reached an instant is the one holding it --
    // the same lookup `block.rs` uses to attribute its own parts.
    let mut anchors: Vec<Vec<(i64, Option<String>)>> = vec![Vec::new(); spans.len()];
    let mut tools: Vec<BTreeMap<Option<String>, u64>> = vec![BTreeMap::new(); spans.len()];
    let mut vendors: Vec<BTreeSet<String>> = vec![BTreeSet::new(); spans.len()];
    for event in events {
        let index = spans.partition_point(|span| span.end < event.instant);
        if event.is_prompt {
            anchors[index].push((event.instant, event.repository.clone()));
        }
        if let Some(family) = &event.tool {
            *tools[index].entry(family.clone()).or_insert(0) += 1;
        }
        vendors[index].insert(event.source_kind.clone());
    }

    clustered
        .into_iter()
        .zip(anchors)
        .zip(tools)
        .zip(vendors)
        .map(|(((block, anchors), tools), vendors)| {
            // The same partition the report runs day-wide, over this block's own
            // anchors: every second of the union goes to exactly one repository, so
            // the parts' shares sum back to the block's attention (design §13).
            let shares: BTreeMap<Option<String>, i64> = partition_by_nearest_anchor(
                &anchors,
                ATTENTION_BEFORE_SECONDS,
                ATTENTION_AFTER_SECONDS,
            )
            .into_iter()
            .collect();
            let attention_seconds =
                union_seconds(&attention_windows(anchors.iter().map(|(t, _)| *t)));

            let mut tools: Vec<(Option<String>, u64)> = tools.into_iter().collect();
            tools.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

            BlockRow {
                span: block.span,
                named_repositories: block
                    .parts
                    .iter()
                    .filter(|p| p.repository.is_some())
                    .count(),
                prompts: block.parts.iter().map(|p| p.prompts).sum(),
                attention_seconds,
                parts: block
                    .parts
                    .into_iter()
                    .map(|part| BlockPartRow {
                        label: labels
                            .get(&part.repository)
                            .cloned()
                            // Unreachable in practice: the labels were built from the
                            // same events. Falls back to the identity rather than a
                            // placeholder, so a bug here shows the truth.
                            .unwrap_or_else(|| {
                                part.repository
                                    .clone()
                                    .unwrap_or_else(|| NO_REPOSITORY_LABEL.to_string())
                            }),
                        attention_seconds: shares
                            .get(&part.repository)
                            .copied()
                            .unwrap_or_default(),
                        repository: part.repository,
                        prompts: part.prompts,
                        observations: part.observations,
                    })
                    .collect(),
                tools,
                vendors: vendors.into_iter().collect(),
                // Filled by `place_commits`, after the spans exist: a commit is placed
                // in a block, never part of building one.
                commits: Vec::new(),
            }
        })
        .collect()
}

/// Attach each commit to the block whose span contains it, and return how many belonged
/// to no block at all.
///
/// **Containment, not proximity.** A commit that falls in a silence between two blocks
/// is attached to neither: it is evidence that work landed at a moment this machine
/// observed nothing, which is exactly what `commits_outside_blocks` says. Snapping it to
/// the nearest block would turn "I do not know which stretch of work this came out of"
/// into a printed claim that it came out of that one -- and the block it would attach to
/// is decided by a gap threshold chosen for something else entirely.
fn place_commits(
    blocks: &mut [BlockRow],
    commits: &[(i64, Option<String>)],
    labels: &BTreeMap<Option<String>, String>,
) -> u64 {
    let mut per_block: Vec<BTreeMap<Option<String>, u64>> = vec![BTreeMap::new(); blocks.len()];
    let mut outside = 0u64;
    for (instant, repository) in commits {
        // Spans are disjoint and ordered, so the first one whose end has not yet passed
        // this instant is the only one that can hold it -- the same lookup `block_rows`
        // uses. Unlike that one, the answer is then *checked*: `partition_point` returns
        // the following block for an instant that falls in a gap, and for an instant
        // past the last block it returns one index too far.
        let index = blocks.partition_point(|block| block.span.end < *instant);
        match blocks.get(index) {
            Some(block) if block.span.start <= *instant => {
                *per_block[index].entry(repository.clone()).or_insert(0) += 1;
            }
            _ => outside += 1,
        }
    }
    for (block, counts) in blocks.iter_mut().zip(per_block) {
        let mut commits: Vec<(String, u64)> = counts
            .into_iter()
            .map(|(repository, count)| {
                // A commit in a repository with no observed activity today has no label
                // on the strip to borrow, so it shows its identity. Unreadable is better
                // than a name that was invented for it.
                let label = labels.get(&repository).cloned().unwrap_or_else(|| {
                    repository.unwrap_or_else(|| NO_REPOSITORY_LABEL.to_string())
                });
                (label, count)
            })
            .collect();
        commits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        block.commits = commits;
    }
    outside
}

// -- rendering -------------------------------------------------------------------

/// Local wall-clock `HH:MM` in the reporting offset.
fn hhmm(instant: i64, tz_offset: TzOffset) -> String {
    let local = (instant + tz_offset.seconds()).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3600, (local % 3600) / 60)
}

/// A block's clock range, or the single time it happened at.
///
/// A block whose first and last observation fall in the same minute renders as
/// `22:43`, not `22:43-22:43`. Two prompts submitted in the same second are real
/// (145 collisions in the measured corpus -- see `clock.rs`), and a zero-width range
/// reads as a formatting failure rather than as the instant it is. `elapsed`, printed
/// beside it, still carries the duration, so nothing is lost by not repeating the
/// time.
fn clock_range(span: Span, tz_offset: TzOffset) -> String {
    let start = hhmm(span.start, tz_offset);
    let end = hhmm(span.end, tz_offset);
    if start == end {
        start
    } else {
        format!("{start}-{end}")
    }
}

/// `claude-code+codex`, in the ledger's own spelling. Not abbreviated: a shortened
/// vendor name is a name this project did not observe.
fn vendor_tag(vendors: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    vendors
        .into_iter()
        .map(|v| v.as_ref().to_string())
        .collect::<Vec<_>>()
        .join("+")
}

/// One cell of the density ramp. `n == 0` is the only cell drawn as a space: a cell
/// holding one observation of a busy day would otherwise round to nothing and read as
/// idle time.
fn density(n: u64, peak: u64) -> char {
    if n == 0 || peak == 0 {
        return DENSITY_RAMP[0];
    }
    let level = ((16 * n + peak) / (2 * peak)).clamp(1, 8);
    DENSITY_RAMP[level as usize]
}

/// How much of the header to print.
///
/// The caveats are all true and none of them was removed -- but a reader who runs
/// this every day has read them, and fourteen lines of them ahead of the first row of
/// content is a wall that gets scrolled past rather than read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    /// One line: what the numbers are, and where the rest of it lives.
    Brief,
    /// Every caveat, at the length it was written -- `--explain`.
    Full,
}

/// The day as the CLI prints it.
pub fn render(log: &DayLog, header: Header) -> String {
    let mut out = String::new();
    render_window_line(&mut out, log);
    match header {
        Header::Brief => render_header_brief(&mut out, log),
        Header::Full => render_header_full(&mut out, log),
    }
    if log.rows.is_empty() {
        // "reached a cell", not "fell in this window": a window can hold gap markers
        // and unreadable timestamps, which `--explain`'s coverage line counts and
        // which never reach a clock. Saying it held nothing would contradict that.
        let _ = writeln!(
            out,
            "(no observation in this {} reached a cell -- there is no axis to draw)",
            // Never "day" when the query was narrower than one: an empty hour would
            // otherwise report that the whole day was empty, which is a different
            // claim and, here, a false one.
            if log.range.is_some() { "window" } else { "day" }
        );
        return out;
    }
    render_strip(&mut out, log);
    render_legend(&mut out, log);
    render_blocks(&mut out, log);
    out
}

/// The stretch of time every number below was computed over.
///
/// Printed in both modes, and it is the one caveat that cannot move under
/// `--explain`: `--from`/`--to` narrow the query, so a narrowed view is a smaller
/// day's worth of numbers, and without this line it is indistinguishable from a
/// quieter day.
fn render_window_line(out: &mut String, log: &DayLog) {
    let range = match &log.range {
        Some(range) => format!("  {range}"),
        None => String::new(),
    };
    let _ = writeln!(
        out,
        "{}{range}  UTC{}  ({} .. {})",
        log.day, log.tz_offset, log.window_start_utc, log.window_end_utc
    );
    let _ = writeln!(out);
}

/// The default header: one line, carrying that the number is an estimate and where
/// the reasoning behind it is.
fn render_header_brief(out: &mut String, log: &DayLog) {
    let _ = writeln!(
        out,
        "basis         AI-observed attention (est.), not total work time -- `--explain` for how"
    );
    // Kept out of `--explain` on purpose: this one says the numbers below are about a
    // stretch of time nothing was collected for, which is not a caveat about how they
    // were derived but about whether there is anything there to derive them from.
    render_uncollected_note(out, log);
    let _ = writeln!(out);
}

/// `--explain`: the header exactly as it shipped, unshortened.
///
/// Every line here was written against a specific way of misreading the output, and
/// each is still the answer to "how was this derived". What changed is that a reader
/// asks for them, rather than meeting all fourteen lines every morning.
fn render_header_full(out: &mut String, log: &DayLog) {
    let _ = writeln!(
        out,
        "basis         when the day's observations happened and which repository each belonged"
    );
    let _ = writeln!(
        out,
        "              to. Not what the work was about: the ledger holds timestamps, event"
    );
    let _ = writeln!(
        out,
        "              types and tool families, and no prompt text at all. No line"
    );
    let _ = writeln!(
        out,
        "              below is a description of the work, and none is derived from one."
    );
    let _ = writeln!(
        out,
        "blocks        every observation, prompts included, clustered on one cross-session"
    );
    let _ = writeln!(
        out,
        "              timeline at a {}m gap. `cclogger report`'s agent runtime clusters the",
        AGENT_GAP_SECONDS / 60
    );
    let _ = writeln!(
        out,
        "              non-prompt observations only, so these are not the same number."
    );
    let _ = writeln!(
        out,
        "attention     estimated from human prompts ([-{}m, +{}m] around each), not total work",
        ATTENTION_BEFORE_SECONDS / 60,
        ATTENTION_AFTER_SECONDS / 60
    );
    let _ = writeln!(
        out,
        "              time. A window is anchored on its prompt and never clipped to the block,"
    );
    let _ = writeln!(
        out,
        "              so a short block can show more attention than elapsed. Blocks are not"
    );
    let _ = writeln!(
        out,
        "              additive -- `cclogger report` has the day's total."
    );
    match &log.config {
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
    render_uncollected_note(out, log);
    let _ = writeln!(
        out,
        "coverage      {} observation(s) in window; {} gap marker(s), excluded from the strip",
        log.observations_in_window, log.gap_markers
    );
    if log.observations_with_inherited_time > 0 {
        let _ = writeln!(
            out,
            "              {} were copied into a forked transcript and carry the copy's write",
            log.observations_with_inherited_time
        );
        let _ = writeln!(
            out,
            "              time, not their own -- excluded from the strip"
        );
    }
    if log.observations_without_usable_time > 0 {
        let _ = writeln!(
            out,
            "              {} carried an unreadable timestamp and reached no cell",
            log.observations_without_usable_time
        );
    }
    let _ = writeln!(out);
}

/// The one note both header modes print: that nothing was ever collected for this
/// stretch of time.
///
/// It is not a caveat about how a number was derived -- it is the statement that
/// there was nothing to derive one from, and an empty strip under a silent header
/// reads as a day off rather than as a gap in collection.
fn render_uncollected_note(out: &mut String, log: &DayLog) {
    if log.within_observed_range {
        return;
    }
    let _ = writeln!(
        out,
        "note          this day is outside the observed range of every source: nothing was"
    );
    let _ = writeln!(
        out,
        "              collected for it, which is not the same as a day with no work."
    );
}

fn render_strip(out: &mut String, log: &DayLog) {
    let width = log
        .rows
        .iter()
        .map(|row| row.label.chars().count())
        .chain(std::iter::once(UNION_LABEL.chars().count()))
        .max()
        .unwrap_or_default();
    let cells = log.first_bucket..=log.last_bucket;

    let _ = writeln!(
        out,
        "strip         1 cell = {} min, {:02}:00 .. {:02}:00 (the active span, snapped to the hour)",
        log.bucket_seconds / 60,
        (log.first_bucket as i64 * log.bucket_seconds) / 3600,
        ((log.last_bucket as i64 + 1) * log.bucket_seconds) / 3600,
    );
    // "of any one repository" is load-bearing: `peak_cell_observations` is the busiest
    // cell of a single row, and the union row shares that scale, so its cells can hold
    // more and saturate at the top of the ramp. A legend that said only "the busiest
    // cell" would be read as an upper bound the union row does not respect.
    let _ = writeln!(
        out,
        "              {} = observations in a cell, scaled to the busiest cell of any one",
        DENSITY_RAMP[1..].iter().collect::<String>()
    );
    let _ = writeln!(
        out,
        "              repository ({}) -- the {UNION_LABEL} row shares that scale and saturates above it",
        log.peak_cell_observations
    );
    let _ = writeln!(
        out,
        "              \u{25b2} = at least one human prompt;  {UNION_LABEL} = every repository at once,"
    );
    let _ = writeln!(out, "              and any observation that carried none");
    let _ = writeln!(out);

    // The hour ruler. An hour whose label cannot start at its own cell is left
    // unlabelled rather than shifted: a mark that does not sit over the cell it names
    // is worse than no mark, and the reader can still count cells between the marks
    // that are there.
    //
    // `next_free` leaves a blank column after each label. Without it, a cell size with
    // only two cells to the hour packs `00` and `01` against each other and the whole
    // ruler reads as one long number -- `000102…` -- with no way to tell where an hour
    // begins. Labelling every other hour instead is the readable answer.
    let mut ruler = String::new();
    let mut next_free = 0usize;
    for cell in cells.clone() {
        let seconds = cell as i64 * log.bucket_seconds;
        let column = cell - log.first_bucket;
        if seconds % 3600 == 0 && column >= next_free {
            while ruler.chars().count() < column {
                ruler.push(' ');
            }
            let _ = write!(ruler, "{:02}", seconds / 3600);
            next_free = column + 3;
        }
    }
    let _ = writeln!(out, "{:width$}  {ruler}", "");

    for row in &log.rows {
        let strip: String = cells
            .clone()
            .map(|cell| density(row.observations[cell], log.peak_cell_observations))
            .collect();
        let _ = writeln!(
            out,
            "{:width$}  {strip}{}",
            row.label,
            row_tail(row.total_prompts, &vendor_tag(&row.vendors))
        );
        if cells.clone().any(|cell| row.prompts[cell] > 0) {
            let marks: String = cells
                .clone()
                .map(|cell| {
                    if row.prompts[cell] > 0 {
                        '\u{25b2}'
                    } else {
                        ' '
                    }
                })
                .collect();
            let _ = writeln!(out, "{:width$}  {}", "", marks.trim_end());
        }
    }

    let union: String = cells
        .clone()
        .map(|cell| density(log.union_row[cell], log.peak_cell_observations))
        .collect();
    let all_vendors: BTreeSet<&str> = log
        .rows
        .iter()
        .flat_map(|row| row.vendors.iter().map(String::as_str))
        .collect();
    let _ = writeln!(
        out,
        "{:width$}  {union}{}",
        UNION_LABEL,
        row_tail(log.prompts, &vendor_tag(all_vendors))
    );
    let _ = writeln!(out);
}

fn render_legend(out: &mut String, log: &DayLog) {
    let label_width = log
        .rows
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or_default();
    let identity_width = log
        .rows
        .iter()
        .map(|row| identity_of(row).chars().count())
        .max()
        .unwrap_or_default();
    let _ = writeln!(out, "repositories");
    for row in &log.rows {
        let workstream = match (&row.repository, &row.workstream) {
            // A row with no repository has no rule to match, so it is not
            // `unassigned` either -- that word means a rule is missing.
            (None, _) => "n/a".to_string(),
            (Some(_), Some(name)) => name.clone(),
            (Some(_), None) => UNASSIGNED.to_string(),
        };
        let _ = writeln!(
            out,
            "  {:label_width$}  {:identity_width$}  {workstream}",
            row.label,
            identity_of(row)
        );
    }
    let _ = writeln!(out);
}

/// What the legend shows a row stands for. A row with no repository has no identity
/// to show, and says so rather than showing an empty column.
fn identity_of(row: &RepositoryStrip) -> String {
    match &row.repository {
        Some(identity) => identity.clone(),
        None => "these observations carried no repository".to_string(),
    }
}

fn render_blocks(out: &mut String, log: &DayLog) {
    // The warning rides on the count rather than on the header alone. Attention is
    // measured in windows anchored on prompts, which overlap across blocks, so a
    // column of them is the one thing in this output a reader will try to add up --
    // and the paragraph that used to say so is behind `--explain` now.
    let _ = writeln!(
        out,
        "blocks        {} -- not additive; `cclogger report` has the day's total",
        log.blocks.len()
    );
    let _ = writeln!(out);
    let part_width = log
        .blocks
        .iter()
        .flat_map(|block| block.parts.iter().map(|part| part.label.chars().count()))
        .max()
        .unwrap_or_default();
    for block in &log.blocks {
        let _ = writeln!(
            out,
            "{:<11}  {:>6} elapsed  {:>6} attention (est.)  {:>4} prompts  {}",
            clock_range(block.span, log.tz_offset),
            hm(block.elapsed_seconds()),
            hm(block.attention_seconds),
            block.prompts,
            vendor_tag(&block.vendors)
        );
        // Said here rather than only in the header: `0m elapsed / 6m attention` reads
        // as a broken tool, and the reader meets it forty lines below the paragraph
        // that explains it. The two numbers measure different things -- one the span
        // the block's own observations cover, the other a fixed window around each
        // prompt in it -- and only the second can reach outside the block.
        if block.attention_seconds > block.elapsed_seconds() {
            let _ = writeln!(
                out,
                "  attention exceeds elapsed -- a prompt's [-{}m, +{}m] window reaches past this block",
                ATTENTION_BEFORE_SECONDS / 60,
                ATTENTION_AFTER_SECONDS / 60
            );
        }
        if block.named_repositories > 1 {
            let _ = writeln!(
                out,
                "  spans {} repositories -- one stretch of time, not one per repository",
                block.named_repositories
            );
        }
        for part in &block.parts {
            let _ = writeln!(
                out,
                "    {:part_width$}  {:>6} attention  {:>7}  {:>4} prompts  {:>6} observations",
                part.label,
                hm(part.attention_seconds),
                share(part.attention_seconds, block.attention_seconds),
                part.prompts,
                part.observations
            );
        }
        if !block.commits.is_empty() {
            let commits = block
                .commits
                .iter()
                .map(|(label, count)| format!("{label} {count}"))
                .collect::<Vec<_>>()
                .join(" \u{b7} ");
            // "landed in" rather than "produced": the block was running when the commit
            // was authored, which is all containment establishes.
            let _ = writeln!(out, "    commits  {commits}  (landed inside this block)");
        }
        if !block.tools.is_empty() {
            let tools = block
                .tools
                .iter()
                .map(|(family, count)| match family {
                    Some(name) => format!("{name} {count}"),
                    // A tool event whose adapter recorded no family. Named as the
                    // absence it is, not folded into `other`, which is a family an
                    // adapter chose on purpose.
                    None => format!("(family not recorded) {count}"),
                })
                .collect::<Vec<_>>()
                .join(" \u{b7} ");
            let _ = writeln!(out, "    tools  {tools}");
        }
        let _ = writeln!(out);
    }
    if log.commits_outside_blocks > 0 {
        let _ = writeln!(
            out,
            "{} commit(s) landed outside every block -- work that reached a repository while",
            log.commits_outside_blocks
        );
        let _ = writeln!(
            out,
            "this machine observed no session at all. They are not attached to a nearby block:"
        );
        let _ = writeln!(
            out,
            "which stretch of work a commit came out of is not something this ledger can say."
        );
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "elapsed spans a block's first observation to its last. It is not time spent: the"
    );
    let _ = writeln!(
        out,
        "{}m gap threshold admits silences of up to that long anywhere inside one block.",
        AGENT_GAP_SECONDS / 60
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::run_import;
    use crate::import::tests::{TempRoot, codex_deferred_first_flush, codex_forked_rollout};
    use crate::report::tests::{
        ACQUIRED_AT, Session, TZ, archive, archive_from, assistant_line, at, build, commits_in,
        cwd_for, day, prompt_line, time, window, write_config,
    };
    use crate::report::{Day, Period, RepositoryRow, run_report};
    use serde_json::json;

    /// The row for `label`, or a panic naming what the strip actually holds -- so a
    /// missing row fails as a missing row, never as a silently skipped assertion.
    fn row<'a>(log: &'a DayLog, label: &str) -> &'a RepositoryStrip {
        log.rows
            .iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no strip row labelled {label:?}; the strip holds {:?}",
                    log.rows.iter().map(|r| &r.label).collect::<Vec<_>>()
                )
            })
    }

    /// A synthetic assistant record carrying one `tool_use` block, which the importer
    /// turns into a `dev.cclog.tool.started.v1` observation with `name`'s family.
    fn tool_line(session: &str, uuid: &str, time: &str, cwd: &str, name: &str) -> String {
        json!({
            "type": "assistant",
            "uuid": uuid,
            "sessionId": session,
            "timestamp": time,
            "cwd": cwd,
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": format!("toolu_{uuid}"),
                    "name": name,
                    "input": { "SYNTHETIC": true },
                }],
            },
        })
        .to_string()
    }

    /// A minimal Codex transcript: the `session_meta` that names the session and its
    /// `cwd` -- Codex prompt records carry neither -- then one human prompt.
    /// Shaped after `adapters/codex/shapes/*.shape.json`; every value is synthetic.
    fn codex_lines(cwd: &str, prompt_at: i64) -> Vec<String> {
        vec![
            json!({
                "type": "session_meta",
                "timestamp": at(prompt_at - 1),
                "payload": {
                    "id": "ffffffff-6666-4666-8666-666666666666",
                    "session_id": "22222222-2222-4222-8222-222222222222",
                    "cwd": cwd,
                    "cli_version": "0.144.2",
                    "originator": "cli",
                    "source": "interactive",
                },
            })
            .to_string(),
            json!({
                "type": "event_msg",
                "timestamp": at(prompt_at),
                "payload": {
                    "type": "user_message",
                    "message": "SYNTHETIC prompt",
                    "client_id": "SYNTHETIC-client-a",
                    "local_images": [],
                },
            })
            .to_string(),
        ]
    }

    /// The fixture the plan calls for: two repositories overlapping inside one hour,
    /// and a third separated from them by more than the gap threshold. The third has
    /// agent activity and no prompt at all.
    fn overlapping_day(root: &std::path::Path) {
        build(
            root,
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[50_400, 50_700],
                    agent: &[50_400, 50_500, 50_800],
                },
                Session {
                    // The second prompt is deliberately in a later cell than every
                    // other prompt here, and still inside the same block: without a
                    // prompt that lands somewhere else, a marker row that put every
                    // mark in one column would satisfy every assertion below.
                    repository: "github.com/acme/web",
                    prompts: &[50_600, 51_100],
                    agent: &[50_600, 50_900],
                },
                Session {
                    repository: "github.com/acme/notes",
                    prompts: &[],
                    agent: &[61_200, 61_260],
                },
            ],
        );
    }

    #[test]
    fn work_in_two_repositories_at_once_is_one_block_and_work_hours_apart_is_another() {
        let root = TempRoot::new("log-overlap");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        assert_eq!(
            log.blocks.len(),
            2,
            "the two overlapping repositories share a block; the third is its own"
        );
        let first: Vec<Option<&str>> = log.blocks[0]
            .parts
            .iter()
            .map(|p| p.repository.as_deref())
            .collect();
        assert_eq!(
            first,
            vec![Some("github.com/acme/api"), Some("github.com/acme/web")],
            "concurrent work stays one stretch of time, with both repositories inside it"
        );
        assert_eq!(log.blocks[0].named_repositories, 2);
        assert_eq!(
            log.blocks[1]
                .parts
                .iter()
                .map(|p| p.repository.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("github.com/acme/notes")],
        );
        assert_eq!(
            log.blocks[1].named_repositories, 1,
            "and a block covering one repository is not marked as spanning several"
        );
    }

    /// Every hour mark the rendered ruler draws, as `(column, hour)`.
    ///
    /// The ruler is the first line after the `strip` header whose content starts with
    /// a digit -- the density rows start with a label, the marker rows with `▲`, and
    /// the header's own continuation lines with letters or the ramp.
    fn ruler_marks(rendered: &str) -> Vec<(usize, i64)> {
        let ruler = rendered
            .lines()
            .skip_while(|line| !line.starts_with("strip "))
            .find(|line| {
                line.starts_with(' ') && line.trim_start().starts_with(|c: char| c.is_ascii_digit())
            })
            .unwrap_or_else(|| panic!("the strip draws no hour ruler:\n{rendered}"));

        let chars: Vec<char> = ruler.chars().collect();
        let mut marks = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if !chars[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            let column = i;
            let mut hour = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                hour.push(chars[i]);
                i += 1;
            }
            assert_eq!(
                hour.len(),
                2,
                "an hour mark is two digits; {hour:?} means two labels ran together \
                 with no gap, and no reader can tell where an hour begins:\n{rendered}"
            );
            marks.push((column, hour.parse().expect("an hour")));
        }
        marks
    }

    /// The characters one rendered strip row draws inside the axis.
    ///
    /// The axis starts where the ruler's first hour mark does -- the drawn range is
    /// snapped to an hour, so column 0 always carries a mark -- which lets this read
    /// the render instead of recomputing the renderer's own gutter from the model.
    fn drawn_cells(rendered: &str, log: &DayLog, label: &str) -> Vec<char> {
        let gutter = ruler_marks(rendered)
            .first()
            .expect("the ruler labels at least the first hour")
            .0;
        let line = rendered
            .lines()
            .find(|line| line.starts_with(label) && line.contains(" prompts"))
            .unwrap_or_else(|| panic!("no strip row labelled {label:?} in:\n{rendered}"));
        line.chars()
            .skip(gutter)
            .take(log.last_bucket - log.first_bucket + 1)
            .collect()
    }

    /// The columns of a rendered row that are not blank.
    fn drawn_columns(rendered: &str, log: &DayLog, label: &str) -> Vec<usize> {
        drawn_cells(rendered, log, label)
            .into_iter()
            .enumerate()
            .filter(|(_, cell)| *cell != ' ')
            .map(|(column, _)| column)
            .collect()
    }

    /// Check that the geometry the header states is the geometry the strip drew.
    ///
    /// A reader takes every distance off this axis using the stated cell size, so a
    /// header that names one resolution while the cells use another makes each of
    /// those distances wrong without looking wrong. The ruler is built by a separate
    /// loop from the header line, so agreeing with it is a real constraint rather
    /// than a restatement of one field.
    fn assert_the_header_describes_the_drawn_strip(rendered: &str, log: &DayLog) {
        let header = rendered
            .lines()
            .find(|line| line.starts_with("strip "))
            .unwrap_or_else(|| panic!("the strip states no geometry:\n{rendered}"));
        let stated_minutes: i64 = header
            .split_once("1 cell = ")
            .and_then(|(_, rest)| rest.split_once(" min"))
            .map(|(minutes, _)| minutes)
            .unwrap_or_else(|| panic!("the header states no cell size:\n{header}"))
            .parse()
            .expect("a whole number of minutes");
        assert_eq!(
            stated_minutes * 60,
            log.bucket_seconds,
            "the header states {stated_minutes}min cells while the strip is drawn with \
             {}s ones:\n{header}",
            log.bucket_seconds
        );

        let marks = ruler_marks(rendered);
        assert!(
            marks.len() >= 2,
            "at least two hour marks, or their spacing tests nothing:\n{rendered}"
        );
        let stated_start: i64 = header
            .split_once(", ")
            .and_then(|(_, rest)| rest.split_once(':'))
            .map(|(hour, _)| hour)
            .unwrap_or_else(|| panic!("the header states no span:\n{header}"))
            .parse()
            .expect("an hour");
        assert_eq!(
            marks[0].1, stated_start,
            "the ruler's first mark must name the hour the header says the axis starts \
             at:\n{rendered}"
        );
        for pair in marks.windows(2) {
            assert_eq!(
                (pair[1].0 - pair[0].0) as i64,
                (pair[1].1 - pair[0].1) * (60 / stated_minutes),
                "hours {} and {} sit {} columns apart, which is not what {stated_minutes}min \
                 cells imply:\n{rendered}",
                pair[0].1,
                pair[1].1,
                pair[1].0 - pair[0].0
            );
        }
    }

    #[test]
    fn the_strip_draws_each_repositorys_observations_in_the_cells_they_fell_in() {
        // The headline output of this milestone, and the one thing no assertion
        // reached before: every row's density is checked on the *rendered* line, so a
        // strip that draws activity as idleness -- or draws nothing at all -- fails
        // here rather than passing on the totals beside it.
        let root = TempRoot::new("log-drawn-cells");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let rendered = render(&log, Header::Brief);

        let column = |offset: i64| (offset / log.bucket_seconds) as usize - log.first_bucket;
        assert_eq!(
            drawn_columns(&rendered, &log, "api"),
            vec![column(50_400)],
            "api's five observations all fall in one cell, and it is drawn:\n{rendered}"
        );
        assert_eq!(
            drawn_columns(&rendered, &log, "web"),
            vec![column(50_600), column(51_100)],
            "web's straddle a cell boundary, and both cells are drawn:\n{rendered}"
        );
        assert_eq!(
            drawn_columns(&rendered, &log, "notes"),
            vec![column(61_200)],
            "and the repository three hours later is drawn there, not beside them:\n{rendered}"
        );
        assert_eq!(
            drawn_columns(&rendered, &log, UNION_LABEL),
            vec![column(50_400), column(51_100), column(61_200)],
            "the union row draws every cell any repository drew -- it is the overlap \
             this view exists to show, and a blank one says the day was idle:\n{rendered}"
        );
        // The columns above must be distinct, or "drawn where they fell" is satisfied
        // by a strip that puts everything in the same place.
        assert_ne!(column(50_600), column(51_100));
        assert_ne!(column(51_100), column(61_200));
    }

    #[test]
    fn the_strip_is_trimmed_to_the_active_span_and_still_covers_its_two_ends() {
        // Two-sided on purpose. "Covers both ends" alone is satisfied by drawing the
        // whole day, which is the thing the trim exists to avoid; "is trimmed" alone
        // is satisfied by dropping hours, which is the thing the plan forbids.
        let root = TempRoot::new("log-active-span");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        let cell_of = |offset: i64| (offset / log.bucket_seconds) as usize;
        assert!(
            log.first_bucket <= cell_of(50_400) && cell_of(61_260) <= log.last_bucket,
            "the drawn range {}..={} must contain the first and last observation \
             (cells {} and {})",
            log.first_bucket,
            log.last_bucket,
            cell_of(50_400),
            cell_of(61_260)
        );
        assert!(
            log.first_bucket > 0,
            "the hours before the first observation are trimmed away, not drawn empty"
        );
        assert!(
            log.last_bucket < log.buckets_per_day - 1,
            "and so are the hours after the last one"
        );
    }

    #[test]
    fn a_repository_with_observations_but_no_prompts_still_gets_a_strip_row() {
        // Attention 0 ("no prompt to anchor a window") and coverage 0 ("nothing was
        // observed") are different statements, and dropping the row conflates them.
        let root = TempRoot::new("log-no-prompts");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let rendered = render(&log, Header::Brief);

        let notes = row(&log, "notes");
        assert_eq!(notes.total_prompts, 0);
        assert_eq!(
            notes.total_observations, 2,
            "its agent activity is real and must still be drawn"
        );
        // On the *drawn* row, not on the full-day vector behind it. Summing the vector
        // counts cells the strip never prints, so it stays true for a row that renders
        // blank -- which is the whole claim this assertion's message makes.
        assert!(
            drawn_cells(&rendered, &log, "notes")
                .iter()
                .any(|cell| *cell != ' '),
            "it must reach the strip's drawn cells, not just its totals:\n{rendered}"
        );
        // Without this the test would pass against a strip that lost every prompt:
        // "0 prompts" is not evidence of anything if nothing has any.
        assert!(
            log.rows.iter().any(|r| r.total_prompts > 0),
            "the same strip must show prompts where there were prompts"
        );
    }

    #[test]
    fn a_blocks_attention_shares_sum_to_the_block_rather_than_exceeding_it() {
        let root = TempRoot::new("log-block-shares");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        let block = &log.blocks[0];
        let allocated: i64 = block.parts.iter().map(|p| p.attention_seconds).sum();
        assert_eq!(
            allocated, block.attention_seconds,
            "every second of the block's attention goes to exactly one repository"
        );
        // The fixture has to overlap, or "the shares sum" holds for the trivial
        // reason that no two windows ever met -- which the partition cannot get
        // wrong and this assertion would not be testing.
        let alone: i64 =
            block.prompts as i64 * (ATTENTION_BEFORE_SECONDS + ATTENTION_AFTER_SECONDS);
        assert!(
            block.prompts > 1 && block.attention_seconds < alone,
            "the block's {} prompt windows must overlap ({}s union vs {alone}s apart)",
            block.prompts,
            block.attention_seconds
        );
        assert!(
            block.parts.iter().all(|p| p.attention_seconds > 0),
            "and both repositories must come out of the partition with a share"
        );
    }

    /// One rendered block's stanza: its header line and every line indented under it,
    /// up to the blank line that ends it. Lets a test say "this block carries the
    /// mark and that one does not", which no whole-output `contains` can.
    fn block_stanzas(rendered: &str) -> Vec<Vec<&str>> {
        let mut stanzas: Vec<Vec<&str>> = Vec::new();
        let mut current: Option<Vec<&str>> = None;
        for line in rendered.lines() {
            if line.starts_with(|c: char| c.is_ascii_digit()) && line.contains(" elapsed  ") {
                if let Some(done) = current.take() {
                    stanzas.push(done);
                }
                current = Some(vec![line]);
            } else if line.trim().is_empty() {
                if let Some(done) = current.take() {
                    stanzas.push(done);
                }
            } else if let Some(open) = current.as_mut() {
                open.push(line);
            }
        }
        if let Some(done) = current {
            stanzas.push(done);
        }
        stanzas
    }

    /// A day holding one block of each kind the attention/elapsed note turns on.
    ///
    /// 1. A single prompt: its `[-1m, +5m]` window is six minutes of attention over
    ///    zero elapsed, so the note belongs on it.
    /// 2. Prompts far apart with agent records bridging them: elapsed outruns
    ///    attention, and the note does not belong.
    /// 3. One prompt and agent records reaching exactly to the end of its window:
    ///    attention *equals* elapsed. Nothing disagrees, so nothing is explained --
    ///    and this is the case that tells `>` from `>=`.
    fn blocks_on_each_side_of_the_attention_note(root: &std::path::Path) {
        build(
            root,
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[50_400, 61_200, 61_800, 75_600],
                agent: &[
                    61_200, 61_400, 61_600, 61_800, 62_000, 62_200, 62_400, // block 2
                    75_600, 75_900, 75_960, // block 3: 360s elapsed, 360s window
                ],
            }],
        );
    }

    #[test]
    fn a_block_whose_attention_outruns_its_elapsed_says_why_on_its_own_row() {
        // `0m elapsed / 6m attention` reads as a broken tool. The header explains it,
        // forty lines earlier, which is not where the reader meets it.
        let root = TempRoot::new("log-attention-spill");
        blocks_on_each_side_of_the_attention_note(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let rendered = render(&log, Header::Brief);

        // The fixture must hold one block of each kind, or the assertions below
        // degenerate into "the note is printed" / "the note is not printed".
        assert_eq!(log.blocks.len(), 3);
        assert!(
            log.blocks[0].attention_seconds > log.blocks[0].elapsed_seconds(),
            "the first block's attention must outrun its elapsed: {:?}",
            log.blocks[0]
        );
        assert!(
            log.blocks[1].attention_seconds < log.blocks[1].elapsed_seconds(),
            "the second block's must fall short of it: {:?}",
            log.blocks[1]
        );
        assert_eq!(
            log.blocks[2].attention_seconds,
            log.blocks[2].elapsed_seconds(),
            "and the third's must be exactly equal, which is what tells `>` from `>=`"
        );

        let stanzas = block_stanzas(&rendered);
        assert_eq!(stanzas.len(), 3, "three block stanzas:\n{rendered}");
        let marked = |stanza: &Vec<&str>| {
            stanza
                .iter()
                .filter(|line| line.trim_start().starts_with("attention exceeds elapsed"))
                .count()
        };
        assert_eq!(
            marked(&stanzas[0]),
            1,
            "the block whose attention outruns its elapsed carries the note:\n{rendered}"
        );
        assert_eq!(
            marked(&stanzas[1]),
            0,
            "the block whose does not, does not -- a note printed always explains \
             nothing:\n{rendered}"
        );
        assert_eq!(
            marked(&stanzas[2]),
            0,
            "and neither does the block whose two numbers agree: there is nothing there \
             to explain:\n{rendered}"
        );

        // The note has to give the reason, not restate the two numbers: a reader who
        // only learns that they differ is exactly as stuck as before.
        let note = stanzas[0]
            .iter()
            .find(|line| line.trim_start().starts_with("attention exceeds elapsed"))
            .expect("the note is in this stanza");
        assert!(
            note.contains("window") && note.contains("past this block"),
            "the note must say why the two can disagree:\n{note}"
        );
    }

    #[test]
    fn a_block_that_began_and_ended_in_one_minute_prints_that_time_once() {
        // `22:43-22:43` reads as a formatting failure rather than as the instant it
        // is. Two prompts in the same second are real -- 145 collisions in the
        // measured corpus -- and `elapsed` beside it already carries the duration.
        let root = TempRoot::new("log-instant-block");
        blocks_on_each_side_of_the_attention_note(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let rendered = render(&log, Header::Brief);
        let stanzas = block_stanzas(&rendered);

        assert_eq!(log.blocks[0].elapsed_seconds(), 0);
        assert!(
            stanzas[0][0].starts_with("14:00 "),
            "an instant is printed once:\n{}",
            stanzas[0][0]
        );
        assert!(
            !stanzas[0][0].contains("14:00-14:00"),
            "and never as a zero-width range:\n{}",
            stanzas[0][0]
        );
        // The other direction, or "always print one time" would pass: a block that
        // really does span minutes must still show both ends.
        assert!(
            log.blocks[1].elapsed_seconds() > 60,
            "the second block has to span more than a minute for this to test anything"
        );
        assert!(
            stanzas[1][0].starts_with("17:00-17:20 "),
            "a block that spans minutes keeps both ends:\n{}",
            stanzas[1][0]
        );
    }

    #[test]
    fn the_block_list_marks_a_block_that_spans_more_than_one_repository() {
        let root = TempRoot::new("log-render-spans");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let rendered = render(&log, Header::Brief);

        // Counted, not merely present: the fixture has one multi-repository block and
        // one single-repository block, so a mark printed unconditionally -- or one
        // printed for neither -- fails here. `contains` would pass for both.
        let marks = rendered
            .lines()
            .filter(|line| line.trim_start().starts_with("spans "))
            .collect::<Vec<_>>();
        assert_eq!(
            marks.len(),
            1,
            "exactly one of the two blocks spans several repositories:\n{rendered}"
        );
        assert!(
            marks[0].contains("spans 2 repositories"),
            "and the mark says how many:\n{rendered}"
        );
    }

    // -- commits: what came of a block, and what came of nothing --------------------

    #[test]
    fn a_commit_inside_a_block_is_attached_to_it() {
        // The one thing this view could not say before: a block ran, and something
        // landed while it did.
        let root = TempRoot::new("log-commit-in-block");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[3660, 3720],
            }],
        );
        let _home = commits_in(root.path(), "github.com/acme/api", &[3700]);

        let log = run_log(root.path(), window(), TZ).expect("draw the day");
        assert_eq!(log.commits, 1);
        assert_eq!(log.commits_outside_blocks, 0);
        assert_eq!(log.blocks.len(), 1);
        assert_eq!(
            log.blocks[0].commits,
            vec![("api".to_string(), 1)],
            "attached to the block, and labelled the way its parts are"
        );
        assert!(
            render(&log, Header::Brief).contains("commits  api 1"),
            "and printed with the block: {}",
            render(&log, Header::Brief)
        );
    }

    #[test]
    fn a_commit_in_a_silence_is_reported_outside_the_blocks_rather_than_snapped_to_one() {
        // A commit between two blocks is evidence that work landed while this machine
        // saw nothing. Attaching it to whichever block is nearer would turn "I cannot
        // say which stretch of work this came from" into a printed claim that I can.
        let root = TempRoot::new("log-commit-outside");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600, 36_000],
                agent: &[3660, 3720, 36_060, 36_120],
            }],
        );
        // Squarely in the silence: hours from either block, and inside neither.
        let _home = commits_in(root.path(), "github.com/acme/api", &[20_000]);

        let log = run_log(root.path(), window(), TZ).expect("draw the day");
        assert_eq!(
            log.blocks.len(),
            2,
            "two blocks, and a long gap between them"
        );
        assert_eq!(log.commits, 1);
        assert_eq!(
            log.commits_outside_blocks, 1,
            "the commit belongs to neither block"
        );
        assert!(log.blocks[0].commits.is_empty());
        assert!(log.blocks[1].commits.is_empty());
        let rendered = render(&log, Header::Brief);
        assert!(
            rendered.contains("landed outside every block"),
            "and it is said rather than dropped: {rendered}"
        );
    }

    #[test]
    fn a_commit_neither_lights_a_strip_cell_nor_stretches_a_block() {
        // A commit is an instant. On the strip it would be a cell claiming activity in a
        // minute that held only a `git commit`; in the clustering it would extend a
        // block's span to it, or create a block one instant long with no work in it.
        let root = TempRoot::new("log-commit-not-a-clock");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[3600],
                agent: &[3660, 3720],
            }],
        );
        let before = run_log(root.path(), window(), TZ).expect("draw the day");
        // 3900 is inside the gap threshold but past the block's last observation, so
        // admitted to the clustering it would stretch the span; 50000 is an empty hour,
        // where it would become a block of its own with no work in it.
        let _home = commits_in(root.path(), "github.com/acme/api", &[3900, 50_000]);
        let after = run_log(root.path(), window(), TZ).expect("draw the day again");

        assert_eq!(after.commits, 2, "both commits are in the window");
        assert_eq!(
            after.blocks.len(),
            before.blocks.len(),
            "a commit in an empty hour must not become a block of its own"
        );
        assert_eq!(after.blocks[0].span, before.blocks[0].span);
        assert_eq!(
            after.blocks[0].attention_seconds,
            before.blocks[0].attention_seconds
        );
        assert_eq!(after.blocks[0].prompts, before.blocks[0].prompts);
        assert_eq!(
            row(&after, "api").observations,
            row(&before, "api").observations,
            "and lights no cell on the strip"
        );
        assert_eq!(
            row(&after, "api").total_observations,
            row(&before, "api").total_observations
        );
        assert_eq!(after.union_row, before.union_row);
    }

    #[test]
    fn every_repository_gets_a_row_and_the_legend_resolves_its_shortened_label() {
        // The strip labels rows with a suffix of the identity so the axis fits. That
        // is only honest if the full identity is printed somewhere.
        let root = TempRoot::new("log-legend");
        write_config(
            root.path(),
            "[[rule]]\nmatch = \"github.com/acme/api\"\nworkstream = \"acme\"\n",
        );
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let rendered = render(&log, Header::Brief);

        assert_eq!(
            row(&log, "api").repository.as_deref(),
            Some("github.com/acme/api")
        );
        for row in &log.rows {
            let identity = row.repository.as_deref().expect("a named repository");
            assert!(
                rendered.lines().any(|line| {
                    let mut fields = line.split_whitespace();
                    fields.next() == Some(row.label.as_str()) && fields.next() == Some(identity)
                }),
                "the legend must resolve {} to {identity}:\n{rendered}",
                row.label
            );
        }
        assert_eq!(row(&log, "api").workstream.as_deref(), Some("acme"));
        assert_eq!(
            row(&log, "web").workstream,
            None,
            "a repository no rule matches is unassigned, not folded into the one that matched"
        );
    }

    #[test]
    fn two_repositories_sharing_a_last_segment_are_both_labelled_by_more_of_their_path() {
        // `acme/api` and `other/api` both end in `api`. A label that collided would
        // put two repositories' work on one row, or make the legend ambiguous.
        let root = TempRoot::new("log-label-collision");
        build(
            root.path(),
            &[
                Session {
                    repository: "github.com/acme/api",
                    prompts: &[50_400],
                    agent: &[50_400],
                },
                Session {
                    repository: "github.com/other/api",
                    prompts: &[50_500],
                    agent: &[50_500],
                },
            ],
        );

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        let labels: BTreeSet<&str> = log.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            BTreeSet::from(["acme/api", "other/api"]),
            "the shortest suffix that tells them apart, not the ambiguous one"
        );
        assert_eq!(log.rows.len(), 2, "and they stay two rows");
    }

    #[test]
    fn a_day_too_wide_for_the_terminal_widens_the_cell_instead_of_cutting_the_axis() {
        let root = TempRoot::new("log-wide-day");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[0],
                agent: &[86_340],
            }],
        );

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        assert!(
            log.bucket_seconds > 600,
            "a full day at 10-minute cells is 144 columns; the cell must widen"
        );
        assert_eq!(log.first_bucket, 0, "and the axis still starts at 00:00");
        assert_eq!(
            log.last_bucket,
            log.buckets_per_day - 1,
            "and still ends at the last observation's hour"
        );
        let rendered = render(&log, Header::Brief);
        let widest = strip_lines(&rendered)
            .map(|line| line.chars().count())
            .max()
            .expect("the strip has lines");
        assert!(
            widest <= TARGET_COLUMNS,
            "the widened strip fits {TARGET_COLUMNS} columns, but a line was {widest}:\n{rendered}"
        );
        // A widened cell is the case where a header still claiming ten minutes does
        // the most damage: every distance a reader takes off this axis would be out
        // by the ratio between the two.
        assert_the_header_describes_the_drawn_strip(&rendered, &log);
    }

    #[test]
    fn a_day_that_fits_keeps_the_finest_cell_rather_than_widening_for_no_reason() {
        // The other half of the rule above: widening is a response to not fitting,
        // not the default. Without this, "always use the coarsest cell" would pass.
        let root = TempRoot::new("log-narrow-day");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        assert_eq!(
            log.bucket_seconds, 600,
            "four hours of activity fits at 10-minute cells"
        );
        assert_the_header_describes_the_drawn_strip(&render(&log, Header::Brief), &log);
    }

    /// The lines the strip itself draws -- ruler, rows and prompt markers -- which are
    /// the ones the width rule is about. The header and the legend may run wider.
    fn strip_lines(rendered: &str) -> impl Iterator<Item = &str> {
        rendered
            .lines()
            .skip_while(|line| !line.starts_with("strip "))
            .take_while(|line| !line.starts_with("repositories"))
    }

    #[test]
    fn the_log_counts_the_same_prompts_in_the_same_day_the_report_does() {
        // Local midnight belongs to the day; the next local midnight does not. The
        // ledger stores `…T15:00:00.000Z` for the second one and `.` sorts before
        // `Z`, so a bound written as `< …T15:00:00Z` silently lets it in. Two views
        // of one day that disagreed about which observations are in it would be worse
        // than either alone, so this pins them together on the case that catches it.
        let root = TempRoot::new("log-day-boundary");
        build(
            root.path(),
            &[Session {
                repository: "github.com/acme/api",
                prompts: &[0, 86_400],
                agent: &[600],
            }],
        );

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let report =
            run_report(root.path(), Period::of_day(day()), TZ).expect("report the same day");

        assert_eq!(
            log.prompts, 1,
            "the prompt at the next local midnight is not this day's"
        );
        assert_eq!(
            log.prompts, report.prompts,
            "and the two views must not disagree about that"
        );
        assert_eq!(
            log.observations_in_window, report.coverage.observations_in_window,
            "nor about how much of the day they saw at all"
        );
        // The parser is `report::Day::parse`, the same one the report's `--day` uses;
        // `main` gives `cclogger log` no other way in. Restated here because a second
        // parser is exactly the drift this test exists to prevent.
        assert!(
            Day::parse("2026-02-30").is_err(),
            "a date the calendar does not have is refused before any of this runs"
        );
    }

    #[test]
    fn an_offset_no_timezone_uses_is_refused_by_the_log_too() {
        let root = TempRoot::new("log-offset");
        overlapping_day(root.path());

        assert!(
            matches!(
                run_log(root.path(), window(), TzOffset::from_hours(999)),
                Err(ReportError::InvalidOffset(_))
            ),
            "an offset no zone on earth uses would cut the day somewhere meaningless"
        );
        assert!(
            run_log(root.path(), window(), TzOffset::from_hours(14)).is_ok(),
            "+14:00 is a real offset"
        );
    }

    #[test]
    fn a_blocks_tool_families_are_counted_from_the_tool_events_inside_it() {
        let root = TempRoot::new("log-tools");
        let session = "0000000f-1111-4111-8111-111111111111";
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/tools.jsonl",
            &[
                prompt_line(session, "p-0", &at(50_400), &cwd),
                tool_line(session, "t-0", &at(50_410), &cwd, "Bash"),
                tool_line(session, "t-1", &at(50_420), &cwd, "Edit"),
                tool_line(session, "t-2", &at(50_430), &cwd, "Write"),
                // Hours later, past the gap threshold: its own block, and its own
                // tally. A tally taken day-wide would put this one in both.
                tool_line(session, "t-3", &at(61_200), &cwd, "Read"),
            ],
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        assert_eq!(log.blocks.len(), 2, "the fixture must produce two blocks");
        assert_eq!(
            log.blocks[0].tools,
            vec![
                (Some("edit".to_string()), 2),
                (Some("shell".to_string()), 1)
            ],
            "Edit and Write are one family; the heavier family leads"
        );
        assert_eq!(
            log.blocks[1].tools,
            vec![(Some("read".to_string()), 1)],
            "and the later block carries only its own tool call"
        );
    }

    #[test]
    fn observations_that_carried_no_repository_get_their_own_row_and_sort_last() {
        // Not "unassigned", which means a rule is missing: these records never had a
        // repository to assign. Folding them into a named row would let a coverage
        // problem hide inside a configuration one.
        let root = TempRoot::new("log-no-repository");
        let session = "00000010-1111-4111-8111-111111111111";
        let inside = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/mixed.jsonl",
            &[
                prompt_line(session, "p-0", &at(50_400), &inside),
                assistant_line(session, "a-0", &at(50_410), &inside),
                // Outside any ghq tree: the importer resolves no repository for it.
                // Two of them, so the residual weighs exactly as much as the
                // repository -- with the counts unequal, weight alone would decide
                // the order and the tie-break below would never be exercised.
                assistant_line(session, "a-1", &at(50_420), "/SYNTHETIC/elsewhere"),
                assistant_line(session, "a-2", &at(50_430), "/SYNTHETIC/elsewhere"),
            ],
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        let unattributed = row(&log, NO_REPOSITORY_LABEL);
        assert_eq!(unattributed.repository, None);
        assert_eq!(
            unattributed.total_observations,
            row(&log, "api").total_observations,
            "the fixture must tie, or nothing below tests the tie-break"
        );
        assert_eq!(
            log.rows
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            vec!["api", NO_REPOSITORY_LABEL],
            "the residual is the last strip row, not the first"
        );
        assert_eq!(
            log.blocks[0]
                .parts
                .iter()
                .map(|p| p.repository.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("github.com/acme/api"), None],
            "and the last part of the block, for the same reason"
        );
        let total: u64 = log.rows.iter().map(|r| r.total_observations).sum();
        assert_eq!(
            total, 4,
            "and every observation in the day is on exactly one row"
        );

        // The residual is a part of the block, but it is not a repository the block
        // spans. Counting it would print "spans 2 repositories" over a block that
        // touched one -- the very claim that mark exists to make truthfully.
        let rendered = render(&log, Header::Brief);
        assert_eq!(
            log.blocks[0].named_repositories, 1,
            "one repository plus a residual is one repository"
        );
        assert!(
            !rendered
                .lines()
                .any(|line| line.trim_start().starts_with("spans ")),
            "so no block here is marked as spanning several:\n{rendered}"
        );

        // `n/a`, never `unassigned`. `unassigned` means no rule matched a repository;
        // these observations had no repository for a rule to match. Conflating them
        // lets a coverage problem hide inside a configuration one -- and `api` in this
        // same fixture *is* unassigned, so the two words have to appear on different
        // rows for the distinction to be visible at all.
        // Scoped to the legend section: `(none)` also opens a strip row, and matching
        // that one instead would test the vendor tag while claiming to test the
        // workstream column.
        let legend_row = |label: &str| -> &str {
            rendered
                .lines()
                .skip_while(|line| *line != "repositories")
                .skip(1)
                .take_while(|line| !line.trim().is_empty())
                .find(|line| line.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("no legend row for {label:?} in:\n{rendered}"))
        };
        let residual = legend_row(NO_REPOSITORY_LABEL);
        assert!(
            residual.ends_with("n/a"),
            "the residual's workstream column is n/a:\n{residual}"
        );
        assert!(
            !residual.contains(UNASSIGNED),
            "and never `unassigned`, which would name a rule it could never have had:\n{residual}"
        );
        assert!(
            residual.contains("carried no repository"),
            "and its identity column says why it has no identity:\n{residual}"
        );
        assert!(
            legend_row("api").ends_with(UNASSIGNED),
            "while a repository no rule matched is exactly `unassigned`:\n{rendered}"
        );
    }

    #[test]
    fn a_repository_both_vendors_observed_names_both_of_them() {
        let root = TempRoot::new("log-two-vendors");
        let cwd = cwd_for("github.com/acme/api");
        archive(
            root.path(),
            ".claude/projects/synthetic/both.jsonl",
            &[prompt_line(
                "00000011-1111-4111-8111-111111111111",
                "p-0",
                &at(50_400),
                &cwd,
            )],
            ACQUIRED_AT,
        );
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/synthetic.jsonl",
            &codex_lines(&cwd, 50_500),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        assert_eq!(
            row(&log, "api").vendors,
            vec!["claude-code".to_string(), "codex".to_string()],
            "both vendors observed this repository, in the ledger's own spelling"
        );
        assert_eq!(
            log.blocks[0].vendors,
            vec!["claude-code".to_string(), "codex".to_string()],
            "and the block they share names both too"
        );
    }

    #[test]
    fn the_union_rows_vendor_tag_names_every_vendor_and_the_axis_is_sized_to_fit_it() {
        // The union row lists every vendor of the day, so its tail is longer than any
        // single repository's whenever no one repository saw them all. A cell size
        // chosen from the repository rows alone lets exactly that one line overrun.
        // The span below is eleven hours: 66 cells at 10 minutes, which fits the
        // budget measured without the union row and does not fit the real one.
        let root = TempRoot::new("log-union-width");
        archive(
            root.path(),
            ".claude/projects/synthetic/claude-only.jsonl",
            &[prompt_line(
                "00000012-1111-4111-8111-111111111111",
                "p-0",
                &at(32_400),
                &cwd_for("github.com/acme/api"),
            )],
            ACQUIRED_AT,
        );
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/codex-only.jsonl",
            &codex_lines(&cwd_for("github.com/acme/web"), 71_940),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let rendered = render(&log, Header::Brief);

        // The fixture has to split the vendors between repositories, or the union tag
        // is no longer than a row's and nothing below distinguishes the two budgets.
        let union_tail = row_tail(log.prompts, "claude-code+codex").chars().count();
        for row in &log.rows {
            assert!(
                row_tail(row.total_prompts, &vendor_tag(&row.vendors))
                    .chars()
                    .count()
                    < union_tail,
                "{} already carries every vendor; the union row would not be the widest",
                row.label
            );
        }
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with(UNION_LABEL) && line.ends_with("claude-code+codex")),
            "the union row must carry the combined tag:\n{rendered}"
        );
        let widest = strip_lines(&rendered)
            .map(|line| line.chars().count())
            .max()
            .expect("the strip has lines");
        assert!(
            widest <= TARGET_COLUMNS,
            "the widest strip line is {widest} columns:\n{rendered}"
        );
    }

    #[test]
    fn a_day_with_no_observation_says_so_instead_of_drawing_an_empty_axis() {
        let root = TempRoot::new("log-empty-day");
        overlapping_day(root.path());

        let earlier = Day::parse("2026-07-20").expect("a well-formed day");
        let log = run_log(
            root.path(),
            DayWindow::new(earlier, None, None).expect("the whole of a day"),
            TZ,
        )
        .expect("log the earlier day");
        let rendered = render(&log, Header::Brief);

        assert!(log.rows.is_empty());
        assert!(log.blocks.is_empty());
        assert!(
            rendered.contains("there is no axis to draw"),
            "an empty strip would read as a day of idleness:\n{rendered}"
        );
        assert_eq!(
            strip_lines(&rendered).count(),
            0,
            "and no ruler or row is printed at all:\n{rendered}"
        );

        // Without this, the same assertions would pass against a `run_log` that never
        // drew anything for any day.
        let observed = run_log(root.path(), window(), TZ).expect("log the fixture's own day");
        assert!(!observed.rows.is_empty());
        assert!(strip_lines(&render(&observed, Header::Brief)).count() > 0);
    }

    #[test]
    fn a_repositorys_prompt_markers_land_in_the_cells_its_prompts_fell_in() {
        let root = TempRoot::new("log-markers");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        let cell = |offset: i64| (offset / log.bucket_seconds) as usize;
        let occupied = |strip: &RepositoryStrip| -> Vec<(usize, u64)> {
            strip
                .prompts
                .iter()
                .enumerate()
                .filter(|(_, n)| **n > 0)
                .map(|(i, n)| (i, *n))
                .collect()
        };

        // Whole vectors, not spot checks: a marker in an idle cell is a claim about
        // minutes nothing happened in, and only listing every non-empty cell catches
        // one. `api`'s two prompts are five minutes apart and share a cell; `web`'s
        // are eight minutes apart and do not.
        assert_eq!(
            occupied(row(&log, "api")),
            vec![(cell(50_400), 2)],
            "both of api's prompts fall in the one cell that covers 14:00-14:10"
        );
        assert_eq!(
            occupied(row(&log, "web")),
            vec![(cell(50_600), 1), (cell(51_100), 1)],
            "web's land in two different cells, in its own row rather than api's"
        );
        assert_ne!(
            cell(50_600),
            cell(51_100),
            "the fixture must straddle a cell boundary, or nothing above tests placement"
        );
        assert!(
            occupied(row(&log, "notes")).is_empty(),
            "and a repository with no prompt gets no marker at all"
        );
    }

    #[test]
    fn density_scales_to_the_busiest_cell_and_never_draws_an_observation_as_nothing() {
        assert_eq!(density(0, 10), ' ', "an empty cell is the only blank one");
        assert_eq!(
            density(1, 1000),
            '\u{2581}',
            "one observation against a huge peak still shows, or the strip would \
             report activity as idleness"
        );
        assert_eq!(density(10, 10), '\u{2588}', "the peak fills the cell");
        assert_eq!(density(5, 10), '\u{2584}', "half the peak is half the ramp");
        assert_eq!(density(3, 0), ' ', "no peak means no scale to draw against");
    }

    #[test]
    fn a_repository_row_is_not_a_description_of_the_work() {
        // The one thing this view must never do. `RepositoryRow` and the strip both
        // carry identities and counts; neither carries prose, and the header says so
        // rather than leaving the reader to infer it from the absence.
        let root = TempRoot::new("log-no-titles");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        // Stated under `--explain` since the header collapsed to one line. Read off
        // that render rather than the default one: a `contains` against the default
        // would silently start testing nothing.
        let explained = render(&log, Header::Full);
        assert!(
            explained.contains("Not what the work was about"),
            "the limit is stated, not left to be discovered:\n{explained}"
        );
        // Every strip row's label is a literal suffix of the identity it stands for.
        // A label assembled from anything else -- a tool name, a workstream -- would
        // fail here, which is the fabrication the plan forbids.
        for row in &log.rows {
            let identity = row.repository.as_deref().unwrap_or(NO_REPOSITORY_LABEL);
            assert!(
                identity.ends_with(&row.label) || row.label == NO_REPOSITORY_LABEL,
                "{:?} is not a suffix of {identity}",
                row.label
            );
        }
        // And nothing in this view carries a field a description could be put in.
        fn _no_title(_: &RepositoryStrip, _: &BlockRow, _: &RepositoryRow) {}
    }

    // -- the header, and the window it describes ---------------------------------

    /// The header's caveat lines: everything between the day line and the blank line
    /// that closes the header.
    ///
    /// Read off the render and by structure, not by substring. The complaint this
    /// answers is "fourteen lines before any content", which is a count -- and no
    /// `contains` can answer a count.
    fn header_lines(rendered: &str) -> Vec<&str> {
        rendered
            .lines()
            .skip_while(|line| !line.trim().is_empty())
            .skip(1)
            .take_while(|line| !line.trim().is_empty())
            .collect()
    }

    /// The subjects the header covers: the first word of each line that starts a new
    /// one. Continuation lines are indented, so they are not subjects.
    fn header_labels(rendered: &str) -> Vec<&str> {
        header_lines(rendered)
            .into_iter()
            .filter(|line| !line.starts_with(' '))
            .filter_map(|line| line.split_whitespace().next())
            .collect()
    }

    /// Everything from the strip onwards -- the part of the render that is the day
    /// rather than the caveats about it.
    fn body_lines(rendered: &str) -> Vec<&str> {
        rendered
            .lines()
            .skip_while(|line| !line.starts_with("strip "))
            .collect()
    }

    #[test]
    fn the_default_header_is_one_line_and_explain_is_what_restores_the_rest() {
        let root = TempRoot::new("log-explain");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let brief = render(&log, Header::Brief);
        let full = render(&log, Header::Full);

        assert_eq!(
            header_lines(&brief).len(),
            1,
            "the default header is one line -- this is a command run every day:\n{brief}"
        );
        assert_eq!(
            header_labels(&brief),
            vec!["basis"],
            "and it covers one subject, not five:\n{brief}"
        );
        // Both halves of what the one line has to carry: that the number is an
        // estimate, and where the reasoning behind it went.
        let one_line = header_lines(&brief)[0];
        assert!(
            one_line.contains("(est.)"),
            "the one line still says the number is an estimate:\n{one_line}"
        );
        assert!(
            one_line.contains("--explain"),
            "and still says how to get the basis for it:\n{one_line}"
        );

        // `--explain` restores every subject, and restores them in full: fourteen
        // lines is what shipped, and this flag is the promise that nothing was
        // dropped rather than merely moved.
        assert_eq!(
            header_labels(&full),
            vec!["basis", "blocks", "attention", "config", "coverage"],
            "--explain covers every subject the default one dropped:\n{full}"
        );
        assert_eq!(
            header_lines(&full).len(),
            14,
            "and prints them at their shipped length, not a rewritten summary:\n{full}"
        );

        // The strongest form of "--explain changes the output": every line it adds is
        // absent from the default render, matched whole rather than by substring.
        for line in header_lines(&full).iter().skip(1) {
            assert!(
                !brief.lines().any(|printed| printed == *line),
                "the default view still prints an --explain line:\n{line}"
            );
        }
        // And the one thing it must not change: the day itself.
        assert_eq!(
            body_lines(&brief),
            body_lines(&full),
            "--explain adds caveats; it does not draw a different day"
        );
    }

    #[test]
    fn the_default_view_still_warns_that_the_blocks_do_not_add_up() {
        // The one caveat that had to survive the header's collapse. A reader meeting
        // a column of durations will try to sum them, and the warning has to be where
        // that column is -- not in a paragraph that is no longer printed.
        let root = TempRoot::new("log-not-additive");
        overlapping_day(root.path());

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");
        let brief = render(&log, Header::Brief);

        let count_line = brief
            .lines()
            .find(|line| line.starts_with("blocks "))
            .unwrap_or_else(|| panic!("the block list states no count:\n{brief}"));
        let mut fields = count_line.split_whitespace();
        assert_eq!(fields.next(), Some("blocks"));
        assert_eq!(
            fields.next().and_then(|n| n.parse::<usize>().ok()),
            Some(log.blocks.len()),
            "the line the warning rides on is the one that opens the list:\n{count_line}"
        );
        assert!(
            count_line.contains("not additive") && count_line.contains("cclogger report"),
            "which must say both that they do not sum and where the total is:\n{count_line}"
        );
        // Nothing else in the default view says it, so the assertion above is the
        // only thing keeping the warning printed at all.
        assert_eq!(
            brief
                .lines()
                .filter(|line| line.contains("not additive"))
                .count(),
            1,
            "exactly one line carries the warning:\n{brief}"
        );
    }

    /// A day that runs end to end, as the day that prompted `--from`/`--to` did:
    /// work in the first hour, work in the middle, work in the last. Whole, its strip
    /// is forced onto the coarsest cell the ladder has.
    ///
    /// The middle repository's name is the long one on purpose -- it is the one
    /// inside the narrowed window, so the label budget that chooses the cell size does
    /// not shrink along with the window, and the finer cell is the span's doing.
    fn a_day_from_end_to_end(root: &std::path::Path) {
        build(
            root,
            &[
                Session {
                    repository: "github.com/acme/dawn",
                    prompts: &[600],
                    agent: &[660, 720],
                },
                Session {
                    // Two stretches, two hours apart, so the narrowed strip still
                    // spans more than one hour mark and its ruler can be checked
                    // against the cell size the header claims.
                    repository: "github.com/acme/platform-integration-svc",
                    prompts: &[50_400, 50_700, 57_600],
                    agent: &[50_400, 50_500, 50_800, 57_660],
                },
                Session {
                    repository: "github.com/acme/dusk",
                    prompts: &[82_800],
                    agent: &[82_860],
                },
            ],
        );
    }

    #[test]
    fn narrowing_the_window_narrows_the_whole_query_and_not_only_the_drawn_axis() {
        let root = TempRoot::new("log-narrowed-query");
        a_day_from_end_to_end(root.path());

        let whole = run_log(root.path(), window(), TZ).expect("log the whole day");
        let narrowed = run_log(
            root.path(),
            DayWindow::new(day(), Some(time("13:00")), Some(time("17:00")))
                .expect("a stretch of the day"),
            TZ,
        )
        .expect("log the narrowed window");

        // Every number, not the axis alone. A narrowing that only moved
        // `first_bucket`/`last_bucket` would leave all of these untouched, which is
        // the failure mode this test exists for.
        assert_eq!(whole.blocks.len(), 4, "the fixture holds four blocks");
        assert_eq!(
            narrowed.blocks.len(),
            2,
            "only the blocks inside the window are blocks of this window"
        );
        assert_eq!(whole.prompts, 5);
        assert_eq!(narrowed.prompts, 3, "and only the prompts inside it count");
        assert_eq!(whole.observations_in_window, 12);
        assert_eq!(
            narrowed.observations_in_window, 7,
            "coverage is what the window covered, not what the day did"
        );
        assert_eq!(
            narrowed
                .rows
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            vec!["platform-integration-svc"],
            "and a repository whose only work fell outside gets no row at all"
        );

        // Attention is the number a reader takes away, so it has to move too.
        let attention =
            |log: &DayLog| -> i64 { log.blocks.iter().map(|b| b.attention_seconds).sum() };
        assert_eq!(attention(&whole), 1740);
        assert_eq!(
            attention(&narrowed),
            1020,
            "the windows anchored outside the range are not this window's attention"
        );

        // A window holding nothing is an empty view, not an error -- and it must not
        // claim the *day* held nothing, which is the reading that would be wrong.
        let quiet = run_log(
            root.path(),
            DayWindow::new(day(), Some(time("03:00")), Some(time("04:00")))
                .expect("a stretch of the day"),
            TZ,
        )
        .expect("log an hour nothing happened in");
        assert!(quiet.rows.is_empty());
        let rendered = render(&quiet, Header::Brief);
        assert!(
            rendered.contains("no observation in this window"),
            "an empty window says it was the window that was empty:\n{rendered}"
        );
    }

    #[test]
    fn narrowing_a_day_that_ran_end_to_end_restores_the_strips_resolution() {
        // The reason the flags exist. Activity in the first hour and the last forces
        // the cell ladder to its coarsest rung, which flattens exactly the detail the
        // strip is for. The span is what chooses the cell, so narrowing the query is
        // enough to get it back.
        let root = TempRoot::new("log-narrowed-cells");
        a_day_from_end_to_end(root.path());

        let whole = run_log(root.path(), window(), TZ).expect("log the whole day");
        let narrowed = run_log(
            root.path(),
            DayWindow::new(day(), Some(time("13:00")), Some(time("17:00")))
                .expect("a stretch of the day"),
            TZ,
        )
        .expect("log the narrowed window");

        assert_eq!(
            whole.bucket_seconds, 3600,
            "a day observed from 00:10 to 23:01 has to widen to hour-wide cells"
        );
        assert_eq!(
            narrowed.bucket_seconds, 600,
            "and two hours of it does not -- six times the resolution, for free"
        );

        // The finer cells still have to sit under the hours they belong to. A cell
        // grid indexed from the narrowed window rather than from local midnight would
        // slide the whole axis by the narrowing and label 14:00 over 01:00 -- and it
        // would stay self-consistent while doing it, so the ruler check below cannot
        // see it. This reads the wall-clock span off the render instead.
        let geometry = |rendered: &str| -> String {
            rendered
                .lines()
                .find(|line| line.starts_with("strip "))
                .unwrap_or_else(|| panic!("the strip states no geometry:\n{rendered}"))
                .split_once("1 cell = ")
                .expect("the geometry line names a cell size")
                .1
                .to_string()
        };
        assert_eq!(
            geometry(&render(&whole, Header::Brief)),
            "60 min, 00:00 .. 24:00 (the active span, snapped to the hour)"
        );
        assert_eq!(
            geometry(&render(&narrowed, Header::Brief)),
            "10 min, 14:00 .. 17:00 (the active span, snapped to the hour)",
            "the narrowed axis covers the hours the work was in, not hours counted \
             from wherever the window happened to start"
        );

        // The same row, drawn at both cell sizes, and the header agreeing with the
        // ruler in each: a finer cell that the header did not follow would make every
        // distance a reader takes off the axis wrong without looking wrong.
        assert_the_header_describes_the_drawn_strip(&render(&whole, Header::Brief), &whole);
        assert_the_header_describes_the_drawn_strip(&render(&narrowed, Header::Brief), &narrowed);
    }

    #[test]
    fn the_header_names_the_window_it_actually_queried() {
        // The narrowing is invisible in the numbers -- fewer blocks looks exactly
        // like a quieter day. The one line that keeps the output self-describing is
        // this one, and it survives into the default view because of it.
        let root = TempRoot::new("log-window-header");
        a_day_from_end_to_end(root.path());

        let narrowed = run_log(
            root.path(),
            DayWindow::new(day(), Some(time("13:00")), Some(time("15:00")))
                .expect("a stretch of the day"),
            TZ,
        )
        .expect("log the narrowed window");
        let whole = run_log(root.path(), window(), TZ).expect("log the whole day");

        assert_eq!(
            render(&narrowed, Header::Brief).lines().next(),
            Some(
                "2026-07-26  13:00-15:00  UTC+09:00  (2026-07-26T04:00:00Z .. 2026-07-26T06:00:00Z)"
            ),
            "the hours asked for, and the instants they mean"
        );
        assert_eq!(
            render(&whole, Header::Brief).lines().next(),
            Some("2026-07-26  UTC+09:00  (2026-07-25T15:00:00Z .. 2026-07-26T15:00:00Z)"),
            "and a whole day still says nothing about hours it did not narrow"
        );

        // A bound given alone is printed as the bound it is, never completed with one
        // the command would refuse to accept back.
        let from_only = run_log(
            root.path(),
            DayWindow::new(day(), Some(time("13:00")), None).expect("a from with no to"),
            TZ,
        )
        .expect("log from 13:00");
        assert_eq!(
            render(&from_only, Header::Brief).lines().next(),
            Some(
                "2026-07-26  from 13:00  UTC+09:00  (2026-07-26T04:00:00Z .. 2026-07-26T15:00:00Z)"
            ),
        );
    }

    #[test]
    fn records_a_fork_copied_light_no_cell_on_the_strip() {
        // The strip's whole claim is *when*. A Codex fork re-writes its parent's
        // history in one instant, so cells lit by those copies would draw a burst of
        // work in a minute that held only the copying -- and, worse, would draw it in
        // the wrong hour of the wrong day. They are kept off the strip for the same
        // reason gap markers are.
        let root = TempRoot::new("log-codex-inherited");
        let cwd = cwd_for("github.com/acme/api");
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/forked.jsonl",
            &codex_forked_rollout(&cwd, &at(3600), &at(36_000)),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        assert_eq!(
            log.observations_with_inherited_time, 1,
            "the copied prompt is counted rather than made to disappear"
        );
        assert_eq!(
            log.prompts, 1,
            "and only the live turn is a prompt the strip drew"
        );
        assert_eq!(
            log.union_row.iter().sum::<u64>(),
            2,
            "two observations reached a cell -- the fork's own start, and the live turn. \
             Not three: the prompt copied in behind the fork lights nothing"
        );

        // The block where the fork ran is real -- the fork happened then -- but it holds
        // only the fork itself. Without the exclusion it would also hold a human prompt,
        // and claim attention nobody spent nine hours before they spent it.
        let fork_block = log
            .blocks
            .iter()
            .min_by_key(|b| b.span.start)
            .expect("the fork's own block");
        assert_eq!(
            fork_block.prompts, 0,
            "no prompt happened when the fork ran; the copied one is the parent's"
        );
        assert_eq!(
            fork_block.attention_seconds, 0,
            "so the fork's block claims no attention at all"
        );

        // Coverage lives under `--explain`, with the rest of "how was this derived".
        let rendered = render(&log, Header::Full);
        assert!(
            rendered.contains("copied into a forked transcript"),
            "and the strip says what it left out:\n{rendered}"
        );
    }

    #[test]
    fn a_deferred_first_flushs_prompt_still_lights_its_cell() {
        // The benign look-alike. Codex does not create a rollout file until the first
        // user message, so an ordinary session opens with three records sharing one
        // millisecond. Reading that shape as a copy would blank the opening cell of
        // every ordinary Codex session on the strip.
        let root = TempRoot::new("log-codex-deferred-flush");
        let cwd = cwd_for("github.com/acme/api");
        archive_from(
            "codex",
            root.path(),
            ".codex/sessions/2026/07/26/ordinary.jsonl",
            &codex_deferred_first_flush(&cwd, &at(3600)),
            ACQUIRED_AT,
        );
        run_import(root.path(), false).expect("import the synthetic ledger");

        let log = run_log(root.path(), window(), TZ).expect("log the synthetic day");

        assert_eq!(log.observations_with_inherited_time, 0);
        assert_eq!(log.prompts, 1, "the session's first prompt is a real turn");
        assert_eq!(
            log.union_row.iter().sum::<u64>(),
            2,
            "the prompt and the session start both reached a cell; only the turn_context \
             did not, and that is because its kind is unmapped"
        );
    }
}
