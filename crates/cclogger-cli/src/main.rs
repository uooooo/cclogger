//! cclog CLI. `archive` gets history out of vendor directories before their
//! retention window deletes it; `hook` records events live as Claude Code emits them;
//! `import` turns both of those into canonical observations in the ledger.
//!
//! The two capture paths are not alternatives. A hook can only record from the moment
//! it is installed, and `archive` is the only thing that can reach a day that already
//! happened -- so `hook` adds precision going forward and takes nothing away.

mod clock;
mod discover;
mod git;
mod hook;
mod import;
mod log;
mod report;
mod tz;

use cclogger_archive::{Ledger, Outcome};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use tz::TzOffset;

#[derive(Parser)]
// `version` matters more now that this ships as a prebuilt binary: whoever installed it
// did not build it, so "which one do you have" has to be answerable from the binary
// itself rather than from a source tree.
#[command(name = "cclogger", about = "Local AI work ledger", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy vendor transcripts into the local archive.
    Archive {
        /// Home directory to scan (defaults to $HOME).
        #[arg(long)]
        home: Option<PathBuf>,
        /// cclog root (defaults to <home>/.cclog). Archived bytes live at
        /// <cclog-root>/archive, the ledger database at <cclog-root>/ledger.db.
        #[arg(long)]
        cclog_root: Option<PathBuf>,
        /// Report what would be archived without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// One-time, non-destructive migration of an M0 `manifest.db` into the M1
    /// `ledger.db` layout. Never modifies or deletes the old `manifest.db`.
    Migrate {
        /// Home directory (defaults to $HOME).
        #[arg(long)]
        home: Option<PathBuf>,
        /// cclog root (defaults to <home>/.cclog).
        #[arg(long)]
        cclog_root: Option<PathBuf>,
        /// Archive root holding the old manifest.db (defaults to <cclog-root>/archive).
        /// Must be the default -- a customized archive root is rejected, not
        /// silently migrated into a ledger whose objects would be unresolvable.
        #[arg(long)]
        archive_root: Option<PathBuf>,
        /// Proceed even if the destination ledger already holds rows. Off by
        /// default: migrating into a populated ledger is not a normal operation,
        /// and explicit `snapshot_id`s copied from manifest.db can collide with
        /// rows already there.
        #[arg(long)]
        force: bool,
    },
    /// Turn archived snapshots and the hook spool into canonical observations in the
    /// ledger. Reads what `cclogger archive` already saved plus what `cclogger hook`
    /// spooled -- never a live vendor directory, which may no longer hold what was
    /// archived (Claude Code deletes its own transcripts after ~30 days).
    Import {
        /// Home directory (defaults to $HOME). Only used to compute the default
        /// `--cclog-root`; import itself never reads a vendor directory.
        #[arg(long)]
        home: Option<PathBuf>,
        /// cclog root (defaults to <home>/.cclog).
        #[arg(long)]
        cclog_root: Option<PathBuf>,
        /// Report what would be ingested without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// What a period's ledger says was worked on, and for how long. One day by
    /// default; `--week`, `--month` and `--from`/`--to` name longer ones. Reads only
    /// what `cclogger import` already committed, and writes nothing.
    Report {
        /// Home directory (defaults to $HOME). Only used to compute the default
        /// `--cclog-root`.
        #[arg(long)]
        home: Option<PathBuf>,
        /// cclog root (defaults to <home>/.cclog). Workstream rules are read from
        /// <cclog-root>/config/workstreams.toml; a missing one is not an error.
        #[arg(long)]
        cclog_root: Option<PathBuf>,
        /// The day to report, `YYYY-MM-DD` in `--tz-offset-hours`. Defaults to today
        /// when no other period is named. May combine with `--from`/`--to` given as
        /// `HH:MM` times, which then narrow this one day rather than name a period.
        #[arg(long, conflicts_with_all = ["week", "month"])]
        day: Option<String>,
        /// The seven days ending on `YYYY-MM-DD`, or on today when given no value. A
        /// rolling week, not a Monday-to-Sunday one -- which is why it takes the day
        /// it ends on rather than a week number, and why the header prints both ends.
        #[arg(long, value_name = "YYYY-MM-DD", num_args = 0..=1,
              conflicts_with_all = ["day", "month", "from", "to"])]
        week: Option<Option<String>>,
        /// One calendar month, `YYYY-MM`, from its first day to its last.
        #[arg(long, value_name = "YYYY-MM", conflicts_with_all = ["day", "week", "from", "to"])]
        month: Option<String>,
        /// The start of the window this reports: a `YYYY-MM-DD` date, which is the
        /// first day of a run of days (needs `--to`, also a date -- the other end is
        /// not completed with today or the ledger's last day, since those are
        /// different periods and picking one would be a guess), or an `HH:MM` time,
        /// which narrows `--day` (or today) to a stretch of it, exactly as `log
        /// --from` does. A date and a time on the two flags names neither.
        #[arg(long, value_name = "YYYY-MM-DD|HH:MM", conflicts_with_all = ["week", "month"])]
        from: Option<String>,
        /// The end of that window, exclusive when it is a time (`HH:MM`) and
        /// inclusive when it is a date (`YYYY-MM-DD`) -- the same asymmetry `--day`
        /// vs. a period already has. See `--from`.
        #[arg(long, value_name = "YYYY-MM-DD|HH:MM", conflicts_with_all = ["week", "month"])]
        to: Option<String>,
        /// The offset the days are bucketed in. Defaults to this machine's current UTC
        /// offset, read fresh on every run. Give it as whole hours (`9`, `-5`) or as
        /// `±HH:MM` for a zone that is not one (`5:30`, `-3:30`). A fixed offset, not a
        /// zone: one number for the whole period, so a period spanning a DST transition
        /// is bucketed on one side of it throughout.
        #[arg(long, value_name = "H|±HH:MM", allow_hyphen_values = true,
              value_parser = tz::parse_flag)]
        tz_offset_hours: Option<TzOffset>,
    },
    /// Record one Claude Code hook event. Called by Claude Code, not by hand: it
    /// reads the hook's JSON payload on stdin, appends one metadata line to
    /// <cclog-root>/spool/hooks.jsonl, and exits.
    ///
    /// Always exits 0, whatever goes wrong. Claude Code runs command hooks
    /// synchronously and treats a hook error as one, so this must never be able to
    /// interrupt a session; a failure it swallows is recorded in the spool (or in
    /// hook-receiver-errors.log beside it) instead of in the exit code.
    ///
    /// Run `cclogger hook-install` for the settings to register it.
    Hook {
        /// Home directory (defaults to $HOME). Only used to compute the default
        /// `--cclog-root`; the receiver reads no vendor directory.
        #[arg(long)]
        home: Option<PathBuf>,
        /// cclog root (defaults to <home>/.cclog).
        #[arg(long)]
        cclog_root: Option<PathBuf>,
    },
    /// Print the settings JSON that registers `cclogger hook` with Claude Code.
    ///
    /// Prints; does not write. ~/.claude/settings.json is yours, its `hooks` object
    /// may already hold entries this tool knows nothing about, and a merge that got
    /// that wrong would silently break your editor.
    HookInstall,
    /// A day as a shape: where the work sat on a time axis, one row per repository,
    /// and the blocks underneath it. Same ledger and same day window as `report`,
    /// which answers how much rather than when. Reads only, and writes nothing.
    Log {
        /// Home directory (defaults to $HOME). Only used to compute the default
        /// `--cclog-root`.
        #[arg(long)]
        home: Option<PathBuf>,
        /// cclog root (defaults to <home>/.cclog). Workstream rules are read from
        /// <cclog-root>/config/workstreams.toml; a missing one is not an error.
        #[arg(long)]
        cclog_root: Option<PathBuf>,
        /// The day to draw, `YYYY-MM-DD` in `--tz-offset-hours`. Defaults to today.
        #[arg(long)]
        day: Option<String>,
        /// Narrow the day to start at this `HH:MM`, in `--tz-offset-hours`. Narrows
        /// the whole query, not just the axis: blocks, prompts, attention and
        /// coverage are all computed over what the narrowed window holds. `log` draws
        /// one day at a time, so a `YYYY-MM-DD` date here -- a day *boundary*, which
        /// only means something across a run of days -- is refused rather than
        /// treated as malformed; see `cclogger report --from` for a range of days.
        #[arg(long, value_name = "HH:MM")]
        from: Option<String>,
        /// Narrow the day to end at this `HH:MM` (exclusive). Leave it off to run to
        /// the end of the day. See `--from` for why a date here is refused.
        #[arg(long, value_name = "HH:MM")]
        to: Option<String>,
        /// Print the full basis: how blocks and attention are derived, what the
        /// ledger cannot see, which config was loaded, and what the day's coverage
        /// was. Off by default -- one line says the number is an estimate.
        #[arg(long)]
        explain: bool,
        /// The offset the day is bucketed in -- see `report --tz-offset-hours`.
        #[arg(long, value_name = "H|±HH:MM", allow_hyphen_values = true,
              value_parser = tz::parse_flag)]
        tz_offset_hours: Option<TzOffset>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Archive {
            home,
            cclog_root,
            dry_run,
        } => {
            let home = home
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").expect("HOME is not set")));
            let root = cclog_root.unwrap_or_else(|| home.join(".cclog"));
            match run_archive(&home, &root, dry_run) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("archive failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Migrate {
            home,
            cclog_root,
            archive_root,
            force,
        } => {
            let home = home
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").expect("HOME is not set")));
            let root = cclog_root.unwrap_or_else(|| home.join(".cclog"));
            let archive_root = archive_root.unwrap_or_else(|| root.join("archive"));
            match run_migrate(&archive_root, &root, force) {
                Ok(clean) => {
                    if clean {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("migrate failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Import {
            home,
            cclog_root,
            dry_run,
        } => {
            let home = home
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").expect("HOME is not set")));
            let root = cclog_root.unwrap_or_else(|| home.join(".cclog"));
            match import::run_import(&root, dry_run) {
                Ok(report) => {
                    print_import_report(&report, dry_run, &root);
                    if report.locators_unreadable == 0 {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("import failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Hook { home, cclog_root } => {
            // No `expect("HOME is not set")` here, unlike every other subcommand: this
            // one runs inside the user's editing loop, and a panic exits non-zero,
            // which Claude Code reports as a hook error. An unset HOME means the spool
            // path cannot be computed, so nothing is recorded -- and nothing is
            // interrupted either.
            let root = cclog_root.or_else(|| {
                home.or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
                    .map(|home| home.join(".cclog"))
            });
            match root {
                Some(root) => ExitCode::from(hook::run_hook(
                    &root,
                    std::io::stdin().lock(),
                    &clock::now_utc_millis(),
                )),
                None => {
                    eprintln!("cclogger hook: neither --cclog-root nor HOME is set");
                    ExitCode::SUCCESS
                }
            }
        }
        Command::HookInstall => {
            // The absolute path of *this* binary, because `cclogger` is very often not
            // on the PATH a hook runs with -- `cargo install` puts it in ~/.cargo/bin,
            // which the README already warns about for scheduled jobs.
            let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cclogger"));
            print!("{}", hook::install_instructions(&binary));
            ExitCode::SUCCESS
        }
        Command::Report {
            home,
            cclog_root,
            day,
            week,
            month,
            from,
            to,
            tz_offset_hours,
        } => {
            let home = home
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").expect("HOME is not set")));
            let root = cclog_root.unwrap_or_else(|| home.join(".cclog"));
            let tz_offset = match resolve_offset(tz_offset_hours) {
                Ok(offset) => offset,
                Err(refusal) => {
                    eprintln!("report failed: {refusal}");
                    return ExitCode::FAILURE;
                }
            };
            match period_for(day, week, month, from, to, tz_offset)
                .and_then(|period| report::run_report(&root, period, tz_offset))
            {
                Ok(period_report) => {
                    print!("{}", report::render(&period_report));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("report failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Log {
            home,
            cclog_root,
            day,
            from,
            to,
            explain,
            tz_offset_hours,
        } => {
            let home = home
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").expect("HOME is not set")));
            let root = cclog_root.unwrap_or_else(|| home.join(".cclog"));
            // `report::day_window` on purpose, not a second parser: it is what
            // refuses a date the calendar never had (`2026-02-30`), and it is the
            // same function `report --day` resolves `--from`/`--to` through, so the
            // two commands cannot come to disagree about what either flag means.
            let header = if explain {
                log::Header::Full
            } else {
                log::Header::Brief
            };
            let tz_offset = match resolve_offset(tz_offset_hours) {
                Ok(offset) => offset,
                Err(refusal) => {
                    eprintln!("log failed: {refusal}");
                    return ExitCode::FAILURE;
                }
            };
            let window = report::day_window(day, from, to, tz_offset);
            match window.and_then(|window| log::run_log(&root, window, tz_offset)) {
                Ok(day_log) => {
                    print!("{}", log::render(&day_log, header));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("log failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// The offset `report` and `log` bucket their days in: the one `--tz-offset-hours`
/// named, or this machine's own.
///
/// Nothing is substituted when the machine's offset cannot be read. There is no number to
/// substitute: any constant is right on one meridian and wrong everywhere else, and being
/// wrong here does not look like an error -- it cuts "today" at an hour that is not the
/// reader's midnight, prints a perfectly plausible offset in the header, and moves a
/// morning's work into yesterday without saying anything. That is the defect this
/// resolution exists to close, so it is refused with the flag that settles it named,
/// rather than reintroduced as a fallback.
fn resolve_offset(given: Option<TzOffset>) -> Result<TzOffset, String> {
    match given {
        Some(offset) => Ok(offset),
        None => tz::detect().map_err(|e| {
            format!(
                "could not read this machine's UTC offset -- {e}. Days have to be \
                 bucketed at some offset, and there is no safe one to assume: a guessed \
                 offset cuts \"today\" at an hour that is not your midnight and says so \
                 nowhere. Name it with --tz-offset-hours (`9`, `-5`, or `5:30` for a zone \
                 that is not a whole hour)."
            )
        }),
    }
}

/// Which period `report`'s flags named, or the refusal that stops it.
///
/// `--week` and `--month` still cannot combine with anything else -- clap enforces
/// that, because two names for what a report covers is a question with no right
/// answer rather than a precedence to settle. `--day` and `--from`/`--to` can
/// combine, though: once both are ruled out here, the rest -- a run of days, one day
/// narrowed by time, or a bare day -- is `report::day_period`'s call, the same
/// function that resolves `--day`/`--from`/`--to` on their own. Everything a period
/// is built from is parsed by the module that owns it (`report::Day::parse`,
/// `report::Period::month`), so a date this command refuses on `--day` is refused
/// identically on `--from`, `--to` and `--week`.
///
/// Separated from `main` so the resolution can be tested without a process.
fn period_for(
    day: Option<String>,
    week: Option<Option<String>>,
    month: Option<String>,
    from: Option<String>,
    to: Option<String>,
    tz_offset: TzOffset,
) -> Result<report::Period, report::ReportError> {
    if let Some(month) = month {
        return report::Period::month(&month);
    }
    if let Some(anchor) = week {
        // `--week` with no value ends today; with one, on the day it names. Both are
        // seven days back from there.
        let last = match anchor {
            Some(raw) => report::Day::parse(&raw)?,
            None => report::Day::today(tz_offset),
        };
        return Ok(report::Period::week_ending(last));
    }
    report::day_period(day, from, to, tz_offset)
}

fn print_import_report(report: &import::ImportReport, dry_run: bool, root: &std::path::Path) {
    if report.ledger_missing {
        println!("no ledger at {} -- nothing to import", root.display());
        println!("(dry run -- nothing written, and nothing created to write into)");
        println!("run `cclogger archive` first to populate one.");
        return;
    }
    if report.ledger_needs_upgrade {
        println!(
            "the ledger at {} needs a schema upgrade -- run without --dry-run first",
            root.display()
        );
        println!("(dry run -- nothing written, and the upgrade was not performed)");
        return;
    }
    if dry_run {
        println!("(dry run -- nothing written)");
    }
    println!("locators scanned    {}", report.locators_scanned);
    println!("locators processed  {}", report.locators_processed);
    println!("locators unchanged  {}", report.locators_unchanged);
    if report.locators_unreadable > 0 {
        println!("locators unreadable {}", report.locators_unreadable);
    }
    if report.checkpoints_reset > 0 {
        println!("cursors reset      {}", report.checkpoints_reset);
    }
    if report.lines_incomplete > 0 {
        println!(
            "snapshots ending mid-record {} (final record deferred to a later snapshot)",
            report.lines_incomplete
        );
    }
    println!();
    // Says "would create" under --dry-run because that is what it is: nothing was
    // written. The split itself models both ways a real run deduplicates -- keys
    // already in the ledger, and keys repeated within the run -- so the numbers are
    // what a real run would report, not an upper bound on them.
    let created_label = if dry_run {
        "observations that would be created"
    } else {
        "observations created"
    };
    if report.observations_created.is_empty() {
        println!("{created_label}  0");
    } else {
        println!("{created_label}");
        for (event_type, count) in &report.observations_created {
            println!("  {event_type:<32} {count}");
        }
    }
    println!(
        "observations already present  {}",
        report.observations_already_present
    );
    if report.observations_unattributed > 0 {
        println!(
            "no repository (gaps excl.)  {}",
            report.observations_unattributed
        );
    }
    if report.observations_inherited > 0 {
        println!(
            "copied history              {}",
            report.observations_inherited
        );
        println!("  a fork re-wrote these out of a parent transcript, so they carry the copy's");
        println!("  write time. Imported and marked; excluded from every clock.");
    }
    if !report.records_skipped.is_empty() {
        println!();
        println!("records skipped (understood, no event)");
        for (kind, count) in &report.records_skipped {
            println!("  {kind:<32} {count}");
        }
    }
    print_spool_section(report);
    print_git_section(report);
    println!();
    if report.total_gaps() == 0 {
        println!("gaps  0");
    } else {
        println!("gaps");
        if report.gap_parse_error > 0 {
            println!(
                "  parse_error                    {}",
                report.gap_parse_error
            );
        }
        if !report.gap_unmapped_kind.is_empty() {
            println!("  unmapped_kind");
            for (kind, count) in &report.gap_unmapped_kind {
                println!("    {kind:<30} {count}");
            }
        }
        if !report.gap_missing_field.is_empty() {
            println!("  missing_field");
            for (field, count) in &report.gap_missing_field {
                println!("    {field:<30} {count}");
            }
        }
        if !report.gap_receiver_error.is_empty() {
            println!("  receiver_error (the hook could not record the event at all)");
            for (reason, count) in &report.gap_receiver_error {
                println!("    {reason:<30} {count}");
            }
        }
    }
    println!();
    println!("cclog root {}", root.display());
}

/// What the hook channel contributed, and the two things it cannot say.
///
/// Printed only when there is a spool, so an installation with no hooks registered sees
/// nothing new. Both caveats are printed whenever it *did* contribute, rather than left
/// for someone to discover from a total that looked complete: the numbers above are a
/// floor on both axes, and neither the arithmetic nor the counter names say so.
fn print_spool_section(report: &import::ImportReport) {
    let Some(begins) = &report.spool_begins_at else {
        return;
    };
    println!();
    println!("hook spool");
    println!("  lines drained     {}", report.spool_lines_drained);
    println!("  capture begins    {begins}");
    println!("    nothing before that instant is hook-observed, and nothing can make it so:");
    println!("    hooks record from the moment they are installed, and Claude Code buffers");
    println!("    and replays nothing. `archive` + `import` remain the only path to a day");
    println!("    that already happened.");
    println!("  turn completion is not exhaustive: `Stop` does not fire on an interrupted");
    println!("    turn, an API error fires `StopFailure` instead, and a killed process fires");
    println!("    nothing at all -- so prompts submitted may exceed responses completed");
    println!("    without anything being wrong.");
}

/// What the git channel contributed, and what it could not read.
///
/// Printed only when there was a repository to look at, so an installation whose ledger
/// holds no repository identity yet sees nothing new.
///
/// The unresolved list is the part that must not be quiet. Every line of it is a
/// repository the ledger says work happened in and this run could not read -- moved,
/// deleted, no longer a repository, or one whose git identity is not configured, so
/// there is no way to tell the person's commits from a colleague's. A count of commits
/// printed without them would read as complete.
fn print_git_section(report: &import::ImportReport) {
    if report.git_repositories_scanned == 0 {
        return;
    }
    println!();
    println!("git");
    println!(
        "  repositories      {} scanned, {} read, {} unchanged",
        report.git_repositories_scanned,
        report.git_repositories_collected,
        report.git_repositories_unchanged
    );
    println!("  commits collected {}", report.git_commits_collected);
    println!(
        "    the last {} days of each repository, the person's own commits only (git's own",
        import::GIT_WINDOW_DAYS
    );
    println!("    configured user.email), merges excluded -- `git log` prints no diffstat for");
    println!("    one, and a merge claiming it changed nothing would be a fabricated zero.");
    println!("    Dated by author time, so a rebase does not move a commit to the day it was");
    println!("    replayed. Nothing before that window is reachable by re-running.");
    if report.git_repositories_truncated > 0 {
        println!(
            "  ceiling reached    {} repository(ies) had more commits in the window than were",
            report.git_repositories_truncated
        );
        println!("    collected; the oldest of them were left out.");
    }
    if !report.git_repositories_unresolved.is_empty() {
        println!("  not read (the ledger says work happened here)");
        for (reason, count) in &report.git_repositories_unresolved {
            println!("    {reason:<30} {count}");
        }
    }
}

fn run_archive(
    home: &std::path::Path,
    root: &std::path::Path,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let found = discover::sources(home);

    // Not fatal, just a nudge: someone who upgraded from M0 and runs `archive`
    // straight away would otherwise start a brand new, empty ledger.db and never
    // notice their pre-M1 history sitting unmigrated in the old manifest.db. Keyed
    // on the durable migration marker (not "ledger.db doesn't exist yet", which
    // `Ledger::open` below makes true for at most this one run), so it fires every
    // time it's still true -- including under `--dry-run`, the one mode whose whole
    // purpose is to say what running for real would do.
    if let Some(notice) = unmigrated_manifest_notice(root) {
        eprintln!("{notice}");
    }

    if dry_run {
        let bytes: u64 = found
            .iter()
            .filter_map(|s| std::fs::metadata(&s.path).ok())
            .map(|m| m.len())
            .sum();
        println!("would archive {} files ({} bytes)", found.len(), bytes);
        println!();
        print_disclosure();
        return Ok(());
    }

    let acquired_at = clock::now_utc_seconds();
    let mut ledger = Ledger::open(root)?;
    let (mut created, mut present, mut failed) = (0u32, 0u32, 0u32);

    for src in &found {
        let bytes = match std::fs::read(&src.path) {
            Ok(b) => b,
            Err(e) => {
                // Keep going: one unreadable file must not strand the rest of the history.
                eprintln!("skip {}: {e}", src.locator);
                failed += 1;
                continue;
            }
        };
        match ledger.archive_file(src.kind, &src.locator, &bytes, &acquired_at, None)? {
            Outcome::Created(_) => created += 1,
            Outcome::AlreadyPresent(_) => present += 1,
        }
    }

    println!("scanned    {}", found.len());
    println!("archived   {created}");
    println!("unchanged  {present}");
    if failed > 0 {
        println!("unreadable {failed}");
    }
    println!("cclog root {}", root.display());
    println!();
    print_disclosure();
    Ok(())
}

/// The unmigrated-manifest nudge's message, or `None` if there is nothing to nudge
/// about.
///
/// Keyed on the durable migration marker
/// (`cclogger_archive::migrate::manifest_already_migrated`), not on whether
/// `<cclog-root>/ledger.db` exists: that file gets created by the very first
/// `Ledger::open` call this process makes (in `run_archive`, right after this
/// check), so an exists-check could fire at most once, ever, and never under
/// `--dry-run` (which never reaches `Ledger::open`). The marker instead reflects
/// whether `cclog migrate` has actually been run, so this fires every time that is
/// still true -- including under `--dry-run`, the one mode whose whole purpose is to
/// say what running for real would do.
fn unmigrated_manifest_notice(root: &std::path::Path) -> Option<String> {
    let old_manifest = root.join("archive").join("manifest.db");
    if !old_manifest.exists() {
        return None;
    }
    // A check failure here must not block `cclogger archive` -- this is an advisory
    // nudge, not a precondition -- so anything short of a clean "yes, migrated" is
    // treated as "not migrated yet" rather than surfaced as an error.
    let already_migrated =
        cclogger_archive::migrate::manifest_already_migrated(root).unwrap_or(false);
    if already_migrated {
        return None;
    }
    Some(format!(
        "note: found an unmigrated {} -- run `cclog migrate` first to bring its \
         history into the ledger",
        old_manifest.display()
    ))
}

/// Runs the migration and prints its report. Returns whether the result was clean
/// (used by `main` to decide the process exit code) -- distinct from `Err`, which
/// means the migration did not run at all (refused, or a hard I/O/DB error), not
/// that it ran and found a discrepancy.
fn run_migrate(
    archive_root: &std::path::Path,
    cclog_root: &std::path::Path,
    force: bool,
) -> Result<bool, Box<dyn std::error::Error>> {
    let manifest_path = archive_root.join("manifest.db");
    if !manifest_path.exists() {
        println!("no {} found -- nothing to migrate", manifest_path.display());
        return Ok(true);
    }

    let existing_rows = if force {
        cclogger_archive::migrate::ExistingRows::Proceed
    } else {
        cclogger_archive::migrate::ExistingRows::Refuse
    };
    let report = cclogger_archive::migrate::migrate_manifest_to_ledger(
        archive_root,
        cclog_root,
        existing_rows,
    )?;

    println!(
        "source_snapshot   {} row(s) in manifest.db",
        report.source_snapshot_count
    );
    if report.missing_snapshots.is_empty() {
        println!("snapshots missing 0");
    } else {
        println!("snapshots missing {}", report.missing_snapshots.len());
        for missing in &report.missing_snapshots {
            println!(
                "  missing: {} (object {})",
                missing.source_locator, missing.object_id
            );
        }
    }
    println!("objects verified  {}", report.objects_verified);
    if report.objects_missing.is_empty() {
        println!("objects missing   0");
    } else {
        println!("objects missing   {}", report.objects_missing.len());
        for id in &report.objects_missing {
            println!("  missing: {id}");
        }
    }
    println!();

    // The "safe to remove" message and a clean exit code are earned, not assumed:
    // both are gated on the migration actually having verified every source row
    // landed, not merely on `migrate_manifest_to_ledger` returning `Ok`. `manifest.db`
    // may be the only surviving copy of data that failed to verify, so a discrepancy
    // must go the other way -- towards "keep it" -- not be silently overridden by an
    // unconditional message printed before the warning.
    let clean = report.is_clean();
    if clean {
        println!(
            "{} was not modified. It is safe to keep or remove once you have verified this output.",
            manifest_path.display()
        );
    } else {
        eprintln!(
            "migration completed with discrepancies -- see above. {} was NOT verified \
             clean and may be the only copy of the data listed as missing above: do not \
             remove it.",
            manifest_path.display()
        );
    }
    Ok(clean)
}

/// Printed on both the real run and `--dry-run`: the one mode whose purpose is "tell
/// me what this would do" must not be the one mode that omits what it would hold.
fn print_disclosure() {
    println!("This archive holds raw prompts, assistant output, and code from your");
    println!("sessions. It is owner-only, and cclog itself never uploads or transmits it.");
    println!("Keep the archive root out of any cloud-synced directory.");
}

#[cfg(test)]
mod tests {
    use super::{Cli, TzOffset, period_for, report, run_archive};
    use cclogger_archive::Ledger;
    use clap::{CommandFactory, Parser};
    use std::os::unix::fs::PermissionsExt;

    /// The offset these resolutions are worked out in. Fixed here rather than detected,
    /// and unrelated to whatever offset the machine running the suite is in: what every
    /// test below checks is which *period* a set of flags names, and a period that
    /// changed with the host would be checking the host instead.
    const TZ: TzOffset = TzOffset::from_hours(9);

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("cclogger").chain(args.iter().copied())).is_ok()
    }

    #[test]
    fn a_day_and_a_period_are_two_names_for_one_report_and_are_refused_together() {
        // Both name what the report covers. Letting one win silently would produce a
        // report over days the person also named, headed by the ones they did not.
        Cli::command().debug_assert();

        for conflicting in [
            vec!["report", "--day", "2026-07-26", "--week"],
            vec!["report", "--day", "2026-07-26", "--month", "2026-07"],
            vec!["report", "--week", "--month", "2026-07"],
            vec!["report", "--month", "2026-07", "--from", "2026-07-01"],
            vec!["report", "--week", "--to", "2026-07-07"],
            // `--week`/`--month` still hard-conflict with `--from`/`--to` regardless
            // of which form the value takes -- narrowing a multi-day period by a
            // single time of day has no one day to resolve it against.
            vec!["report", "--week", "--from", "09:00"],
            vec!["report", "--month", "2026-07", "--to", "18:00"],
        ] {
            assert!(
                !parses(&conflicting),
                "{conflicting:?} names the period twice and must be refused"
            );
        }

        // And each on its own is a period, or refusing every combination would pass
        // the whole test above.
        for accepted in [
            vec!["report", "--day", "2026-07-26"],
            vec!["report", "--week"],
            vec!["report", "--week", "2026-07-26"],
            vec!["report", "--month", "2026-07"],
            vec!["report", "--from", "2026-07-01", "--to", "2026-07-07"],
            vec!["report"],
            // `--day` and `--from`/`--to` are no longer a clap-level conflict: a
            // time on `--from`/`--to` narrows the day `--day` names, so the two
            // combine (see `a_day_and_a_time_shaped_from_to_combine_to_narrow_it`
            // below for what that resolves to).
            vec![
                "report",
                "--day",
                "2026-07-26",
                "--from",
                "09:00",
                "--to",
                "18:00",
            ],
            // `--day` plus a *date*-shaped `--from`/`--to` also parses at the clap
            // level now -- clap cannot see that the value is a date rather than a
            // time -- but is refused once resolved; see
            // `a_day_already_pins_one_day_so_a_date_on_from_to_is_refused` below.
            vec![
                "report",
                "--day",
                "2026-07-26",
                "--from",
                "2026-07-01",
                "--to",
                "2026-07-07",
            ],
        ] {
            assert!(
                parses(&accepted),
                "{accepted:?} names one period and must parse"
            );
        }
    }

    #[test]
    fn the_hook_receiver_takes_no_flag_that_could_stop_it_reading_stdin() {
        // Claude Code invokes `<binary> hook` with the payload on stdin and nothing
        // else, so the bare form has to parse. `--cclog-root` exists for tests and for
        // a non-default root; a *required* argument here would mean every hook
        // invocation failed, which -- because the receiver can only ever exit 0 --
        // would be a silent no-op rather than a visible error.
        assert!(parses(&["hook"]));
        assert!(parses(&["hook", "--cclog-root", "/SYNTHETIC/root"]));
        assert!(parses(&["hook", "--home", "/SYNTHETIC/home"]));
        assert!(parses(&["hook-install"]));
        // `hook-install` prints; it takes no root to write into, and offering one
        // would imply it does.
        assert!(!parses(&[
            "hook-install",
            "--cclog-root",
            "/SYNTHETIC/root"
        ]));
    }

    #[test]
    fn one_end_of_a_range_alone_is_refused_rather_than_completed_with_a_guess() {
        let from = || Some("2026-07-01".to_string());
        let to = || Some("2026-07-07".to_string());

        assert!(
            matches!(
                period_for(None, None, None, from(), None, TZ),
                Err(report::ReportError::IncompleteRange { .. })
            ),
            "today, the ledger's last day and the month's end are three different periods"
        );
        assert!(matches!(
            period_for(None, None, None, None, to(), TZ),
            Err(report::ReportError::IncompleteRange { .. })
        ));

        let range = period_for(None, None, None, from(), to(), TZ).expect("both ends name a range");
        assert_eq!(range.label(), "2026-07-01 .. 2026-07-07  (7 days)");
        assert_eq!(range.calendar_days(), 7);
    }

    #[test]
    fn each_flag_resolves_to_the_period_it_names_and_nothing_resolves_to_today() {
        let today = report::Day::today(TZ).label();

        let default = period_for(None, None, None, None, None, TZ).expect("no flag is today");
        assert_eq!(
            default.label(),
            today,
            "no flag at all is today, and one day of it"
        );
        assert_eq!(default.calendar_days(), 1);

        // `--week` with no day ends today, and is seven days rather than one.
        let week = period_for(None, Some(None), None, None, None, TZ).expect("a bare --week");
        assert!(
            week.label().ends_with(&format!("(7 days ending {today})")),
            "the rolling week ends on the day it is run: {}",
            week.label()
        );
        assert_eq!(week.calendar_days(), 7);

        // `--week <day>` ends on that day instead, so the value is read rather than
        // merely accepted.
        let anchored = period_for(None, Some(Some("2026-07-26".into())), None, None, None, TZ)
            .expect("an anchored --week");
        assert_eq!(
            anchored.label(),
            "2026-07-20 .. 2026-07-26  (7 days ending 2026-07-26)"
        );

        let month =
            period_for(None, None, Some("2026-02".into()), None, None, TZ).expect("a month");
        assert_eq!(
            month.label(),
            "2026-02  (2026-02-01 .. 2026-02-28, 28 days)"
        );

        let day = period_for(Some("2026-07-26".into()), None, None, None, None, TZ).expect("a day");
        assert_eq!(day.label(), "2026-07-26");
        assert_eq!(day.calendar_days(), 1);

        // The refusals `Day::parse` and `Period::month` make are reached through the
        // flags rather than only in isolation.
        assert!(period_for(Some("2026-02-30".into()), None, None, None, None, TZ).is_err());
        assert!(period_for(None, Some(Some("2026-02-30".into())), None, None, None, TZ).is_err());
        assert!(period_for(None, None, Some("2026-13".into()), None, None, TZ).is_err());
        assert!(
            period_for(
                None,
                None,
                None,
                Some("2026-07-07".into()),
                Some("2026-07-01".into()),
                TZ
            )
            .is_err(),
            "a range that runs backwards is refused through the flags too"
        );
    }

    #[test]
    fn a_lone_time_on_from_or_to_narrows_today_without_day_being_named() {
        // The repro this whole change exists to fix: `report --from 09:00` used to
        // fail with "not a calendar date", a message that did not even acknowledge
        // `--from` could mean something else. It now means exactly what `log --from
        // 09:00` already meant: today, narrowed from 09:00 to the end of the day.
        let today = report::Day::today(TZ);

        let from_only = period_for(None, None, None, Some("09:00".into()), None, TZ)
            .expect("a lone --from time narrows today rather than failing");
        assert_eq!(from_only.label(), today.label());
        assert_eq!(from_only.calendar_days(), 1);
        assert_eq!(from_only.range_label().as_deref(), Some("from 09:00"));
        let (start, end) = from_only.utc_window(TZ);
        assert_eq!(start, today.utc_window(TZ).0 + 9 * 3600);
        assert_eq!(
            end,
            today.utc_window(TZ).1,
            "no --to means open to the end of the day, same as `log`"
        );

        let to_only = period_for(None, None, None, None, Some("18:00".into()), TZ)
            .expect("a lone --to time narrows today the same way");
        assert_eq!(to_only.range_label().as_deref(), Some("to 18:00"));

        let both = period_for(
            None,
            None,
            None,
            Some("09:00".into()),
            Some("18:00".into()),
            TZ,
        )
        .expect("--from and --to both times narrow today between them");
        assert_eq!(both.range_label().as_deref(), Some("09:00-18:00"));
        let (start, end) = both.utc_window(TZ);
        assert_eq!(end - start, 9 * 3600, "09:00 to 18:00 is nine hours");
    }

    #[test]
    fn a_day_and_a_time_shaped_from_to_combine_to_narrow_it() {
        let day = period_for(
            Some("2026-07-26".into()),
            None,
            None,
            Some("09:00".into()),
            Some("18:00".into()),
            TZ,
        )
        .expect("--day and a time-shaped --from/--to combine rather than conflict");
        assert_eq!(day.label(), "2026-07-26");
        assert_eq!(day.range_label().as_deref(), Some("09:00-18:00"));
        assert_eq!(day.calendar_days(), 1);

        // `--day` alone, or with only one of the two times, is unaffected.
        let bare = period_for(Some("2026-07-26".into()), None, None, None, None, TZ)
            .expect("--day alone is unchanged");
        assert_eq!(bare.range_label(), None);
    }

    #[test]
    fn a_day_already_pins_one_day_so_a_date_on_from_to_is_refused() {
        let err = period_for(
            Some("2026-07-26".into()),
            None,
            None,
            Some("2026-07-01".into()),
            None,
            TZ,
        )
        .expect_err("a date on --from conflicts with --day, which already named the day");
        assert!(
            matches!(
                err,
                report::ReportError::DateOnSingleDay {
                    scope: report::DayScope::ReportDay,
                    ..
                }
            ),
            "got {err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("--day already names the one day this report covers"),
            "the message must say *why* the date is out of scope, not merely call it \
             invalid: {message:?}"
        );
        // It is a real, well-formed date -- the refusal must not describe it the way
        // `InvalidDay` describes a date the calendar never had.
        assert!(
            !message.contains("is not a calendar date"),
            "a well-formed date must not be reported as malformed: {message:?}"
        );
    }

    #[test]
    fn a_date_and_a_time_on_from_to_together_name_no_window() {
        let err = period_for(
            None,
            None,
            None,
            Some("2026-07-01".into()),
            Some("18:00".into()),
            TZ,
        )
        .expect_err(
            "one date and one time on --from/--to names neither a range nor a narrowed day",
        );
        assert!(
            matches!(err, report::ReportError::MixedBoundKinds { .. }),
            "got {err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("name no window"),
            "the message must say why the pair fails, not just call it invalid: {message:?}"
        );

        // The reverse assignment of date/time to from/to is caught the same way.
        let err = period_for(
            None,
            None,
            None,
            Some("09:00".into()),
            Some("2026-07-31".into()),
            TZ,
        )
        .expect_err("a time on --from and a date on --to is the same mismatch, reversed");
        assert!(matches!(err, report::ReportError::MixedBoundKinds { .. }));
    }

    #[test]
    fn log_refuses_a_date_on_from_to_by_naming_the_reason_not_by_calling_it_malformed() {
        let err = report::day_window(None, Some("2026-07-01".into()), None, TZ)
            .expect_err("log is day-scoped; a date on --from names a range, out of scope");
        assert!(
            matches!(
                err,
                report::ReportError::DateOnSingleDay {
                    scope: report::DayScope::Log,
                    ..
                }
            ),
            "got {err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("only ever draws one day"),
            "the refusal must say log is day-scoped, not that the date is malformed: \
             {message:?}"
        );
        assert!(
            !message.contains("is not a calendar date"),
            "a well-formed date must not be described the way `InvalidDay` describes \
             a malformed one: {message:?}"
        );

        let err = report::day_window(
            Some("2026-07-26".into()),
            None,
            Some("2026-08-01".into()),
            TZ,
        )
        .expect_err("--day does not change log's refusal of a date on --to either");
        assert!(matches!(
            err,
            report::ReportError::DateOnSingleDay {
                scope: report::DayScope::Log,
                ..
            }
        ));
    }

    #[test]
    fn logs_existing_from_to_forms_still_resolve_the_same_window() {
        let bare = report::day_window(Some("2026-07-26".into()), None, None, TZ)
            .expect("a bare day is unaffected");
        assert_eq!(bare.range_label(), None);

        let one_end = report::day_window(Some("2026-07-26".into()), Some("13:00".into()), None, TZ)
            .expect("either flag may still be given alone");
        assert_eq!(one_end.range_label().as_deref(), Some("from 13:00"));

        let both = report::day_window(
            Some("2026-07-26".into()),
            Some("13:00".into()),
            Some("18:00".into()),
            TZ,
        )
        .expect("both flags still narrow the day between them");
        assert_eq!(both.range_label().as_deref(), Some("13:00-18:00"));

        let defaulted = report::day_window(None, Some("13:00".into()), None, TZ)
            .expect("no --day still defaults to today, unchanged");
        assert_eq!(defaulted.day(), report::Day::today(TZ));

        let inverted = report::day_window(
            Some("2026-07-26".into()),
            Some("18:00".into()),
            Some("09:00".into()),
            TZ,
        );
        assert!(
            matches!(inverted, Err(report::ReportError::InvertedRange { .. })),
            "the half-open ordering refusal is unchanged: {inverted:?}"
        );
    }

    #[test]
    fn an_unreadable_file_does_not_prevent_the_rest_from_being_archived() {
        // Not root-safe by design: root ignores permission bits, so this file would
        // be readable anyway and the test would pass for the wrong reason. That is a
        // known limitation of testing permission-denied behavior as a non-root user,
        // not something worked around here.
        let home =
            std::env::temp_dir().join(format!("cclogger-cli-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".claude/projects/p")).unwrap();
        std::fs::write(home.join(".claude/projects/p/readable-one.jsonl"), b"one\n").unwrap();
        std::fs::write(home.join(".claude/projects/p/blocked.jsonl"), b"blocked\n").unwrap();
        std::fs::write(home.join(".claude/projects/p/readable-two.jsonl"), b"two\n").unwrap();

        let blocked = home.join(".claude/projects/p/blocked.jsonl");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let root = home.join(".cclog");
        let result = run_archive(&home, &root, false);

        // Restore permissions immediately, before any assertion can panic, so the
        // temp directory is always cleanable regardless of what the run found.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            result.is_ok(),
            "one unreadable file must not abort the whole archive run: {result:?}"
        );

        let ledger = Ledger::open(&root).unwrap();
        assert_eq!(
            ledger
                .snapshot_count(".claude/projects/p/readable-one.jsonl")
                .unwrap(),
            1,
            "a readable file alongside an unreadable one must still be archived"
        );
        assert_eq!(
            ledger
                .snapshot_count(".claude/projects/p/readable-two.jsonl")
                .unwrap(),
            1,
            "a readable file after an unreadable one must still be archived"
        );
        assert_eq!(
            ledger
                .snapshot_count(".claude/projects/p/blocked.jsonl")
                .unwrap(),
            0,
            "the unreadable file itself must not have been archived"
        );

        std::fs::remove_dir_all(&home).unwrap();
    }
}
