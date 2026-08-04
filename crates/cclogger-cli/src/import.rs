//! The importer: archived snapshots and the hook spool → ledger observations.
//!
//! This is the piece that connects `cclogger-archive`'s two storage layers -- the
//! content-addressed object store (raw bytes) and the SQLite ledger (observations,
//! checkpoints). It never reads a live vendor directory (`~/.claude`, `~/.codex`): it
//! walks snapshots the ledger already knows about via [`Ledger::find_snapshots`], the
//! same read path `docs/superpowers/specs/expected-output-2026-07-26.md`
//! §9 describes for recovering a day's input set after Claude Code's own ~30-day
//! retention has deleted the originals.
//!
//! # Two vendors, one machine
//!
//! Both vendors [`crate::discover`] archives are imported, and the per-locator
//! machinery below runs once per [`Vendor`]. Only three things differ between them --
//! which `source_kind` names the snapshots, what a whole-file pre-scan can learn from
//! them, and which adapter transforms a record -- so only those three are dispatched.
//! Checkpoints, the torn-tail rule, the prefix check and the gap classification are
//! vendor-independent and shared: a Codex transcript is JSONL that grows by append,
//! exactly like a Claude Code session file.
//!
//! # Three sources, one ingest
//!
//! The third source is the hook spool, drained by [`run_spool`]. It is the one file
//! this module reads off disk rather than out of the archive -- but it is cclog's own
//! file (`<root>/spool/hooks.jsonl`, written by [`crate::hook`]), not a vendor's, and
//! it is JSONL that grows by append like the other two. So it is a third [`Vendor`]
//! variant feeding the *same* machinery, not a second importer: the same torn-tail
//! rule, the same line-count checkpoint, the same [`prefix_check`], the same
//! [`classify_line`], the same dedupe keys, the same [`ImportReport`].
//!
//! What the spool cannot do is reach backwards. Hooks record from the moment they are
//! installed and Claude Code replays nothing, so a spool that starts mid-history is the
//! normal case rather than a fault -- which is why [`ImportReport::spool_begins_at`]
//! exists and is printed.
//!
//! # Per-locator algorithm
//!
//! For each locator (a session file), observations are derived from the **latest**
//! snapshot only -- session files grow by append, so replaying every intermediate
//! snapshot version would just re-derive the same observations the newest one already
//! contains. (One older snapshot is still *read*: the one the checkpoint's cursor was
//! counted against, so [`prefix_check`] can verify the cursor still means what it
//! meant. Nothing is transformed from it.)
//!
//! The [`Checkpoint::cursor`] this importer writes is a count of **complete** lines.
//! "Complete" is load-bearing and is not assumed: `cclogger archive` copies a live
//! session file with a plain `fs::read` while Claude Code may be writing it, so a
//! snapshot can end partway through a record. A snapshot whose bytes do not end in
//! `\n` therefore has a possibly-truncated final line, and [`complete_lines`] drops
//! it from *both* the transformation pass and the cursor: the torn line is neither
//! transformed, nor diagnosed as a parse failure, nor counted as read, so the next
//! snapshot -- which has it whole -- picks it up as new. Counting it as read instead
//! would permanently swallow the completed record, human prompts included, while
//! reporting no gap at all.
//!
//! On each run:
//!
//! 1. If the checkpoint's `snapshot_id` already equals the latest snapshot for this
//!    locator, nothing has changed -- skip without even reading the bytes.
//! 2. Otherwise, read the latest snapshot's bytes once, split into complete lines,
//!    and:
//!    - **verify the prefix property the cursor depends on** ([`prefix_check`]):
//!      complete line `N` of the snapshot the checkpoint was written against must
//!      still be complete line `N` of this one. Append-only growth makes that true,
//!      but nothing enforces append-only, so it is checked rather than assumed. On a
//!      mismatch (or when the older snapshot can no longer be read) the cursor resets
//!      to 0 and the file is rescanned -- safe, because dedupe keys come from stable
//!      record identity, so re-transformed records collapse onto the rows already
//!      there instead of duplicating;
//!    - **pre-scan every line** (not just the new ones) to populate a [`Keystore`]
//!      with deterministic opaque refs (session / workspace / tool / tool_family) and
//!      each tool call's start time -- this guarantees a tool result on a later
//!      line always resolves the call id registered by an earlier line in the
//!      *same* file, regardless of where the checkpoint cursor happens to sit. For
//!      Codex the pre-scan carries more than that: its human-prompt records name
//!      neither a session nor a `cwd`, so without the whole-file view every Codex
//!      observation would be unattributed (see [`CodexPreScan`]);
//!    - **transform only the lines past the checkpoint's cursor** into observations,
//!      turning a JSON parse failure, an unmapped record kind (anything not in that
//!      vendor's adapter's `MAPPED_KINDS`), or a mapped-kind record missing a
//!      field that kind requires into a `dev.cclog.source.gap.v1` marker (design doc
//!      §8) instead of silently dropping it. A mapped-kind record that is well-formed
//!      but legitimately carries no event is counted in
//!      [`ImportReport::records_skipped`], so records-in always reconciles against
//!      observations-out.
//! 3. Ingest the new observations plus the checkpoint advance in one
//!    [`Ledger::ingest`] transaction -- including when there is nothing new to ingest,
//!    so the checkpoint moves onto the newest `snapshot_id` and step 1 short-circuits
//!    next time instead of re-reading the same bytes forever.
//!
//! Re-running with nothing new is therefore cheap (step 1 short-circuits per
//! locator), and idempotent (dedupe keys are derived from stable record identity --
//! `uuid`, `tool_use_id`, or for the Codex records that carry no id at all, a content
//! tuple -- not from anything that changes between runs).

use crate::clock::now_utc_seconds;
use crate::hook;
use cclogger_adapters::{
    Keystore, claude_code, claude_code_history, codex_history, git_log, pseudonymize, rfc3339,
};
use cclogger_archive::{
    CheckpointAdvance, Ledger, LedgerError, ObservationOutcome, SnapshotFilter, SnapshotRef,
};
use cclogger_domain::workspace;
use cclogger_domain::{
    IntegrityState, Observation, ObservationDraft, PrivacyClass, Profile, RuntimeStamp, SourceKind,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A vendor whose archived snapshots this importer knows how to read.
///
/// Exactly three things differ between the two, and this enum is where all three are
/// dispatched: the `source_kind` that names a vendor's snapshots, the whole-file
/// pre-scan that builds its [`Keystore`], and the adapter that transforms one of its
/// records. Everything else in this module -- checkpoints, the torn-tail rule,
/// [`prefix_check`], the gap/skip classification -- is vendor-independent and runs once
/// per locator regardless of which vendor produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vendor {
    ClaudeCode,
    Codex,
    /// The hook spool: lines `cclogger hook` appended live, rather than a transcript
    /// the vendor wrote and `cclogger archive` copied.
    ///
    /// Not a member of [`Vendor::ALL`], and that is the one structural difference. The
    /// archived vendors are driven by [`Ledger::find_snapshots`] -- import reads what
    /// archive already saved -- whereas the spool is cclog's own file, read from disk by
    /// [`run_spool`] and published as a snapshot by the same [`Ledger::ingest`] call
    /// that stores its observations. Everything downstream of "here are the bytes"
    /// (torn tail, checkpoint cursor, prefix check, gap classification, dedupe,
    /// reporting) is the shared code, which is why this is a third variant of this enum
    /// rather than a second importer.
    ClaudeCodeHook,
    /// Commits, collected from a repository the ledger already holds an identity for
    /// ([`run_git`]).
    ///
    /// Not in [`Vendor::ALL`] either, and for a stronger reason than the spool: its
    /// bytes do not exist until this run makes them. `cclogger-cli`'s `git` module runs
    /// `git log` and normalizes what comes back into JSONL, one record per commit, and
    /// *that* is the snapshot -- which is why no commit message or author address is in
    /// the archive either, not merely absent from the row.
    ///
    /// Everything after "here are the bytes" is the shared machinery again:
    /// [`classify_line`], the gap classification, the dedupe key, [`Ledger::ingest`].
    /// The one piece it does not use is the checkpoint *cursor*. A transcript grows by
    /// append, so a line count means "already read"; a git snapshot is a window on a
    /// history, and a commit can enter it at any position (a rebase re-dates one, the
    /// window's far edge slides forward and drops another). Every line is therefore
    /// transformed on every run and collapses onto the row already there -- which is
    /// what the `(repository, sha)` dedupe key is for.
    Git,
}

impl Vendor {
    const ALL: [Vendor; 2] = [Vendor::ClaudeCode, Vendor::Codex];

    /// The `source_kind` this vendor's snapshots and checkpoints are stored under.
    /// Must match what `crate::discover` archives them as, or the importer looks for
    /// rows that are not there.
    fn source_kind(self) -> &'static str {
        match self {
            Vendor::ClaudeCode => "claude-code",
            Vendor::Codex => "codex",
            // Deliberately not "claude-code": the two channels keep separate
            // checkpoints (they read different files), and telling them apart in the
            // ledger is what lets a report say where hook capture *starts* instead of
            // implying it covers the whole history.
            Vendor::ClaudeCodeHook => hook::SPOOL_SOURCE_KIND,
            Vendor::Git => "git",
        }
    }

    /// The canonical `cclogsourcekind` for observations this vendor produces. Only
    /// gap markers need it here -- an adapter stamps its own drafts -- but a gap
    /// marker labelled with the wrong vendor would put every unmapped Codex kind into
    /// Claude Code's coverage report.
    fn domain_kind(self) -> SourceKind {
        match self {
            // The hook channel observes Claude Code too. Which *channel* an
            // observation came through is carried by `cclogsourceversion`
            // (`claude-code-hook/1` vs `claude-code-transcript/1`), which is the field
            // the schema defines for exactly that -- not by inventing a second source
            // kind for one vendor.
            Vendor::ClaudeCode | Vendor::ClaudeCodeHook => SourceKind::ClaudeCode,
            Vendor::Codex => SourceKind::Codex,
            Vendor::Git => SourceKind::Git,
        }
    }

    fn source_version(self) -> &'static str {
        match self {
            Vendor::ClaudeCode => claude_code_history::SOURCE_VERSION,
            Vendor::Codex => codex_history::SOURCE_VERSION,
            Vendor::ClaudeCodeHook => claude_code::SOURCE_VERSION,
            Vendor::Git => git_log::SOURCE_VERSION,
        }
    }

    fn adapter_version(self) -> &'static str {
        match self {
            Vendor::ClaudeCode => claude_code_history::ADAPTER_VERSION,
            Vendor::Codex => codex_history::ADAPTER_VERSION,
            Vendor::ClaudeCodeHook => claude_code::ADAPTER_VERSION,
            Vendor::Git => git_log::ADAPTER_VERSION,
        }
    }

    /// Record kinds this vendor's adapter has a case for. Anything else is an
    /// unmapped-kind gap.
    fn mapped_kinds(self) -> &'static [&'static str] {
        match self {
            Vendor::ClaudeCode => claude_code_history::MAPPED_KINDS,
            Vendor::Codex => codex_history::MAPPED_KINDS,
            Vendor::ClaudeCodeHook => claude_code::MAPPED_HOOKS,
            Vendor::Git => git_log::MAPPED_KINDS,
        }
    }

    /// The kind string this record is matched against [`Vendor::mapped_kinds`] by, and
    /// labelled with in the gap report.
    ///
    /// Claude Code's is the bare `type`. Codex's is `type` plus `payload.type` for the
    /// two envelope types carrying a discriminated union (`event_msg:user_message`),
    /// computed by the adapter's own [`codex_history::kind`] rather than a second copy
    /// here -- two copies is how a match arm and its gap label drift apart.
    fn kind(self, record: &Value) -> String {
        match self {
            Vendor::ClaudeCode => record
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Vendor::Codex => codex_history::kind(record),
            Vendor::ClaudeCodeHook => record
                .get("hook_event_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Vendor::Git => git_log::kind(record),
        }
    }

    fn transform(self, record: &Value, keystore: &Keystore) -> Vec<ObservationDraft> {
        match self {
            Vendor::ClaudeCode => claude_code_history::transform(record, keystore),
            Vendor::Codex => codex_history::transform(record, keystore),
            // One hook event is one observation, so the hook adapter's `Option` is the
            // 0..1 case of the 0..N the other two return.
            Vendor::ClaudeCodeHook => claude_code::transform(record, keystore)
                .into_iter()
                .collect(),
            Vendor::Git => git_log::transform(record, keystore),
        }
    }

    /// A line that says, in its own content, that the *receiver* could make nothing of
    /// the event it was handed -- and why.
    ///
    /// `cclogger hook` must never fail a session, so it swallows its own errors; this
    /// is where the swallowed one resurfaces, as a diagnosed gap marker with the
    /// receiver's reason on it rather than as a line nobody ever looks at. The reason
    /// is matched against [`hook::RECEIVER_ERRORS`] and the *matched constant* is
    /// returned, never the string off the line: a gap `reason` must be a fixed
    /// vocabulary, not text a malformed payload could steer.
    fn self_declared_loss(self, record: &Value) -> Option<&'static str> {
        if self != Vendor::ClaudeCodeHook {
            return None;
        }
        let raw = record.get(hook::RECEIVER_ERROR).and_then(Value::as_str)?;
        Some(
            hook::RECEIVER_ERRORS
                .iter()
                .find(|known| **known == raw)
                .copied()
                // A spool written by a newer receiver, naming a reason this build does
                // not know. Still a loss, still counted -- just without the detail.
                .unwrap_or("receiver_error_unrecognized"),
        )
    }

    /// What `cclogsourcerecordref` points back at.
    ///
    /// Claude Code records carry a `uuid`. Codex records carry no per-record id at all
    /// -- `payload.id` on a tool call is the call id, and the human-prompt record has
    /// nothing -- so a Codex observation is located by line number within the snapshot,
    /// the same locator the gap path uses.
    fn record_ref(self, record: &Value, locator_ref: &str) -> String {
        match self {
            Vendor::ClaudeCode => record
                .get("uuid")
                .and_then(Value::as_str)
                .unwrap_or(locator_ref)
                .to_string(),
            // Neither a Codex record nor a spool line carries a per-record id, so both
            // are located by line number within the snapshot. A git record has the one
            // genuinely stable id in this whole codebase -- the sha -- and is located by
            // line number anyway: `cclogsourcerecordref` is stored on the observation,
            // and the sha reaches a row only as the pseudonym the adapter derives from
            // it (see `git_log`'s header).
            Vendor::Codex | Vendor::ClaudeCodeHook | Vendor::Git => locator_ref.to_string(),
        }
    }

    /// The instant this record is dated to, and which clock that reading came from.
    ///
    /// Per-vendor because the field differs and, more importantly, because what it
    /// *means* differs. A transcript record's `timestamp` is the source's own wall
    /// clock at the moment of the activity ([`TimeBasis::Occurred`]). A spool line's
    /// `received_at` is when cclog's receiver was invoked -- no hook event carries a
    /// timestamp at all, so there is no source clock to read ([`TimeBasis::Received`]).
    /// Collapsing the two into one field name would erase exactly the distinction the
    /// `time_basis` machinery exists to keep.
    fn record_time(self, record: &Value, acquired_at: &str) -> (String, TimeBasis) {
        let (field, basis) = match self {
            Vendor::ClaudeCode | Vendor::Codex => ("timestamp", TimeBasis::Occurred),
            Vendor::ClaudeCodeHook => (hook::RECEIVED_AT, TimeBasis::Received),
            // A commit's author time: when the work was written, and still that after a
            // rebase re-dates the commit itself.
            Vendor::Git => ("author_time", TimeBasis::Occurred),
        };
        match record.get(field).and_then(Value::as_str) {
            Some(t) => (t.to_string(), basis),
            None => (acquired_at.to_string(), TimeBasis::Acquired),
        }
    }

    /// Build this vendor's [`Keystore`] from every complete line of one snapshot, plus
    /// the identities the caller should write to the registry.
    ///
    /// `codex_subagent_threads` is only ever consulted by the `Vendor::Codex` arm --
    /// every other vendor ignores it. It cannot be learned here: a subagent's parent
    /// and child are two different snapshots, and this function is called once per
    /// snapshot and its `Keystore` is discarded before the next one is even read (see
    /// [`codex_subagent_thread_ids`]'s doc comment). The caller collects it once,
    /// across every locator, before the per-locator loop that calls this even starts.
    fn pre_scan(
        self,
        lines: &[&str],
        home: &str,
        codex_subagent_threads: &std::collections::HashSet<String>,
    ) -> (Keystore, BTreeMap<String, (String, String)>) {
        match self {
            Vendor::ClaudeCode => {
                let mut scan = PreScan::default();
                for line in lines {
                    if let Ok(record) = serde_json::from_str::<Value>(line) {
                        scan.observe(&record, home);
                    }
                }
                scan.finish()
            }
            Vendor::Codex => {
                let mut scan = CodexPreScan::default();
                for line in lines {
                    if let Ok(record) = serde_json::from_str::<Value>(line) {
                        scan.observe(&record, home);
                    }
                }
                scan.finish(codex_subagent_threads)
            }
            // The same [`PreScan`] the transcript channel uses, including its
            // session-level majority vote: a spool line normally carries its own `cwd`
            // (a documented common field), but the vote is what places one that does
            // not, exactly as it does for a cwd-less transcript record. Only the
            // per-record part differs -- see [`PreScan::observe_hook`].
            Vendor::ClaudeCodeHook => {
                let mut scan = PreScan::default();
                for line in lines {
                    if let Ok(record) = serde_json::from_str::<Value>(line) {
                        scan.observe_hook(&record, home);
                    }
                }
                scan.finish()
            }
            // No cwd to resolve and no session to vote on: a git record names its
            // repository outright (cclog wrote the record, from an identity the ledger
            // already held), so the only work here is deriving the two pseudonyms.
            Vendor::Git => {
                let mut scan = GitPreScan::default();
                for line in lines {
                    if let Ok(record) = serde_json::from_str::<Value>(line) {
                        scan.observe(&record);
                    }
                }
                scan.finish()
            }
        }
    }

    /// The field this record of a *mapped* kind is missing, if that is why the adapter
    /// produced nothing. `None` means the record is well-formed and legitimately
    /// carries no event -- a counted skip rather than a gap.
    fn missing_required_field(self, record: &Value, keystore: &Keystore) -> Option<&'static str> {
        match self {
            Vendor::ClaudeCode => missing_required_field(record),
            Vendor::Codex => codex_missing_required_field(record, keystore),
            Vendor::ClaudeCodeHook => hook_missing_required_field(record),
            Vendor::Git => git_log::missing_field(record),
        }
    }

    /// One flag per complete line: whether that line is **copied history** rather than
    /// a record of something that happened when its timestamp says.
    ///
    /// A file-level structural question, which is why it is answered here rather than
    /// per record inside a (pure, single-record) adapter: whether a line was copied is
    /// decided by other lines of the same file -- what the first `session_meta` says
    /// about this thread's lineage, and which lines share the opening write's
    /// millisecond. See [`codex_inherited`] for the rule.
    ///
    /// Claude Code has nothing of the kind. It duplicates records into a resumed
    /// session's new file too, but it copies them *with their original timestamps* --
    /// which is exactly why the ledger's dedupe collapses a resume copy onto the row
    /// already there. A copy that kept the original time is not a wrong time.
    fn inherited_lines(self, lines: &[&str]) -> Vec<bool> {
        match self {
            // Nor does the hook channel: a spool line is written once, by the receiver,
            // at the moment the event reached it. There is no copying step that could
            // re-stamp one.
            // Nor git: a commit's author time is written once, by the person who wrote
            // it, and copying a commit between repositories keeps it.
            Vendor::ClaudeCode | Vendor::ClaudeCodeHook | Vendor::Git => vec![false; lines.len()],
            Vendor::Codex => codex_inherited(lines).per_line(lines),
        }
    }
}

/// Everything [`run_import`] did, for the CLI to print honestly.
///
/// The record-side counters are designed to *close*: every complete line the run
/// looked at ends up in exactly one of `observations_created` /
/// `observations_already_present` (via the drafts it produced), `gap_parse_error`,
/// `gap_unmapped_kind`, `gap_missing_field`, or `records_skipped`. Without that last
/// pair a record could be understood, produce nothing, and appear in no counter at
/// all -- which is the silent swallow the gap machinery exists to prevent.
#[derive(Debug, Default)]
pub struct ImportReport {
    pub locators_scanned: usize,
    pub locators_processed: usize,
    pub locators_unchanged: usize,
    pub locators_unreadable: usize,
    /// Locators whose checkpoint cursor failed [`prefix_check`] and was reset to 0
    /// for a full rescan. Reported rather than hidden: a nonzero count means a
    /// snapshot was not the append-only growth of its predecessor.
    pub checkpoints_reset: usize,
    /// Snapshots whose bytes stopped partway through a record, so their final line was
    /// deferred to a later snapshot rather than transformed ([`ends_mid_record`]). At
    /// most one per locator, since only the last line can be torn.
    ///
    /// Deferred is normally the right answer -- the next `cclogger archive` picks the
    /// record up whole -- but it is not *free*: if the vendor deletes the session file
    /// before another snapshot is taken, that last record is never imported and never
    /// gapped either, because a fragment cannot be parsed or identified well enough to
    /// diagnose. This counter is the only place that outcome is visible, which is why
    /// it is reported even though it is usually benign.
    pub lines_incomplete: u64,
    /// New observations, by `event_type`.
    pub observations_created: BTreeMap<String, u64>,
    /// Observations transformed again but already present in the ledger (a re-run
    /// over content whose checkpoint had not yet advanced, or an exact resume
    /// duplicate).
    pub observations_already_present: u64,
    pub gap_parse_error: u64,
    /// Gap markers for an unmapped record `type`, by that type's name (or
    /// [`UNPRINTABLE_KIND`] when the vendor `type` string is not a plausible kind
    /// name -- see [`kind_detail`]).
    pub gap_unmapped_kind: BTreeMap<String, u64>,
    /// Gap markers for a record whose `type` *is* mapped but which is missing a field
    /// that kind requires (`sessionId` / `timestamp` / `uuid`), by field name.
    pub gap_missing_field: BTreeMap<String, u64>,
    /// Gap markers for a spool line on which `cclogger hook` recorded its own failure
    /// (`payload_not_an_object`, `payload_oversize`, ...), by reason.
    ///
    /// A fourth gap bucket rather than a reuse of one of the three above, because it
    /// says something none of them does: the loss happened at *capture*, before any of
    /// this ran, and no re-import can recover it. The receiver exits 0 on every
    /// internal error so it can never fail a session -- this counter is where that
    /// silence stops.
    pub gap_receiver_error: BTreeMap<String, u64>,
    /// Records the adapter understood and deliberately produced no event for (a
    /// sidechain non-tool-result `user` record, an `attachment` that is not a
    /// `SessionStart` hook success), by record `type`. Not a gap -- but counted, so
    /// records-in reconciles against observations-out.
    pub records_skipped: BTreeMap<String, u64>,
    /// Transformed observations that ended up with no repository identity: their
    /// record carried a cwd outside the ghq tree, or carried none and its session
    /// never named one. Reported so an unattributed tail is visible rather than
    /// merely absent from every per-repository total.
    ///
    /// Counts observations, not records -- one record can produce several. The name
    /// says so; do not relabel it `records_*` without changing what it counts.
    ///
    /// **Gap markers are excluded**, and deliberately so: a gap stands for a line
    /// that could not be transformed at all, so it never had an attribution to lose.
    /// Including them would peg this number at no less than the gap count and make
    /// it useless as a signal -- on the real corpus that is 36,842 gaps against
    /// 11,255 genuinely unattributed observations. Query
    /// `WHERE repository_ref IS NULL` on the ledger to get both together.
    pub observations_unattributed: u64,
    /// Observations derived from **copied history** -- records a Codex fork or
    /// subagent spawn re-wrote into a child's transcript, carrying the copy's write
    /// time rather than their own ([`codex_inherited`]).
    ///
    /// They are imported, not dropped, and marked `data.time_basis = "copied_at"` so
    /// no clock reads their timestamp as an event time. Counted here so a run says how
    /// many it saw: on the surveyed corpus this is 9.2% of the ledger and 78% of every
    /// Codex human prompt, which is not a number that should have to be discovered.
    ///
    /// Counts observations, not records -- one record can produce several -- and
    /// includes the gap markers copied lines produce, which are excluded from clocks
    /// twice over. Orthogonal to the outcome counters above rather than a fourth
    /// alternative to them: every copied line is *also* counted in exactly one of
    /// created / already-present / gap / skipped.
    pub observations_inherited: u64,
    /// Spool lines this run turned into observations (or gap markers) -- the ones past
    /// the checkpoint cursor, not the whole file.
    pub spool_lines_drained: u64,
    /// The `received_at` on the spool's **first** line: the instant before which this
    /// machine has no hook-channel evidence at all.
    ///
    /// Reported because a spool that starts mid-history is the normal case, not an
    /// anomaly: hooks record from the moment they are installed, Claude Code buffers
    /// nothing and replays nothing, and no re-run can reach further back. A number of
    /// hook observations printed without this would read as coverage of the whole
    /// period. `None` when there is no spool, or it is empty.
    pub spool_begins_at: Option<String>,
    /// Repositories the ledger holds an identity for, and so looked for on disk.
    ///
    /// The git counters are kept apart from the locator counters above rather than
    /// folded into them: a repository is not a file, "unchanged" means something
    /// different for a window on a history than for an append-only transcript, and the
    /// reasons a repository contributes nothing have no analogue on the transcript side.
    pub git_repositories_scanned: usize,
    /// Repositories a `git log` actually ran against.
    pub git_repositories_collected: usize,
    /// Repositories whose collected history was byte-identical to the last one
    /// imported, so nothing was re-transformed.
    pub git_repositories_unchanged: usize,
    /// Commit records collected this run, before dedupe. Not the number of new
    /// observations: the window is re-walked every time, so most of these are commits
    /// already in the ledger (see [`ImportReport::observations_already_present`]).
    pub git_commits_collected: u64,
    /// Repositories whose history hit the per-repository ceiling, so the oldest
    /// commits in the window were not collected.
    pub git_repositories_truncated: usize,
    /// Why a repository the ledger knows about contributed no commits, by reason
    /// (`missing`, `not_a_repository`, `no_commits_yet`, `no_identity`, `unreadable`,
    /// `timed_out`).
    ///
    /// Every one of these is a *stated gap*: the ledger says work happened in that
    /// repository, and this run could not read it. Reported rather than skipped, so a
    /// repository that has been moved or deleted shows up as evidence that is missing
    /// rather than as a repository that had no commits.
    pub git_repositories_unresolved: BTreeMap<&'static str, u64>,
    /// True only for `--dry-run` against a root with no `ledger.db`: dry run refuses
    /// to create one, so there was nothing to read.
    pub ledger_missing: bool,
    /// True only for `--dry-run` against a ledger whose schema predates this build:
    /// opening it would upgrade and re-stamp it, which is a write, so the dry run
    /// stops instead and says so. Distinct from [`ImportReport::ledger_missing`]
    /// because the remedy differs -- there is a ledger, it just has to be opened for
    /// real once before anything can be reported about it.
    pub ledger_needs_upgrade: bool,
}

impl ImportReport {
    fn record_outcome(&mut self, event_type: &str, outcome: ObservationOutcome) {
        match outcome {
            ObservationOutcome::Created => {
                *self
                    .observations_created
                    .entry(event_type.to_string())
                    .or_insert(0) += 1;
            }
            ObservationOutcome::AlreadyPresent => self.observations_already_present += 1,
        }
    }

    fn record_gap(&mut self, bucket: &GapBucket) {
        match bucket {
            GapBucket::ParseError => self.gap_parse_error += 1,
            GapBucket::UnmappedKind(label) => {
                *self.gap_unmapped_kind.entry(label.clone()).or_insert(0) += 1;
            }
            GapBucket::MissingField(field) => {
                *self
                    .gap_missing_field
                    .entry((*field).to_string())
                    .or_insert(0) += 1;
            }
            GapBucket::ReceiverError(reason) => {
                *self
                    .gap_receiver_error
                    .entry((*reason).to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    pub fn total_gaps(&self) -> u64 {
        self.gap_parse_error
            + self.gap_unmapped_kind.values().sum::<u64>()
            + self.gap_missing_field.values().sum::<u64>()
            + self.gap_receiver_error.values().sum::<u64>()
    }
}

/// How far back a run collects commits, as the CLI states it. Read from the same
/// [`crate::git::Scan`] default the import uses, so the printed number cannot drift from
/// the window actually walked.
pub const GIT_WINDOW_DAYS: i64 = crate::git::Scan::DEFAULT_SINCE_DAYS;

/// Runs the historical import for every [`Vendor`]. Returns `Ok(report)` whether or not gaps
/// were found (gaps are an honestly-diagnosed, expected outcome, not a failure); an
/// `Err` means a hard failure (the ledger itself could not be opened, or a query
/// against it failed). An individual unreadable snapshot object does not abort the
/// run -- it is counted in [`ImportReport::locators_unreadable`] instead, the same
/// "one bad file must not strand the rest" policy `cclogger archive` already follows --
/// but the CLI treats a nonzero count of those as a non-clean outcome for its exit
/// code.
pub fn run_import(root: &Path, dry_run: bool) -> Result<ImportReport, LedgerError> {
    // `--dry-run` prints "nothing written", so it must write nothing -- and
    // `Ledger::open` creates the root, `archive/`, and `ledger.db` as a side effect
    // of opening, while `device_id` mints and persists a file. Against a root with
    // no ledger there is nothing to import anyway, so refuse to open rather than
    // bringing four filesystem entries into existence and then claiming otherwise.
    if dry_run && !root.join("ledger.db").exists() {
        return Ok(ImportReport {
            ledger_missing: true,
            ..Default::default()
        });
    }
    // The other write `Ledger::open` performs as a side effect of opening: it
    // reconciles an out-of-date schema and stamps `user_version` forward. That one is
    // worse than creating a directory, because it is not reversible from the user's
    // side -- an older cclog refuses a ledger stamped past what it understands. So a
    // dry run asks first and stops, rather than upgrading a ledger the user only
    // asked it to describe.
    if dry_run && cclogger_archive::needs_schema_upgrade(root)? {
        return Ok(ImportReport {
            ledger_needs_upgrade: true,
            ..Default::default()
        });
    }

    let mut ledger = Ledger::open(root)?;
    let device = if dry_run {
        // Read, never mint. If no device id has been persisted yet then no import
        // has ever written an observation, so every dedupe key derived from this
        // placeholder is correctly absent from the ledger -- the same answer a real
        // first run (which would mint its own fresh device id) arrives at.
        read_device_id(root).unwrap_or_else(|| DRY_RUN_DEVICE.to_string())
    } else {
        device_id(root).map_err(LedgerError::Io)?
    };
    let observed_at = now_utc_seconds();
    // The historical cwds were recorded on this machine, so this machine's home is the
    // right prefix to strip. A cwd archived on a different machine will not resolve --
    // correctly, since this build cannot know that machine's layout.
    let home = std::env::var("HOME").unwrap_or_default();

    let mut report = ImportReport::default();
    // Dedupe keys already counted by this `--dry-run` invocation. Declared outside both
    // loops because the duplicates that matter most are *cross*-locator: a resumed
    // session file repeats the earlier file's records verbatim. (Cross-*vendor* it can
    // never collide -- `finalize` prefixes every key with the source-kind slug -- so
    // one set serves both without conflating them.)
    let mut dry_run_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for vendor in Vendor::ALL {
        let snapshots = ledger.find_snapshots(&SnapshotFilter {
            source_kind: Some(vendor.source_kind()),
            ..Default::default()
        })?;

        let mut latest_by_locator: BTreeMap<String, cclogger_archive::Snapshot> = BTreeMap::new();
        // Every snapshot by id, so a checkpoint can be resolved back to the exact bytes
        // its cursor was counted against (see `prefix_check`) without a second query.
        let mut by_snapshot_id: BTreeMap<i64, cclogger_archive::Snapshot> = BTreeMap::new();
        for snapshot in snapshots {
            by_snapshot_id.insert(snapshot.snapshot_id, snapshot.clone());
            latest_by_locator
                .entry(snapshot.source_locator.clone())
                .and_modify(|current| {
                    if snapshot.snapshot_id > current.snapshot_id {
                        *current = snapshot.clone();
                    }
                })
                .or_insert(snapshot);
        }

        // A Codex subagent gets its own rollout file, so nothing on that file's own
        // records marks it as one -- the only evidence is a *different* file's
        // `sub_agent_activity` naming this file's thread. That means the fact has to
        // be collected across every Codex locator before the per-locator loop below
        // touches any one of them individually; see `codex_subagent_thread_ids`.
        let subagent_threads = if vendor == Vendor::Codex {
            codex_subagent_thread_ids(&ledger, &latest_by_locator)
        } else {
            std::collections::HashSet::new()
        };

        for (locator, snapshot) in latest_by_locator {
            report.locators_scanned += 1;

            let checkpoint = ledger.checkpoint(vendor.source_kind(), &locator)?;
            if let Some(cp) = &checkpoint
                && cp.snapshot_id == snapshot.snapshot_id
            {
                // Nothing has changed for this locator since the last run.
                report.locators_unchanged += 1;
                continue;
            }
            let recorded_cursor: usize = checkpoint
                .as_ref()
                .and_then(|cp| cp.cursor.as_deref())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let bytes = match ledger.read(&snapshot.object_id) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("skip {locator}: {e}");
                    report.locators_unreadable += 1;
                    continue;
                }
            };
            let text = String::from_utf8_lossy(&bytes);
            let lines = complete_lines(&text, &bytes);
            if ends_mid_record(&bytes) {
                report.lines_incomplete += 1;
            }

            let mut start_line = recorded_cursor;
            if let (Some(cp), true) = (checkpoint.as_ref(), recorded_cursor > 0)
                && let Err(why) = prefix_check(
                    &ledger,
                    &by_snapshot_id,
                    cp.snapshot_id,
                    &lines,
                    recorded_cursor,
                )
            {
                // Safe to rescan: dedupe keys come from stable record identity, so
                // every re-transformed record collapses onto the row already there.
                eprintln!("{locator}: cursor reset to 0 and rescanned -- {why}");
                report.checkpoints_reset += 1;
                start_line = 0;
            }

            // Pre-scan the WHOLE file (not just the new lines) so a tool result on a
            // new line always resolves a call id -- and its start time -- registered by
            // an earlier line in the same file, even if that earlier line was already
            // ingested on a prior run. For Codex it also carries the session and its
            // workspace across from the `session_meta` that names them, which no other
            // Codex record does.
            let (keystore, identities) = vendor.pre_scan(&lines, &home, &subagent_threads);
            // Which lines are copied history rather than live activity -- a whole-file
            // question, answered once here for the same reason the pre-scan is: an
            // adapter sees one record and cannot see the file it sits in.
            let inherited = vendor.inherited_lines(&lines);
            // `--dry-run` prints "nothing written", so it must write nothing -- the
            // same rule that already keeps `ledger.ingest` out of the dry-run path a
            // few lines below. The keystore is what transformation needs; the registry
            // is only for display.
            if !dry_run {
                for (opaque, (kind, display)) in &identities {
                    ledger.register_identity(opaque, kind, display, &observed_at)?;
                }
            }

            let mut observations: Vec<Observation> = Vec::new();
            for (idx, line) in lines.iter().enumerate().skip(start_line) {
                match classify_line(
                    vendor,
                    line,
                    &keystore,
                    snapshot.object_id.as_str(),
                    &snapshot.acquired_at,
                    idx + 1,
                    inherited[idx],
                ) {
                    LineOutcome::Drafts { record_ref, drafts } => {
                        for draft in drafts {
                            if draft.repository_ref.is_none() {
                                report.observations_unattributed += 1;
                            }
                            if inherited[idx] {
                                report.observations_inherited += 1;
                            }
                            observations.push(finalize(draft, &record_ref, &device, &observed_at));
                        }
                    }
                    LineOutcome::Gap { draft, bucket } => {
                        let record_ref = format!("line:{}", idx + 1);
                        if inherited[idx] {
                            report.observations_inherited += 1;
                        }
                        observations.push(finalize(*draft, &record_ref, &device, &observed_at));
                        report.record_gap(&bucket);
                    }
                    LineOutcome::Skipped { kind } => {
                        *report.records_skipped.entry(kind.to_string()).or_insert(0) += 1;
                    }
                }
            }

            report.locators_processed += 1;

            if dry_run {
                // Dedupe is evaluated by reading, not by writing -- see
                // `account_dry_run` for the two kinds of duplication it has to model.
                account_dry_run(&ledger, &observations, &mut dry_run_seen, &mut report)?;
                continue;
            }

            // Runs even when `observations` is empty: the point of this call is also to
            // move the checkpoint onto the newest snapshot_id, so a locator whose only
            // change was a still-incomplete final line stops being re-read on every
            // future run.
            let ingest_report = ledger.ingest(
                SnapshotRef {
                    source_kind: vendor.source_kind(),
                    source_locator: &locator,
                    bytes: &bytes,
                    acquired_at: &snapshot.acquired_at,
                    format_fingerprint: snapshot.format_fingerprint.as_deref(),
                },
                &observations,
                CheckpointAdvance {
                    cursor: Some(&lines.len().to_string()),
                    updated_at: &observed_at,
                },
            )?;

            for (obs, outcome) in observations.iter().zip(ingest_report.observations.iter()) {
                report.record_outcome(&obs.event_type, *outcome);
            }
        }
    }

    let run = Run {
        root,
        home: &home,
        device: &device,
        observed_at: &observed_at,
        dry_run,
    };
    run_spool(&mut ledger, &run, &mut dry_run_seen, &mut report)?;
    // Last, and reading the ledger's own identity registry, so it sees every repository
    // the transcripts above just registered rather than only the ones a previous run
    // had already recorded.
    run_git(
        &mut ledger,
        &run,
        &crate::git::Scan::default(),
        &mut dry_run_seen,
        &mut report,
    )?;

    Ok(report)
}

/// Every thread id any Codex snapshot in this run names, as the `agent_thread_id` of a
/// `sub_agent_activity` record -- i.e. every thread some file claims to have spawned as
/// a subagent.
///
/// A whole pass over every Codex locator, run once **before** the per-locator loop in
/// [`run_import`] touches any one of them individually. It has to run first: a Codex
/// subagent gets its own rollout file, so nothing on that file's own records marks it
/// as one -- the only evidence is a *different* file's `sub_agent_activity` record --
/// and [`Vendor::pre_scan`] builds and discards one snapshot's [`Keystore`] before the
/// next snapshot is even read. A fact that only ever appears in a different file than
/// the one it is about cannot be learned by that per-snapshot pass, no matter where in
/// it the check is placed.
///
/// Cheap by construction: measured over the real corpus, 196 of 347 Codex snapshots
/// contain a `sub_agent_activity` record at all, so the other 44% are skipped -- every
/// line of them -- without `serde_json` ever looking at one.
///
/// An unreadable snapshot contributes nothing here and is silently skipped; it is not
/// double-counted as a failure, because the per-locator loop right after this one is
/// the pass responsible for [`ImportReport::locators_unreadable`].
fn codex_subagent_thread_ids(
    ledger: &Ledger,
    latest_by_locator: &BTreeMap<String, cclogger_archive::Snapshot>,
) -> std::collections::HashSet<String> {
    let mut threads = std::collections::HashSet::new();
    for snapshot in latest_by_locator.values() {
        let Ok(bytes) = ledger.read(&snapshot.object_id) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in complete_lines(&text, &bytes) {
            // Reject before `serde_json` ever touches the line: most lines, even in a
            // snapshot that does contain a `sub_agent_activity` record somewhere, are
            // something else entirely -- a tool call, a token count, the human's own
            // prompt.
            if !line.contains("sub_agent_activity") {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if record.get("type").and_then(Value::as_str) != Some("event_msg") {
                continue;
            }
            let Some(payload) = record.get("payload") else {
                continue;
            };
            if payload.get("type").and_then(Value::as_str) != Some("sub_agent_activity") {
                continue;
            }
            if let Some(thread) = payload.get("agent_thread_id").and_then(Value::as_str)
                && !thread.is_empty()
            {
                threads.insert(thread.to_string());
            }
        }
    }
    threads
}

/// What one `run_import` invocation settled before any source was read: where to look,
/// whose home the cwds are relative to, which device is writing, when this run started,
/// and whether it may write at all.
///
/// Gathered into a struct for the same reason [`SnapshotRef`] is -- a function taking
/// four `&str`s in a row is a function whose arguments can be swapped silently -- and
/// so `run_spool` stays inside the argument count clippy allows without an `allow`
/// attribute, which this workspace carries none of.
struct Run<'a> {
    root: &'a Path,
    /// The home the archived cwds were recorded under, for repository resolution.
    home: &'a str,
    device: &'a str,
    /// `observed_at`: this run's own wall clock.
    observed_at: &'a str,
    dry_run: bool,
}

/// Drain the hook spool into the same ledger, through the same ingest.
///
/// The spool is the third source, and the only one that is not an archived snapshot:
/// `cclogger hook` appends to `<root>/spool/hooks.jsonl` live, so this reads that file
/// rather than an object the archive already holds. Everything after "here are the
/// bytes" is the machinery the vendor loop above already uses -- [`complete_lines`]'s
/// torn-tail rule, the line-count checkpoint, [`prefix_check`], [`classify_line`]'s
/// gap/skip classification, `finalize`'s dedupe key, and [`Ledger::ingest`], which
/// publishes the spool's bytes as a snapshot in the same transaction that writes the
/// observations and moves the checkpoint.
///
/// **Drained exactly once**, twice over. The checkpoint cursor means a re-run
/// transforms only lines past it, and the dedupe key means a line that *is*
/// re-transformed (after a [`prefix_check`] reset) collapses onto the row already
/// there. Neither alone would be enough: without the cursor a re-run would re-read the
/// whole spool forever, and without stable dedupe keys a reset would duplicate.
///
/// **Nothing is deleted.** "Drain" here means "advance past", not "truncate": the
/// alternative is to remove lines this process believes it has committed, and a bug in
/// that belief is unrecoverable, where a spool that keeps its lines is merely larger. It
/// is also the raw evidence for its own observations, exactly as the archive is for the
/// transcript channel.
fn run_spool(
    ledger: &mut Ledger,
    run: &Run,
    dry_run_seen: &mut std::collections::HashSet<String>,
    report: &mut ImportReport,
) -> Result<(), LedgerError> {
    let Run {
        root,
        home,
        device,
        observed_at,
        dry_run,
    } = *run;
    let vendor = Vendor::ClaudeCodeHook;
    let bytes = match std::fs::read(hook::spool_path(root)) {
        Ok(bytes) => bytes,
        // No spool at all is the ordinary state of an installation with no hooks
        // registered. Not a gap, not an error, and not something to announce.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            eprintln!("skip {}: {e}", hook::SPOOL_LOCATOR);
            report.locators_unreadable += 1;
            return Ok(());
        }
    };
    report.locators_scanned += 1;

    let text = String::from_utf8_lossy(&bytes);
    let lines = complete_lines(&text, &bytes);
    if ends_mid_record(&bytes) {
        // A hook process was mid-append when this read the file. The record is picked
        // up whole on the next run -- the same deferral the transcript channel makes.
        report.lines_incomplete += 1;
    }
    // Read from the *whole* file, not from the lines this run drains: the question it
    // answers is "how far back does hook capture reach at all", which does not change
    // when a run has nothing new to do.
    report.spool_begins_at = lines
        .first()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|line| {
            line.get(hook::RECEIVED_AT)
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    let checkpoint = ledger.checkpoint(vendor.source_kind(), hook::SPOOL_LOCATOR)?;
    let recorded_cursor: usize = checkpoint
        .as_ref()
        .and_then(|cp| cp.cursor.as_deref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut start_line = recorded_cursor;
    if let (Some(cp), true) = (checkpoint.as_ref(), recorded_cursor > 0) {
        // The spool grows by append, but nothing enforces that -- someone can rotate or
        // truncate it -- so the property the cursor rests on is checked, exactly as it
        // is for a transcript. A reset re-transforms lines already ingested, which is
        // safe: their dedupe keys are unchanged, so they collapse onto the rows there.
        let by_snapshot_id: BTreeMap<i64, cclogger_archive::Snapshot> = ledger
            .snapshots_for_locator(hook::SPOOL_LOCATOR)?
            .into_iter()
            .map(|s| (s.snapshot_id, s))
            .collect();
        if let Err(why) = prefix_check(
            ledger,
            &by_snapshot_id,
            cp.snapshot_id,
            &lines,
            recorded_cursor,
        ) {
            eprintln!(
                "{}: cursor reset to 0 and rescanned -- {why}",
                hook::SPOOL_LOCATOR
            );
            report.checkpoints_reset += 1;
            start_line = 0;
        }
    }
    if start_line >= lines.len() {
        report.locators_unchanged += 1;
        return Ok(());
    }

    // The whole file, not just the new lines: a `PostToolUse` on a new line has to
    // resolve the session, turn and workspace refs that a `UserPromptSubmit` on an
    // already-ingested line registered, or it would land unattributed.
    //
    // `vendor` here is always `Vendor::ClaudeCodeHook`, never Codex, so there is no
    // cross-file subagent set to pass -- an empty one is simply never consulted.
    let (keystore, identities) = vendor.pre_scan(&lines, home, &std::collections::HashSet::new());
    if !dry_run {
        for (opaque, (kind, display)) in &identities {
            ledger.register_identity(opaque, kind, display, observed_at)?;
        }
    }

    // The digest `ingest` will publish these bytes under, computed the same way
    // (sha256 over the whole file, `sha256:<hex>`) so a gap marker's identity is
    // derived from snapshot + line here exactly as it is on the transcript path.
    let digest = snapshot_digest(&bytes);
    let mut observations: Vec<Observation> = Vec::new();
    for (idx, line) in lines.iter().enumerate().skip(start_line) {
        report.spool_lines_drained += 1;
        match classify_line(
            vendor,
            line,
            &keystore,
            &digest,
            // A spool line with no `received_at` falls back to this, flagged
            // `acquired_at`. It is when this import ran, not when anything happened --
            // which is why it is marked rather than quietly used.
            observed_at,
            idx + 1,
            false,
        ) {
            LineOutcome::Drafts { record_ref, drafts } => {
                for draft in drafts {
                    if draft.repository_ref.is_none() {
                        report.observations_unattributed += 1;
                    }
                    observations.push(finalize(draft, &record_ref, device, observed_at));
                }
            }
            LineOutcome::Gap { draft, bucket } => {
                let record_ref = format!("line:{}", idx + 1);
                observations.push(finalize(*draft, &record_ref, device, observed_at));
                report.record_gap(&bucket);
            }
            LineOutcome::Skipped { kind } => {
                *report.records_skipped.entry(kind.to_string()).or_insert(0) += 1;
            }
        }
    }
    report.locators_processed += 1;

    if dry_run {
        account_dry_run(ledger, &observations, dry_run_seen, report)?;
        return Ok(());
    }

    let ingest_report = ledger.ingest(
        SnapshotRef {
            source_kind: vendor.source_kind(),
            source_locator: hook::SPOOL_LOCATOR,
            bytes: &bytes,
            // The spool's own lines carry the arrival times; this is when the import
            // that read them ran, which is what `acquired_at` means for every other
            // snapshot too.
            acquired_at: observed_at,
            format_fingerprint: None,
        },
        &observations,
        CheckpointAdvance {
            cursor: Some(&lines.len().to_string()),
            updated_at: observed_at,
        },
    )?;
    for (obs, outcome) in observations.iter().zip(ingest_report.observations.iter()) {
        report.record_outcome(&obs.event_type, *outcome);
    }
    Ok(())
}

/// Collect commits from every repository the ledger holds an identity for, and ingest
/// them through the same path everything else goes through.
///
/// **Which repositories** is answered by the ledger, not by the disk and not by a config
/// file: `workspace_identity` already holds one row per repository the person has worked
/// in, as a normalized `host/owner/repo`. That means no repository is ever looked at
/// unless AI work was already observed in it, and it means the identity a commit lands
/// under is *the same pseudonym* the sessions in that repository carry -- which is the
/// only reason `log` can put a commit next to the block it landed in.
///
/// It also means the path has to be reconstructed from the identity, and that
/// reconstruction can be wrong: a repository can be moved, renamed or deleted after the
/// ledger last saw work in it. Every one of those outcomes is counted in
/// [`ImportReport::git_repositories_unresolved`] and printed. A repository that cannot
/// be read is a gap in the evidence, not a repository with no commits.
///
/// **Nothing is re-collected cheaply.** Unlike the two transcript paths, there is no
/// cursor to advance past: a git snapshot is a window on a history, not a file that
/// grows by append, and a commit can appear anywhere in it (a rebase re-dates one; the
/// window's far edge slides forward and drops another). So the whole window is walked
/// every run and every record is re-transformed, and what keeps a re-import from
/// duplicating is the dedupe key -- `(repository, sha)` -- exactly as it keeps a
/// re-scanned transcript from duplicating. The one short-circuit is byte equality: a
/// repository whose collected history is identical to the snapshot already stored, and
/// whose checkpoint already points at it, is skipped without transforming anything.
///
/// `--dry-run` still runs `git log`: it reads and writes nothing, and the point of the
/// dry run is to say what a real one would ingest.
fn run_git(
    ledger: &mut Ledger,
    run: &Run,
    scan: &crate::git::Scan,
    dry_run_seen: &mut std::collections::HashSet<String>,
    report: &mut ImportReport,
) -> Result<(), LedgerError> {
    let vendor = Vendor::Git;
    let home = Path::new(run.home);

    for (_, identity) in ledger.identities("repository")? {
        report.git_repositories_scanned += 1;
        let records = match crate::git::scan_repository(home, &identity, scan) {
            crate::git::RepositoryScan::Collected { records, truncated } => {
                if truncated {
                    report.git_repositories_truncated += 1;
                }
                records
            }
            other => {
                *report
                    .git_repositories_unresolved
                    .entry(unresolved_reason(&other))
                    .or_insert(0) += 1;
                continue;
            }
        };
        report.git_repositories_collected += 1;
        report.git_commits_collected += records.len() as u64;

        // The locator is the normalized identity, never a path: it is stored in the
        // ledger and printed, and a path would carry the username. It is also what the
        // identity registry already holds, so the two are readable side by side.
        let locator = format!("git/{identity}");
        let mut bytes = records.join("\n");
        if !bytes.is_empty() {
            bytes.push('\n');
        }
        let bytes = bytes.into_bytes();
        let digest = snapshot_digest(&bytes);

        // Step 1 of the per-locator algorithm, in the only form available here: the
        // bytes are identical to the ones already stored *and* the checkpoint already
        // names that snapshot, so there is nothing this run could add.
        if let Some(latest) = ledger.latest_snapshot(&locator)?
            && latest.object_id.as_str() == digest
            && let Some(checkpoint) = ledger.checkpoint(vendor.source_kind(), &locator)?
            && checkpoint.snapshot_id == latest.snapshot_id
        {
            report.git_repositories_unchanged += 1;
            continue;
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines = complete_lines(&text, &bytes);
        // `vendor` here is always `Vendor::Git`, never Codex, so there is no cross-file
        // subagent set to pass -- an empty one is simply never consulted.
        let (keystore, identities) =
            vendor.pre_scan(&lines, run.home, &std::collections::HashSet::new());
        if !run.dry_run {
            for (opaque, (kind, display)) in &identities {
                ledger.register_identity(opaque, kind, display, run.observed_at)?;
            }
        }

        let mut observations: Vec<Observation> = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            match classify_line(
                vendor,
                line,
                &keystore,
                &digest,
                // A commit record with no `author_time` falls back to this, flagged
                // `acquired_at`: it is when this import ran, not when anything was
                // written, and the flag is what keeps it off every clock.
                run.observed_at,
                idx + 1,
                false,
            ) {
                LineOutcome::Drafts { record_ref, drafts } => {
                    for draft in drafts {
                        if draft.repository_ref.is_none() {
                            report.observations_unattributed += 1;
                        }
                        observations.push(finalize(
                            draft,
                            &record_ref,
                            run.device,
                            run.observed_at,
                        ));
                    }
                }
                LineOutcome::Gap { draft, bucket } => {
                    let record_ref = format!("line:{}", idx + 1);
                    observations.push(finalize(*draft, &record_ref, run.device, run.observed_at));
                    report.record_gap(&bucket);
                }
                LineOutcome::Skipped { kind } => {
                    *report.records_skipped.entry(kind.to_string()).or_insert(0) += 1;
                }
            }
        }

        if run.dry_run {
            account_dry_run(ledger, &observations, dry_run_seen, report)?;
            continue;
        }

        let ingest_report = ledger.ingest(
            SnapshotRef {
                source_kind: vendor.source_kind(),
                source_locator: &locator,
                bytes: &bytes,
                // These bytes did not exist until this run made them, so the moment
                // they were collected is the moment the run started -- which is what
                // `acquired_at` means for every other snapshot too.
                acquired_at: run.observed_at,
                format_fingerprint: None,
            },
            &observations,
            // A count of the records this window held. Not a resume point -- see this
            // function's doc comment for why a git snapshot has none -- but the
            // checkpoint row is what the byte-equality short-circuit above reads, and
            // leaving its cursor empty would say less than is known.
            CheckpointAdvance {
                cursor: Some(&lines.len().to_string()),
                updated_at: run.observed_at,
            },
        )?;
        for (obs, outcome) in observations.iter().zip(ingest_report.observations.iter()) {
            report.record_outcome(&obs.event_type, *outcome);
        }
    }
    Ok(())
}

/// The reported reason a repository contributed no commits. `'static` labels, never text
/// off a `git` invocation -- the same discipline a gap marker's `reason` follows.
fn unresolved_reason(scan: &crate::git::RepositoryScan) -> &'static str {
    match scan {
        crate::git::RepositoryScan::Missing => "missing",
        crate::git::RepositoryScan::NotARepository => "not_a_repository",
        crate::git::RepositoryScan::NoCommitsYet => "no_commits_yet",
        crate::git::RepositoryScan::NoIdentity => "no_identity",
        crate::git::RepositoryScan::TimedOut => "timed_out",
        crate::git::RepositoryScan::Unreadable => "unreadable",
        // Unreachable: the caller matches `Collected` first and never asks for a reason
        // a repository it just read contributed nothing. Named honestly rather than
        // folded in beside `Unreadable`, so that if it ever *were* reached it would read
        // as the bug it is instead of as a repository git could not open.
        crate::git::RepositoryScan::Collected { .. } => "collected_but_reported_unresolved",
    }
}

/// Report what ingesting `observations` *would* do, without writing anything.
///
/// Two sources of duplication have to be modelled, not one: keys already in the ledger
/// ([`Ledger::observation_present`]) and keys this same run has already counted
/// (`seen`). The second is the common case on a first dry run -- an empty ledger plus
/// the resume copies Claude Code writes into every forked session file, or the same
/// commit collected from two repositories -- and without it the dry run reports each
/// copy as a separate creation, which is the exact overstatement this exists to avoid.
fn account_dry_run(
    ledger: &Ledger,
    observations: &[Observation],
    seen: &mut std::collections::HashSet<String>,
    report: &mut ImportReport,
) -> Result<(), LedgerError> {
    for obs in observations {
        let first_this_run = seen.insert(obs.cclogdedupekey.clone());
        let outcome = if !first_this_run || ledger.observation_present(&obs.cclogdedupekey)? {
            ObservationOutcome::AlreadyPresent
        } else {
            ObservationOutcome::Created
        };
        report.record_outcome(&obs.event_type, outcome);
    }
    Ok(())
}

/// The `sha256:<hex>` id the object store will give these bytes.
///
/// Recomputed here rather than read back from the store because the gap markers have to
/// be built *before* `ingest` publishes anything -- and it is the same formula, so the
/// two never disagree about what a snapshot is called.
fn snapshot_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// Whether a snapshot's bytes stop partway through a record.
///
/// `cclogger archive` copies a live session file with a plain `fs::read` while Claude
/// Code may be mid-append, so a snapshot's last line can be a fragment. JSONL
/// terminates every record with `\n`, so bytes that do not end in one have an
/// incomplete final line. (Empty bytes have no final line at all, incomplete or
/// otherwise.)
///
/// The single source of truth for that condition: [`complete_lines`] drops the line
/// and [`run_import`] counts it, and those two must never disagree about which
/// snapshots have one.
fn ends_mid_record(bytes: &[u8]) -> bool {
    !bytes.is_empty() && !bytes.ends_with(b"\n")
}

/// The lines of a snapshot that are certainly whole records.
///
/// An incomplete final line ([`ends_mid_record`]) is dropped here, which keeps it out
/// of the transformation pass *and* out of the cursor, leaving the next snapshot to
/// pick up the completed record as new. `String::from_utf8_lossy` (which the caller
/// applies) would otherwise hand back a fragment split through a multi-byte character
/// as replacement bytes, so this is not only about JSON validity.
fn complete_lines<'a>(text: &'a str, bytes: &[u8]) -> Vec<&'a str> {
    let mut lines: Vec<&str> = text.lines().collect();
    if ends_mid_record(bytes) {
        lines.pop();
    }
    lines
}

/// Verify the property the line-count cursor rests on: complete line `N` of the
/// snapshot the checkpoint was written against is still complete line `N` of the
/// snapshot about to be read.
///
/// Append-only growth makes this true, but nothing in the pipeline *enforces*
/// append-only -- a session file could be rewritten, relocated, or truncated -- so it
/// is checked rather than assumed (the previous version of this module asserted it in
/// prose and then relied on it). `Err` carries a reason for the diagnostic; it never
/// carries record content, only line numbers.
fn prefix_check(
    ledger: &Ledger,
    by_snapshot_id: &BTreeMap<i64, cclogger_archive::Snapshot>,
    checkpoint_snapshot_id: i64,
    lines: &[&str],
    cursor: usize,
) -> Result<(), String> {
    if cursor > lines.len() {
        return Err(format!(
            "cursor {cursor} is past the new snapshot's {} complete line(s)",
            lines.len()
        ));
    }
    let Some(previous) = by_snapshot_id.get(&checkpoint_snapshot_id) else {
        return Err(format!(
            "snapshot {checkpoint_snapshot_id}, which the cursor was counted against, is no \
             longer in the ledger"
        ));
    };
    let previous_bytes = ledger.read(&previous.object_id).map_err(|e| {
        format!("snapshot {checkpoint_snapshot_id}, which the cursor was counted against, is unreadable: {e}")
    })?;
    let previous_text = String::from_utf8_lossy(&previous_bytes);
    let previous_lines = complete_lines(&previous_text, &previous_bytes);
    if previous_lines.len() < cursor {
        return Err(format!(
            "cursor {cursor} is past snapshot {checkpoint_snapshot_id}'s {} complete line(s)",
            previous_lines.len()
        ));
    }
    for i in 0..cursor {
        if previous_lines[i] != lines[i] {
            return Err(format!(
                "line {} differs from snapshot {checkpoint_snapshot_id}, so this snapshot is not \
                 that one grown by append",
                i + 1
            ));
        }
    }
    Ok(())
}

/// What one snapshot line accounts for. Every complete line lands in exactly one of
/// these, which is what makes [`ImportReport`]'s arithmetic close.
#[derive(Debug)]
enum LineOutcome {
    /// The record produced observations, to be stamped with `record_ref`.
    Drafts {
        record_ref: String,
        drafts: Vec<ObservationDraft>,
    },
    /// The record could not be turned into observations, and that is diagnosed
    /// rather than dropped. The draft is boxed only to keep this enum's size in
    /// line with its other variants -- an `ObservationDraft` is ~300 bytes and every
    /// complete line is matched against this type.
    Gap {
        draft: Box<ObservationDraft>,
        bucket: GapBucket,
    },
    /// The record's kind is understood and this instance legitimately carries no
    /// event (a sidechain non-tool-result `user` record; an `attachment` that is not
    /// a `SessionStart` hook success). Not a gap -- but not invisible either.
    Skipped { kind: &'static str },
}

/// Which [`ImportReport`] counter a [`LineOutcome::Gap`] belongs to.
#[derive(Debug, PartialEq, Eq)]
enum GapBucket {
    ParseError,
    /// Carries the *validated* label, never the raw vendor string.
    UnmappedKind(String),
    MissingField(&'static str),
    /// The line records the receiver's own failure to make an event of a hook payload.
    /// A `'static` reason matched against [`hook::RECEIVER_ERRORS`], never text off the
    /// line.
    ReceiverError(&'static str),
}

/// The fields every mapped Claude Code record kind needs before the adapter can place
/// it in a session and on a timeline. Checked in a fixed order so the same malformed
/// record always reports the same field.
const REQUIRED_RECORD_FIELDS: &[&str] = &["sessionId", "timestamp", "uuid"];

/// Substituted for a record `type` that is not a plausible kind name. See
/// [`kind_detail`].
pub const UNPRINTABLE_KIND: &str = "(unprintable)";

/// Turn one snapshot line into observations, a diagnosed gap, or a counted skip.
///
/// Pure: no ledger, no clock, no id minting. Everything it needs about the enclosing
/// snapshot arrives as `snapshot_digest` (gap identity), `acquired_at` (the fallback
/// timestamp for a record that carries none), and `inherited` -- whether this line is
/// copied history, which is a fact about the whole file and so is decided by
/// [`Vendor::inherited_lines`] before this is called, not re-derived per record.
fn classify_line(
    vendor: Vendor,
    line: &str,
    keystore: &Keystore,
    snapshot_digest: &str,
    acquired_at: &str,
    line_no: usize,
    inherited: bool,
) -> LineOutcome {
    let locator_ref = format!("line:{line_no}");

    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return LineOutcome::Gap {
            // A line that is not JSON has no timestamp to trust, so the marker is
            // dated to acquisition -- flagged as such by `time_basis`.
            draft: Box::new(gap_draft(
                vendor,
                snapshot_digest,
                &locator_ref,
                "parse_error",
                None,
                acquired_at,
                TimeBasis::Acquired,
            )),
            bucket: GapBucket::ParseError,
        };
    };

    let kind = vendor.kind(&record);
    let mut drafts = vendor.transform(&record, keystore);
    if !drafts.is_empty() {
        if inherited {
            mark_inherited(&mut drafts);
        }
        return LineOutcome::Drafts {
            record_ref: vendor.record_ref(&record, &locator_ref),
            drafts,
        };
    }

    let (time, basis) = vendor.record_time(&record, acquired_at);

    // Before asking what kind of record this is: a spool line can say the *receiver*
    // already failed, in which case there was never a record kind to diagnose. Checked
    // first so the receiver's own reason survives instead of being overwritten by
    // "unmapped kind: (unprintable)", which is what an empty `hook_event_name` would
    // otherwise produce.
    if let Some(reason) = vendor.self_declared_loss(&record) {
        return LineOutcome::Gap {
            draft: Box::new(gap_draft(
                vendor,
                snapshot_digest,
                &locator_ref,
                reason,
                None,
                &time,
                basis,
            )),
            bucket: GapBucket::ReceiverError(reason),
        };
    }
    // A gap marker on a copied line is dated by the copy too: `record_time` reads the
    // record's own `timestamp`, which is exactly the value the copy overwrote.
    let basis = if inherited { basis.copied() } else { basis };

    if !vendor.mapped_kinds().contains(&kind.as_str()) {
        let detail = kind_detail(&kind);
        let label = detail
            .clone()
            .unwrap_or_else(|| UNPRINTABLE_KIND.to_string());
        return LineOutcome::Gap {
            draft: Box::new(gap_draft(
                vendor,
                snapshot_digest,
                &locator_ref,
                "unmapped_kind",
                detail,
                &time,
                basis,
            )),
            bucket: GapBucket::UnmappedKind(label),
        };
    }

    // A kind this adapter has a case for, producing nothing: either the instance is
    // missing a field the kind requires (malformed -- a gap), or it is well-formed
    // and legitimately eventless (a counted skip). Conflating the two is what let a
    // human prompt with no `timestamp` disappear without a trace.
    if let Some(field) = vendor.missing_required_field(&record, keystore) {
        return LineOutcome::Gap {
            draft: Box::new(gap_draft(
                vendor,
                snapshot_digest,
                &locator_ref,
                "missing_field",
                Some(field.to_string()),
                &time,
                basis,
            )),
            bucket: GapBucket::MissingField(field),
        };
    }

    LineOutcome::Skipped {
        // `kind` matched `mapped_kinds()`, so this is one of that constant's own
        // `'static` strings, never vendor-controlled text.
        kind: vendor
            .mapped_kinds()
            .iter()
            .find(|k| **k == kind)
            .copied()
            .unwrap_or("unknown"),
    }
}

/// Stamp `data.time_basis = "copied_at"` on every draft derived from a copied record.
///
/// The mark, not a drop. Every copied prompt in the surveyed corpus does have a live
/// original in the parent's own file -- but that is a property of that corpus, not a
/// guarantee, and dropping a human turn on the strength of it is the silent loss this
/// project treats as its worst failure. So the record is imported, and marked so that
/// no clock can read its timestamp as an event time.
///
/// `data` is a JSON object on every draft the canonical schema admits (`"data": {
/// "type": "object" }`), and `every_codex_draft_carries_an_object_data_so_the_copied_mark_lands`
/// pins that for the adapter this actually runs against, so the mark can never be
/// silently dropped for want of somewhere to put it.
fn mark_inherited(drafts: &mut [ObservationDraft]) {
    for draft in drafts {
        if let Some(data) = draft.data.as_object_mut() {
            data.insert("time_basis".to_string(), json!(TimeBasis::Copied.as_str()));
        }
    }
}

/// The first field in [`REQUIRED_RECORD_FIELDS`] this Claude Code record does not
/// carry, or `None` if it carries all of them.
fn missing_required_field(record: &Value) -> Option<&'static str> {
    for field in REQUIRED_RECORD_FIELDS {
        let present = match *field {
            // The adapter accepts either spelling for the session id.
            "sessionId" => {
                record.get("sessionId").and_then(Value::as_str).is_some()
                    || record.get("session_id").and_then(Value::as_str).is_some()
            }
            other => record.get(other).and_then(Value::as_str).is_some(),
        };
        if !present {
            return Some(field);
        }
    }
    None
}

/// What a mapped-kind Codex record that produced nothing was missing.
///
/// Deliberately not a list of field names the way [`REQUIRED_RECORD_FIELDS`] is,
/// because the last of the three is not a field on the record at all. A Codex record
/// needs a `timestamp` and a `payload` of its own, and then a *session* -- which for
/// nearly every record kind is a fact about the enclosing file rather than about the
/// record (`event_msg:user_message` carries no session field whatsoever). A transcript
/// with no resolvable `session_meta` therefore leaves its prompts unplaceable, and that
/// is a diagnosed gap rather than a counted skip: the record is evidence of a human
/// turn, and dropping it into `records_skipped` would file the loss of the primary
/// signal under "legitimately carries no event".
///
/// Tool records are never reported here -- the adapter emits them with a degraded
/// identity rather than nothing when their session is unresolved, so they never reach
/// this path.
fn codex_missing_required_field(record: &Value, keystore: &Keystore) -> Option<&'static str> {
    if record.get("timestamp").and_then(Value::as_str).is_none() {
        return Some("timestamp");
    }
    let Some(payload) = record.get("payload") else {
        return Some("payload");
    };
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or(codex_history::FILE_SESSION);
    if keystore.resolve("session", session).is_none() {
        return Some("session_id");
    }
    None
}

/// What a mapped hook event was missing, if that is why the adapter produced nothing.
///
/// Checked in a fixed order so the same malformed line always reports the same field.
/// The last two are per-event: a turn event has nothing to anchor on without a
/// `prompt_id`, and a tool event nothing without a `tool_use_id` -- but a *tool* event
/// missing only its `prompt_id` is not reported here at all, because the adapter emits
/// it one level up rather than dropping it, so it never reaches this path.
fn hook_missing_required_field(record: &Value) -> Option<&'static str> {
    let string = |field: &str| record.get(field).and_then(Value::as_str).is_some();
    if !string("session_id") {
        return Some("session_id");
    }
    if !string(hook::RECEIVED_AT) {
        return Some(hook::RECEIVED_AT);
    }
    match record.get("hook_event_name").and_then(Value::as_str) {
        Some("UserPromptSubmit" | "Stop" | "StopFailure") if !string("prompt_id") => {
            Some("prompt_id")
        }
        Some("PreToolUse" | "PostToolUse" | "PostToolUseFailure") if !string("tool_use_id") => {
            Some("tool_use_id")
        }
        _ => None,
    }
}

/// Accept a vendor `type` string as a gap marker's `detail` only if it is a plausible
/// kind name.
///
/// `detail` lands verbatim in a T1 ledger row and on stdout, and the whole point of
/// the gap path is handling records that do *not* look like the surveyed corpus, so
/// the one field carrying vendor-controlled text gets the same discipline the
/// schema already imposes on `reason` (`^[a-z0-9_]+$`). A `type` that fails this is
/// dropped to `null` rather than truncated or escaped: the marker's identity is
/// already carried by the snapshot digest plus the line number, so nothing is lost.
///
/// `:` is allowed because a Codex kind is two vendor `type` fields joined by one
/// (`event_msg:token_count` -- see [`codex_history::kind`]). Rejecting it would collapse
/// every unmapped Codex kind into [`UNPRINTABLE_KIND`] and leave the per-kind coverage
/// report with a single undifferentiated bucket.
///
/// On the Codex path that colon is a separator this code inserts. **On the Claude Code
/// path it is not**: there the kind is the raw vendor `type` string, so a colon in it
/// would be vendor-controlled, and widening the charset widens that path too. What
/// bounds the risk is the rest of the discipline, which is unchanged: no whitespace,
/// no `/`, no `.`, and at most 64 bytes -- so a path, an address or a sentence still
/// fails, which is what this guard exists to stop.
fn kind_detail(kind: &str) -> Option<String> {
    let plausible = (1..=64).contains(&kind.len())
        && kind
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':');
    plausible.then(|| kind.to_string())
}

/// Everything the pre-scan learns from one locator's records before any of them are
/// transformed.
///
/// Two things need a whole-file view rather than a single record. Tool durations
/// need a `tool_use`'s timestamp when the matching tool *result* is transformed. And
/// a record can carry no `cwd` at all, in which case its workspace can only come from
/// the other records of the same session -- a majority vote, which cannot be taken one
/// record at a time. (Only `user`, `assistant` and `attachment` records ever reach the
/// adapter, so only their cwd-less instances are backfilled; a cwd-less record of any
/// other kind becomes a gap marker and is never attributed.)
///
/// The [`Keystore`] it builds is also how a value the adapter cannot compute for
/// itself reaches it without giving the adapter a clock or the file: `tool_started_at`
/// carries the `tool_use` record's own timestamp forward to the (later, separate)
/// tool-result record, which is what lets the adapter emit a real `duration_ms`.
#[derive(Default)]
struct PreScan {
    keystore: Keystore,
    /// session id -> *resolved* repository identity -> how many of its records
    /// resolved to it.
    ///
    /// Keyed by the resolved identity rather than the raw cwd: a repository whose
    /// records are spread over its own subdirectories or worktrees would otherwise
    /// split its vote against a rival that concentrated its cwds, and lose a session
    /// it held the majority of. A cwd that resolves to nothing does not vote at all
    /// rather than voting for itself.
    session_repositories: BTreeMap<String, BTreeMap<String, usize>>,
    /// session id -> repository identity -> workspace identity -> count.
    ///
    /// Nested under the repository, not tallied flat beside it, because a workspace
    /// identity is *defined* as living inside a repository -- it is literally
    /// `repository` or `repository@branch`. Two independent votes could elect a pair
    /// whose workspace does not sit under its repository, which is not a vaguer
    /// answer but an impossible one: a consumer joining the two would read one record
    /// as being in two different projects. So the repository settles first and the
    /// workspace vote runs over only that repository's records. Nothing of value is
    /// discarded -- comparing workspace counts *across* repositories was never a
    /// meaningful comparison.
    session_workspaces: BTreeMap<String, BTreeMap<String, BTreeMap<String, usize>>>,
    /// opaque ref -> (kind, display name)
    identities: BTreeMap<String, (String, String)>,
}

impl PreScan {
    /// Learn every opaque ref one JSONL `record` names: its session, the repository
    /// and workspace its `cwd` resolves to, and -- for an `assistant` record -- each
    /// `tool_use` block's id, family, and start time.
    ///
    /// Deterministic (`pseudonymize` over the *normalized* identity), so re-running
    /// this pre-scan on a re-import assigns the same refs and therefore the same
    /// dedupe keys.
    fn observe(&mut self, record: &Value, home: &str) {
        self.observe_common(record, home);
        if record.get("type").and_then(Value::as_str) == Some("assistant") {
            let started_at = record.get("timestamp").and_then(Value::as_str);
            if let Some(items) = record
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for item in items {
                    if item.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let Some(tool_use_id) = item.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let opaque = pseudonymize("tol", tool_use_id);
                    self.map("tool", tool_use_id, &opaque);
                    let family = tool_family(item.get("name").and_then(Value::as_str));
                    self.map("tool_family", tool_use_id, family);
                    if let Some(started_at) = started_at {
                        self.map("tool_started_at", tool_use_id, started_at);
                    }
                }
            }
        }
    }

    /// Learn what one **spool line** names: the same session and cwd every transcript
    /// record carries, plus the three ids only the hook channel has.
    ///
    /// No `tool_started_at` is registered, and that is the point of the whole channel:
    /// the transcript path has to reconstruct a tool's duration by subtracting a
    /// `tool_use` line's timestamp from its result's, which silently includes any time
    /// the person spent at a permission prompt. `PostToolUse` carries the vendor's own
    /// `duration_ms`, which the reference documents as excluding exactly that, so there
    /// is nothing to close by subtraction here.
    fn observe_hook(&mut self, record: &Value, home: &str) {
        self.observe_common(record, home);
        for (field, prefix, kind) in [
            ("prompt_id", "trn", "turn"),
            ("tool_use_id", "tol", "tool"),
            ("agent_id", "agt", "agent"),
        ] {
            if let Some(id) = record.get(field).and_then(Value::as_str) {
                let opaque = pseudonymize(prefix, id);
                self.map(kind, id, &opaque);
            }
        }
    }

    /// The part both channels share: the record's session, and the identities its `cwd`
    /// resolves to (including that cwd's vote in the session-level majority).
    fn observe_common(&mut self, record: &Value, home: &str) {
        let session = session_id(record);
        if let Some(session_id) = session {
            let opaque = pseudonymize("ses", session_id);
            self.map("session", session_id, &opaque);
        }
        if let Some(cwd) = record.get("cwd").and_then(Value::as_str) {
            let id = self.register_identities_for(cwd, home);
            if let Some(session_id) = session {
                // Vote with the resolved identities, not the cwd -- see
                // `session_repositories`. An unresolvable cwd contributes to neither
                // tally: it has no identity to cast a vote for, and letting it win a
                // session would leave that session's cwd-less records unattributed
                // even though it also named repositories that could be resolved.
                //
                // Both votes are cast together, from one resolution, so a workspace
                // is never counted apart from the repository it sits in.
                if let (Some(repository), Some(workspace)) = (id.repository, id.workspace) {
                    Self::tally(&mut self.session_repositories, session_id, &repository);
                    Self::tally(
                        self.session_workspaces
                            .entry(session_id.to_string())
                            .or_default(),
                        &repository,
                        &workspace,
                    );
                }
            }
        }
    }

    /// Resolve one cwd and register both identities it carries, keyed by the cwd so
    /// the adapter can look them up from a record without ever seeing the path again.
    ///
    /// A cwd outside the ghq tree registers nothing: the adapter then resolves it to
    /// `None`, which is design §10 rule 4 -- unresolved, never guess-merged.
    ///
    /// Raw cwd strings are deliberately never persisted -- they contain the username,
    /// and the ledger stays metadata-only. What reaches the registry is the
    /// *normalized* identity (`github.com/acme/api`) behind each pseudonym.
    ///
    /// Returns what the cwd resolved to, so the caller can cast the session's votes
    /// without resolving it a second time.
    fn register_identities_for(&mut self, cwd: &str, home: &str) -> workspace::WorkspaceIdentity {
        let id = workspace::resolve(cwd, home);
        if let Some(workspace) = &id.workspace {
            let opaque = pseudonymize("wsp", workspace);
            self.map("workspace", cwd, &opaque);
            self.identities
                .insert(opaque, ("workspace".to_string(), workspace.clone()));
        }
        if let Some(repository) = &id.repository {
            let opaque = pseudonymize("rep", repository);
            self.map("repository", cwd, &opaque);
            self.identities
                .insert(opaque, ("repository".to_string(), repository.clone()));
        }
        id
    }

    fn map(&mut self, kind: &str, vendor: &str, opaque: &str) {
        self.keystore = std::mem::take(&mut self.keystore).map(kind, vendor, opaque);
    }

    /// Add one vote for `identity` under `bucket`.
    fn tally(
        tallies: &mut BTreeMap<String, BTreeMap<String, usize>>,
        bucket: &str,
        identity: &str,
    ) {
        *tallies
            .entry(bucket.to_string())
            .or_default()
            .entry(identity.to_string())
            .or_insert(0) += 1;
    }

    /// Settle the session-level majority votes and hand back the finished keystore
    /// plus every identity seen, for the caller to write to the registry.
    ///
    /// The repository settles first; the workspace is then the majority *within* that
    /// repository, so the pair is always coherent. A session whose records are spread
    /// across several worktrees of one repository therefore still has an unambiguous
    /// repository, and a workspace chosen from that repository's own worktrees.
    fn finish(mut self) -> (Keystore, BTreeMap<String, (String, String)>) {
        let votes: Vec<(String, String, Option<String>)> = self
            .session_repositories
            .iter()
            .filter_map(|(session, counts)| {
                let repository = majority(counts)?;
                // Only the winning repository's own records vote on the workspace.
                let workspace = self
                    .session_workspaces
                    .get(session)
                    .and_then(|by_repository| by_repository.get(&repository))
                    .and_then(majority);
                Some((session.clone(), repository, workspace))
            })
            .collect();
        for (session, repository, workspace) in votes {
            let opaque = pseudonymize("rep", &repository);
            self.map("session_repository", &session, &opaque);
            // Always `Some` in practice -- both votes are cast together from one
            // resolution -- but a missing workspace registers nothing rather than
            // falling back to the bare repository, which would claim work happened in
            // the main checkout when the evidence says only which project it was in.
            if let Some(workspace) = workspace {
                let opaque = pseudonymize("wsp", &workspace);
                self.map("session_workspace", &session, &opaque);
            }
        }
        (self.keystore, self.identities)
    }
}

/// The most-voted-for identity, ties broken by the lexicographically smallest.
///
/// The tie-break has to be total and reproducible, not merely self-consistent within
/// one process. A session resumed into a second file is voted on again by that file's
/// own pre-scan, and every re-import votes from scratch; `ingest` writes an
/// observation once and `ON CONFLICT(cclogdedupekey) DO NOTHING` never rewrites it, so
/// a vote that could land differently would attribute the same session to whichever
/// repository happened to be counted first, permanently and with nothing to show that
/// two runs disagreed. (No dedupe *key* is derived from these refs -- every seed is
/// session + record identity -- so the failure is a silent misattribution rather than
/// a duplicated row, which is the harder one to notice.)
fn majority(counts: &BTreeMap<String, usize>) -> Option<String> {
    let mut best: Option<(&String, usize)> = None;
    for (identity, n) in counts {
        // Strict `>` keeps the first seen, and a BTreeMap iterates in key order,
        // so a tie resolves to the smallest identity.
        if best.is_none_or(|(_, best_n)| *n > best_n) {
            best = Some((identity, *n));
        }
    }
    best.map(|(identity, _)| identity.clone())
}

/// The raw vendor session id on a record, if it names one.
fn session_id(record: &Value) -> Option<&str> {
    record
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| record.get("session_id").and_then(Value::as_str))
}

/// Same tool-family normalization as the adapters -- kept local since the importer,
/// not the (pure, per-record) adapter, is what needs to look a `tool_use`'s name up
/// again later from a *different* record (the matching tool result).
fn tool_family(name: Option<&str>) -> &'static str {
    match name.unwrap_or("") {
        "Bash" | "Shell" => "shell",
        "Edit" | "Write" | "MultiEdit" => "edit",
        "Read" => "read",
        "Grep" | "Glob" => "search",
        "WebFetch" | "WebSearch" => "web",
        n if n.starts_with("mcp__") => "mcp",
        _ => "other",
    }
}

// -- the git pre-scan --------------------------------------------------------------

/// The two pseudonyms a collected commit record names.
///
/// The smallest of the three pre-scans, because a git record has no cross-record
/// question to answer: it names its own repository (`cclogger-cli`'s `git` module puts the
/// identity on every line, from the ledger row it was collected for), so there is no
/// `cwd` to resolve, no session to place it in, and nothing to vote on.
///
/// The repository pseudonym is derived exactly as [`PreScan::register_identities_for`]
/// derives it -- `pseudonymize("rep", <normalized identity>)` -- and that is not a
/// coincidence to be tidied up later: it is what makes a commit and the session that
/// produced it land on the *same* `cclogrepositoryref`, which is the only reason a
/// report can put them side by side.
#[derive(Default)]
struct GitPreScan {
    keystore: Keystore,
    identities: BTreeMap<String, (String, String)>,
}

impl GitPreScan {
    fn observe(&mut self, record: &Value) {
        if let Some(identity) = record.get("repository").and_then(Value::as_str) {
            let opaque = pseudonymize("rep", identity);
            self.map("repository", identity, &opaque);
            // Re-registered on every run, which is a no-op after the first
            // (`INSERT OR IGNORE` keeps the original `first_seen`). Worth doing anyway:
            // it is what lets a repository whose transcripts have aged out of the
            // archive still have a name in a report built from its commits alone.
            self.identities
                .insert(opaque, ("repository".to_string(), identity.to_string()));
        }
        if let Some(sha) = record.get("commit").and_then(Value::as_str) {
            let opaque = pseudonymize("cmt", sha);
            self.map("commit", sha, &opaque);
            // Deliberately *not* registered as an identity: `workspace_identity` maps a
            // pseudonym back to the readable thing it stands for, and writing the raw
            // sha there would undo the pseudonym on the very next line.
        }
    }

    fn map(&mut self, kind: &str, vendor: &str, opaque: &str) {
        self.keystore = std::mem::take(&mut self.keystore).map(kind, vendor, opaque);
    }

    fn finish(self) -> (Keystore, BTreeMap<String, (String, String)>) {
        (self.keystore, self.identities)
    }
}

// -- the Codex pre-scan ----------------------------------------------------------

/// Everything the pre-scan learns from one Codex transcript before any of its records
/// are transformed.
///
/// Structurally much smaller than [`PreScan`], and the difference is measured rather
/// than stylistic. **There is no majority vote here.** The Claude Code pre-scan next
/// door votes because 14 of its 22 record kinds carry no `cwd` and one session's
/// records can genuinely name several; a Codex transcript's `cwd` was measured never to
/// vary within a file (0 of 328 real files show more than one), so the file's answer is
/// every record's answer and a vote would only be a more expensive way to reach it.
///
/// What this pre-scan does carry is the piece without which nothing else works. Measured
/// over the real corpus, `event_msg:user_message` -- the human prompt, and the primary
/// signal this whole project exists to count -- carries **no session field at all**, and
/// no `cwd` either. Both live on `session_meta`, a different record on a different line.
/// So every ref is registered twice: under the raw vendor session id, for the records
/// that name one, and under [`codex_history::FILE_SESSION`], for the overwhelming
/// majority that do not.
///
/// The one fact this pre-scan cannot learn by itself is whether the file *in front of
/// it* is a subagent's: that is named only in a *different* file, so [`Self::finish`]
/// takes the whole run's answer as a parameter rather than deriving it here -- see
/// [`codex_subagent_thread_ids`].
#[derive(Default)]
struct CodexPreScan {
    keystore: Keystore,
    /// opaque ref -> (kind, display name)
    identities: BTreeMap<String, (String, String)>,
    /// The file's session id, from the first `session_meta` that names one.
    session: Option<String>,
    /// This file's own canonical thread id -- the first `session_meta`'s `payload.id`.
    ///
    /// Deliberately not [`codex_session_id`] (`session_id`, falling back to `id`): a
    /// corpus survey found every `sub_agent_activity.agent_thread_id` resolves to a
    /// real file's own `session_meta.id`, the same field [`codex_inherited`]'s `SELF`
    /// reads for exactly the same reason (upstream's own reader distinguishes threads
    /// by this field, not by `session_id`). It is read here independently of
    /// `session_id` because the two are measured to differ whenever both are present,
    /// so treating the former as a stand-in for the latter would compare a subagent
    /// spawn's parent-supplied thread id against a value it was never issued.
    thread_id: Option<String>,
    /// The resolved identities of the file's `cwd`. `None` when no `session_meta`
    /// carried one, or it resolved outside the ghq tree -- unresolved, never
    /// guess-merged (design §10 rule 4).
    repository: Option<String>,
    workspace: Option<String>,
}

impl CodexPreScan {
    /// Learn what one Codex transcript record contributes: the file's session and
    /// working directory (`session_meta` only), or a tool call's id, family and start
    /// time.
    fn observe(&mut self, record: &Value, home: &str) {
        let Some(payload) = record.get("payload") else {
            return;
        };
        match codex_history::kind(record).as_str() {
            "session_meta" => {
                // First one wins, and only the first: `session_meta` is re-announced up
                // to 30 times per file, and the `id` fallback below takes a *different*
                // value on each re-announcement.
                if self.session.is_none() {
                    self.session = codex_session_id(payload).map(str::to_string);
                }
                // Same "first one wins" rule, for the same reason -- see `thread_id`'s
                // doc comment for why this is `payload.id` rather than `session_id`.
                if self.thread_id.is_none() {
                    self.thread_id = payload
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                // Every `session_meta`, not just the first, so a variant that omits
                // `cwd` does not lose the file its workspace. Last writer wins, which
                // is deterministic (file order is stable and the whole file is always
                // rescanned) and unobservable in practice, `cwd` having been measured
                // constant within a file.
                if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                    let id = workspace::resolve(cwd, home);
                    if id.repository.is_some() {
                        self.repository = id.repository;
                        self.workspace = id.workspace;
                    }
                }
            }
            "response_item:custom_tool_call" | "response_item:function_call" => {
                self.observe_tool_call(record, payload);
            }
            _ => {}
        }
    }

    /// Register a tool call under **both** spellings of its id.
    ///
    /// A call record carries `id` (`ctc_…`) and `call_id`; its matching output record
    /// carries only `call_id`. Registering under one spelling alone would leave the
    /// other record resolving nothing: a null `duration_ms` on every Codex tool call,
    /// a `tool_family` of `"other"` on every finished event, and a scope the pair
    /// cannot be joined on. The ref itself is derived from `call_id` where there is one
    /// -- the field both records share -- so the two land on the same value.
    fn observe_tool_call(&mut self, record: &Value, payload: &Value) {
        let id = payload.get("id").and_then(Value::as_str);
        let call_id = payload.get("call_id").and_then(Value::as_str);
        let Some(canonical) = call_id.or(id) else {
            return;
        };
        let opaque = pseudonymize("tol", canonical);
        let family = codex_history::tool_family(payload.get("name").and_then(Value::as_str));
        let started_at = record.get("timestamp").and_then(Value::as_str);
        for key in [id, call_id].into_iter().flatten() {
            self.map("tool", key, &opaque);
            self.map("tool_family", key, family);
            if let Some(started_at) = started_at {
                self.map("tool_started_at", key, started_at);
            }
        }
    }

    fn map(&mut self, kind: &str, vendor: &str, opaque: &str) {
        self.keystore = std::mem::take(&mut self.keystore).map(kind, vendor, opaque);
    }

    /// One ref under several vendor keys -- how the file's session and its workspace
    /// come to be reachable both by the id `session_meta` names and by
    /// [`codex_history::FILE_SESSION`].
    fn map_all(&mut self, kind: &str, vendors: &[String], opaque: &str) {
        for vendor in vendors {
            self.map(kind, vendor, opaque);
        }
    }

    /// Hand back the finished keystore plus every identity seen, for the caller to
    /// write to the registry.
    ///
    /// The session's refs are registered here rather than in [`CodexPreScan::observe`]
    /// because the two halves arrive on different records: a `session_meta` variant may
    /// name the session without a `cwd`, or the reverse. Settling once, at the end,
    /// keeps them keyed together.
    ///
    /// Raw cwd strings never reach the registry -- they contain the username, and the
    /// ledger stays metadata-only. What is stored is the normalized identity
    /// (`github.com/acme/api`) behind each pseudonym.
    ///
    /// `subagent_threads` is the whole run's answer to a question this one file cannot
    /// answer about itself -- see [`codex_subagent_thread_ids`]. Consulted only here,
    /// once [`Self::thread_id`] has settled, rather than per-record in
    /// [`CodexPreScan::observe`].
    fn finish(
        mut self,
        subagent_threads: &std::collections::HashSet<String>,
    ) -> (Keystore, BTreeMap<String, (String, String)>) {
        // Both keys, always: the records that name their session (`session_meta`) and
        // the ones that cannot (everything else) must resolve to the same refs.
        let mut keys = vec![codex_history::FILE_SESSION.to_string()];
        if let Some(session) = &self.session {
            keys.push(session.clone());
        }
        if let Some(session) = self.session.clone() {
            let opaque = pseudonymize("ses", &session);
            self.map_all("session", &keys, &opaque);
        }
        if let Some(repository) = self.repository.clone() {
            let opaque = pseudonymize("rep", &repository);
            self.map_all("repository", &keys, &opaque);
            self.identities
                .insert(opaque, ("repository".to_string(), repository));
        }
        if let Some(workspace) = self.workspace.clone() {
            let opaque = pseudonymize("wsp", &workspace);
            self.map_all("workspace", &keys, &opaque);
            self.identities
                .insert(opaque, ("workspace".to_string(), workspace));
        }
        // If some *other* file in this run named this file's own thread id as a
        // subagent's, every prompt this file itself goes on to produce is a
        // subagent's, not a human's. Registered under the same keys "session" is, so
        // `codex_history::transform_user_message` resolves it by whichever of them the
        // record in hand carries -- overwhelmingly `FILE_SESSION`, since a prompt
        // carries no session field of its own (see this struct's doc comment). The
        // value is unused: presence is the whole fact.
        if self
            .thread_id
            .as_deref()
            .is_some_and(|id| subagent_threads.contains(id))
        {
            self.map_all("codex_subagent_session", &keys, "subagent");
        }
        (self.keystore, self.identities)
    }
}

/// The session id a Codex `session_meta` names: `session_id`, falling back to `id`.
///
/// This deliberately reads the *opposite* way round from what
/// `adapters/codex/shapes/session-meta-variants.shape.json` advises ("identity
/// resolution must key off id/cwd rather than the richer optional fields"). That
/// guidance predates the corpus survey, and the survey contradicts it: `session_id`
/// takes exactly **one** value per file, while `id` takes 2--3 because `session_meta` is
/// re-announced up to 30 times with a fresh `id` each time. Keying on `id` would split
/// one session into three -- and every per-session aggregate with it.
///
/// What the fixture is really warning about is real, though: 4 of 328 files (older CLI
/// versions) carry no `session_id` at all. That is exactly what the `id` fallback is
/// for, and the caller applies it to the *first* `session_meta` only, so those files
/// still get one stable session rather than three.
///
/// Scoped to `session_meta` on purpose. On any other Codex record `id` means something
/// else entirely -- on a tool call it is the `ctc_…` call id -- so a blanket fallback
/// would have every tool event looking up a session that does not exist.
fn codex_session_id(payload: &Value) -> Option<&str> {
    payload
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("id").and_then(Value::as_str))
}

// -- copied history (Codex forks and subagent spawns) ----------------------------

/// How far into a Codex rollout file the *copied* history reaches.
///
/// See [`codex_inherited`] for how one of these is decided. The three variants are the
/// three answers upstream's own metadata can give: nothing was copied, the copy ends
/// at a stated ordinal, or -- when neither of the two exact markers is present -- the
/// copy is the file's opening write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inherited {
    /// Every line is live: it happened when its timestamp says.
    Nothing,
    /// The file's opening write was a copy flush: every line up to and including
    /// `through` (1-based) was copied out of the parent's transcript -- **except**
    /// line `self_meta`.
    ///
    /// That exception is the file's own `session_meta`, and it is not a special case
    /// grudgingly carved out: it is written *at the moment the fork happens*, so its
    /// timestamp is a true event time. It is precisely when this session began. Only
    /// the records copied in behind it carry a write time standing in for an event
    /// time that is no longer recoverable. Marking it too would throw away the one
    /// timestamp in the block that is real, and with it the child's `session.started`
    /// and its day's claim to having been worked at all.
    ///
    /// In every shape Codex writes, `self_meta` is 1 -- the `session_meta` is the
    /// file's first line. It is carried as a line number rather than assumed to be 1
    /// so that the exception follows the *reason* (this thread's own metadata) rather
    /// than a position: a file whose first line were something else would otherwise
    /// have a copied line kept live, which is the error this whole change exists to
    /// remove, pointed the other way.
    Flush { through: usize, self_meta: usize },
    /// Every line carrying an `ordinal` strictly below this one was copied. Exact,
    /// and stated by Codex itself (`payload.subagent_history_start_ordinal`).
    ///
    /// No `self_meta` exception here, deliberately. This boundary is not inferred --
    /// Codex states which ordinals belong to the projected inherited history -- and
    /// carving an exception into a number upstream computed would be substituting a
    /// guess for the one exact answer available. If the child's own `session_meta`
    /// falls below `S`, Codex has said it is part of the inherited projection.
    BelowOrdinal(u64),
}

impl Inherited {
    /// One flag per line, in file order.
    fn per_line(self, lines: &[&str]) -> Vec<bool> {
        match self {
            Inherited::Nothing => vec![false; lines.len()],
            Inherited::Flush { through, self_meta } => (1..=lines.len())
                .map(|n| n <= through && n != self_meta)
                .collect(),
            Inherited::BelowOrdinal(start) => lines
                .iter()
                .map(|line| {
                    // A line carrying no `ordinal` is not below one. It is not
                    // classified by its position instead: `ordinal` is the record's
                    // own, and inferring one from a line number would be a value this
                    // file never stated.
                    line_ordinal(line).is_some_and(|ordinal| ordinal < start)
                })
                .collect(),
        }
    }
}

/// A rollout line's own `ordinal` (`RolloutLine.ordinal`, a top-level sibling of
/// `timestamp`), or `None` for the older CLI versions that write none.
fn line_ordinal(line: &str) -> Option<u64> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("ordinal")?
        .as_u64()
}

/// How much of one Codex rollout file is copied history rather than a record of live
/// activity.
///
/// # Why any of this is needed
///
/// Every line of a Codex rollout carries a **write** time, not an event time: the
/// recorder stamps `OffsetDateTime::now_utc()` at serialization
/// (`codex-rs/rollout/src/recorder.rs`) and drains its buffer in a tight loop, so one
/// flush stamps its whole batch with a single millisecond. That is harmless while a
/// file only ever records what just happened -- and exactly one mechanism breaks
/// that. A fork or subagent spawn under `ForkPersistence::Copied` re-writes the
/// parent's entire history vector into the child's file in one call
/// (`codex-rs/core/src/session/mod.rs`), so those records get the *copy's* write time.
/// The originals are structurally unrecoverable from the child: `InitialHistory::Forked`
/// carries a `Vec<RolloutItem>`, which has no timestamp field to preserve.
///
/// Resume does not do this -- it appends to the existing file.
///
/// # The rule
///
/// It is upstream's own, not a heuristic. Codex's reader distinguishes an embedded
/// parent from the file's own thread by thread id
/// (`codex-rs/state/src/extract.rs`: *"Ignore session_meta lines that don't match the
/// canonical thread ID, e.g., forked rollouts that embed the source session
/// metadata"*). `SELF` below is that canonical thread id -- the first `session_meta`'s
/// `payload.id`.
///
/// 1. `payload.history_base` present -> the parent's records were *referenced*, not
///    copied. Nothing here is a copy.
/// 2. else `payload.subagent_history_start_ordinal = S` present -> lines with
///    `ordinal < S` are copied, the rest are live. Exact, and Codex's own answer.
///    (No file in the surveyed corpus carries an `ordinal` at all -- it ships from
///    0.145.0 -- so this branch exists for data that has not arrived yet.)
/// 3. else no `session_meta` in the file names a parent (`forked_from_id` /
///    `parent_thread_id`) -> nothing was copied.
/// 4. else take `B`, the maximal run from line 1 whose timestamps are within
///    [`FLUSH_TOLERANCE_MS`] of line 1's, and require that `B` contain a
///    `session_meta` whose `payload.id` is not `SELF`. With one, all of `B` is copied
///    **except the file's own `session_meta`**, which was written at the moment the
///    fork happened and so carries a true event time (see [`Inherited::Flush`]).
///    Without one, `B` is the **benign deferred first flush** -- Codex does not create
///    the file until the first user message, so an ordinary session's `session_meta`,
///    `turn_context` and first prompt legitimately share one millisecond -- and is
///    kept.
///
/// Checked against the real corpus before it was written: this rule and a cruder
/// timestamp-only heuristic agree on 192 of 339 files; the rule catches 3 the
/// heuristic missed and correctly declines 6 it wrongly flagged.
///
/// # What is deliberately not done here
///
/// A child names its parent (`forked_from_id` / `parent_thread_id`), so a later pass
/// could join to the parent's file and recover the copied records' *true* times.
/// Nothing here attempts it: that needs a second file, which this per-locator pass
/// does not have, and a partial recovery would be worse than an honest mark.
fn codex_inherited(lines: &[&str]) -> Inherited {
    let records: Vec<Option<Value>> = lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    // Paired with each line's 1-based number, because rule 4 needs to know *where* the
    // file's own `session_meta` sits, not only what it says.
    let metas = || {
        records
            .iter()
            .enumerate()
            .filter_map(|(idx, record)| record.as_ref().map(|record| (idx + 1, record)))
            .filter(|(_, record)| codex_history::kind(record) == "session_meta")
    };

    // The file's own `session_meta` -- the first one. Every later one is either a
    // re-announcement of this thread or an embedded foreign thread, and rule 4 is
    // precisely the test that tells those apart.
    let Some((self_meta_line, first_meta)) = metas().next() else {
        // No `session_meta` at all: nothing declares a lineage, so nothing was copied.
        return Inherited::Nothing;
    };

    // Rule 1. `history_base` means the parent's prefix is *pointed at*, not copied in.
    if meta_field(first_meta, "history_base").is_some() {
        return Inherited::Nothing;
    }

    // Rule 2. Codex states the boundary itself; nothing has to be inferred.
    if let Some(start) =
        meta_field(first_meta, "subagent_history_start_ordinal").and_then(Value::as_u64)
    {
        return Inherited::BelowOrdinal(start);
    }

    // Rule 3. No lineage marker anywhere in the file means no parent to have copied
    // from -- including for every file written before those fields existed.
    let names_a_parent = metas().any(|(_, meta)| {
        meta_field(meta, "forked_from_id").is_some()
            || meta_field(meta, "parent_thread_id").is_some()
    });
    if !names_a_parent {
        return Inherited::Nothing;
    }

    // Rule 4. Without `SELF` there is no discriminator: upstream's own rule is
    // "`session_meta` lines that don't match the canonical thread ID", and a file whose
    // first `session_meta` names no `id` has no canonical thread id to compare against.
    // Guessing one would mark live records with a time basis they do not have.
    let Some(canonical) = first_meta.pointer("/payload/id").and_then(Value::as_str) else {
        return Inherited::Nothing;
    };

    let Some(opening) = records
        .first()
        .and_then(Option::as_ref)
        .and_then(record_millis)
    else {
        return Inherited::Nothing;
    };
    // A line whose timestamp cannot be read is not within a second of anything, so it
    // ends the run -- the same answer as a line written a minute later.
    let flush_end = records
        .iter()
        .take_while(|record| {
            record
                .as_ref()
                .and_then(record_millis)
                .is_some_and(|millis| (millis - opening).abs() <= FLUSH_TOLERANCE_MS)
        })
        .count();

    let embeds_a_foreign_thread = records[..flush_end]
        .iter()
        .flatten()
        .filter(|record| codex_history::kind(record) == "session_meta")
        .any(|meta| meta.pointer("/payload/id").and_then(Value::as_str) != Some(canonical));

    if embeds_a_foreign_thread {
        Inherited::Flush {
            through: flush_end,
            self_meta: self_meta_line,
        }
    } else {
        Inherited::Nothing
    }
}

/// How far apart two records' timestamps may be and still belong to one buffered
/// write.
///
/// One flush stamps its whole batch with a single millisecond, so the exact answer is
/// 0; a second of slack covers a batch large enough to straddle a millisecond tick
/// without reaching into the next thing the session did, which is never within a
/// second of the file being opened.
const FLUSH_TOLERANCE_MS: i64 = 1_000;

/// A `session_meta` payload field, `None` when absent **or** explicitly null -- both
/// of which mean the same thing here, since Codex omits these fields rather than
/// writing nulls (`skip_serializing_if = "Option::is_none"`).
fn meta_field<'a>(meta: &'a Value, field: &str) -> Option<&'a Value> {
    meta.pointer(&format!("/payload/{field}"))
        .filter(|value| !value.is_null())
}

/// A rollout line's own timestamp in milliseconds, or `None` if it carries none.
fn record_millis(record: &Value) -> Option<i64> {
    rfc3339::epoch_millis(record.get("timestamp")?.as_str()?)
}

/// Which clock an observation's `time` came from.
///
/// Two ways a `time` can fail to be when the thing happened, and both are stated on
/// the observation rather than left for a consumer to infer. 14 of the 22 record kinds
/// surveyed in `docs/source-inventory.md` carry no `timestamp` at all, so most
/// unmapped-kind gap markers are necessarily dated to when `cclogger archive` ran. And
/// a Codex fork re-writes its parent's history into the child's file, stamping every
/// copied record with the copy's write time. Emitting either as an ordinary `time`
/// with nothing to distinguish it would make a day-window query misattribute them
/// silently; `data.time_basis` says which clock the value came from so a consumer can
/// exclude or re-bucket them.
///
/// [`Occurred`](TimeBasis::Occurred) is the only value a clock may use, and
/// `report.rs`'s `time_is_when_it_happened` is where that is enforced for both views.
///
/// The variants drop the `_at` their wire values carry -- `Occurred`, not
/// `OccurredAt` -- because three variants sharing a suffix is a clippy lint, and this
/// codebase carries no `allow` attributes. [`TimeBasis::as_str`] is the single place
/// the wire spellings live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeBasis {
    /// `occurred_at`: the record's own `timestamp`, i.e. when the activity happened.
    Occurred,
    /// `acquired_at`: the snapshot's collection time -- a fallback, not a measurement
    /// of the activity.
    Acquired,
    /// `copied_at`: the record's own `timestamp`, which for a **copied** record is
    /// when the copy was written and not when the activity happened. The original time
    /// is not recoverable from this file: Codex copies history as a
    /// `Vec<RolloutItem>`, which carries no timestamp. See [`codex_inherited`].
    Copied,
    /// `received_at`: the instant `cclogger hook` was invoked. **Not** a fallback like
    /// [`Acquired`](TimeBasis::Acquired) -- it is the only time reading that exists for
    /// a hook event, since no hook event carries a timestamp, and the vendor invokes the
    /// receiver synchronously, so it is a measurement of the event's instant with a
    /// bounded error of one process spawn. That is why `report.rs` admits it onto a
    /// clock and does not admit the two above.
    Received,
}

impl TimeBasis {
    fn as_str(self) -> &'static str {
        match self {
            TimeBasis::Occurred => "occurred_at",
            TimeBasis::Acquired => "acquired_at",
            TimeBasis::Copied => "copied_at",
            TimeBasis::Received => "received_at",
        }
    }

    /// What a record's stated basis becomes once its line is known to be copied
    /// history.
    ///
    /// Only [`Occurred`](TimeBasis::Occurred) moves: it is the one that claims the
    /// record's own timestamp is the event time, and on a copied record that claim is
    /// exactly what is wrong. A marker already dated to acquisition stays that way --
    /// it never made the claim.
    fn copied(self) -> Self {
        match self {
            TimeBasis::Occurred => TimeBasis::Copied,
            other => other,
        }
    }
}

/// Build a `dev.cclog.source.gap.v1` draft. Design doc §8 requires this marker's
/// identity to be derived from the snapshot digest and the record locator alone, so
/// the same bad record produces the same marker on every re-run rather than
/// accumulating duplicates -- see the determinism tests below, which pin exactly
/// that property (the ledger's own gap-marker test, `ledger.rs`'s
/// `the_same_malformed_record_yields_the_same_gap_marker_across_runs`, covers the
/// ledger's *dedupe mechanism*, not this derivation).
fn gap_draft(
    vendor: Vendor,
    snapshot_digest: &str,
    record_locator: &str,
    reason: &'static str,
    detail: Option<String>,
    time: &str,
    time_basis: TimeBasis,
) -> ObservationDraft {
    ObservationDraft {
        event_type: "dev.cclog.source.gap.v1".to_string(),
        subject: format!("source/gap/{record_locator}"),
        time: time.to_string(),
        traceparent: None,
        source_kind: vendor.domain_kind(),
        source_version: vendor.source_version().to_string(),
        adapter_version: vendor.adapter_version().to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Gap,
        workspace_ref: None,
        repository_ref: None,
        correlation_cluster: None,
        dedupe_seed: vec![
            "source.gap".to_string(),
            snapshot_digest.to_string(),
            record_locator.to_string(),
        ],
        data: json!({
            "reason": reason,
            "detail": detail,
            "time_basis": time_basis.as_str(),
        }),
    }
}

/// Apply the runtime-supplied fields: a fresh id, the device identity, the import's
/// wall clock as `observed_at`, and the M1 personal-only profile
/// (`docs/superpowers/specs/2026-07-29-history-import-slice-design.md` §7: this slice
/// has exactly one profile). Historical import has no monotonic clock or boot id to
/// sample -- both stay `None`, exactly as `cclogger-domain`'s own
/// `finalize_with_no_monotonic_clock_serializes_a_null_cclogmonotonicns` test expects
/// of a historical-import caller.
fn finalize(
    draft: ObservationDraft,
    source_record_ref: &str,
    device: &str,
    observed_at: &str,
) -> Observation {
    finalize_with_id(draft, next_uuidv7(), source_record_ref, device, observed_at)
}

/// [`finalize`] with the minted id supplied rather than generated, so a golden test
/// can assert the whole canonical observation (id included) against a fixture the way
/// the adapter golden tests do.
fn finalize_with_id(
    draft: ObservationDraft,
    id: String,
    source_record_ref: &str,
    device: &str,
    observed_at: &str,
) -> Observation {
    draft.finalize(RuntimeStamp {
        id,
        device: device.to_string(),
        observed_at: observed_at.to_string(),
        monotonic_ns: None,
        boot_id: None,
        source_record_ref: Some(source_record_ref.to_string()),
        profile: Profile::Personal,
    })
}

// -- device identity -----------------------------------------------------------

/// A stable local device id, persisted at `<root>/device_id`, minted once and reused
/// on every run. This has to be stable across runs: `device` feeds every
/// observation's dedupe key (`ObservationDraft::finalize`), so a device id that
/// changed between runs would make every previously-ingested observation's dedupe
/// key unreproducible, breaking re-import idempotency.
fn device_id(root: &Path) -> std::io::Result<String> {
    if let Some(existing) = read_device_id(root) {
        return Ok(existing);
    }
    let path = root.join("device_id");
    let id = generate_device_id();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(id.as_bytes())?;
    Ok(id)
}

/// The persisted device id, or `None` if none has been minted yet. Never writes --
/// this is the half of [`device_id`] `--dry-run` is allowed to use.
fn read_device_id(root: &Path) -> Option<String> {
    let existing = std::fs::read_to_string(root.join("device_id")).ok()?;
    let trimmed = existing.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Stands in for a device id under `--dry-run` when none has been persisted yet. Only
/// ever feeds dedupe-key lookups that are guaranteed to miss (no device id means no
/// import has written an observation), never a stored observation.
const DRY_RUN_DEVICE: &str = "dev_dryrun";

fn generate_device_id() -> String {
    use sha2::{Digest, Sha256};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = format!(
        "{nanos}-{}-{}-{}",
        std::process::id(),
        random_u64(),
        random_u64()
    );
    let digest = Sha256::digest(seed.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("dev_{hex}")
}

// -- id minting (UUIDv7) --------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A practically-unique 64-bit value: not cryptographic randomness, just enough
/// entropy that combined with a monotonic per-process counter (see [`next_uuidv7`]),
/// no two ids minted by one importer run can ever collide.
fn random_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}

/// Mint a UUIDv7 (RFC 9562): 48-bit unix-ms timestamp, 4-bit version (`0111`), 12-bit
/// `rand_a`, 2-bit variant (`10`), 62-bit `rand_b`. `rand_a`/`rand_b` are sourced from
/// a random seed combined with a strictly-increasing in-process counter, so two ids
/// minted within the same run -- even within the same millisecond -- never collide;
/// this is the only property the ledger's `id PRIMARY KEY` actually depends on.
fn next_uuidv7() -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let seed = random_u64();
    let rand: u128 = ((seed as u128) << 64) | (seq as u128);

    let ts = now_ms & 0xFFFF_FFFF_FFFF;
    let rand_b = rand & 0x3FFF_FFFF_FFFF_FFFF; // low 62 bits
    let rand_a = (rand >> 62) & 0xFFF; // next 12 bits

    let value: u128 = (ts << 80) | (0x7u128 << 76) | (rand_a << 64) | (0b10u128 << 62) | rand_b;
    format_uuid(value)
}

fn format_uuid(value: u128) -> String {
    let bytes = value.to_be_bytes();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn next_uuidv7_matches_the_canonical_observation_schemas_uuidv7_pattern() {
        let re_version = |c: char| c == '7';
        let re_variant = |c: char| matches!(c, '8' | '9' | 'a' | 'b');
        for _ in 0..64 {
            let id = next_uuidv7();
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(parts.len(), 5, "{id}");
            assert_eq!(parts[0].len(), 8);
            assert_eq!(parts[1].len(), 4);
            assert_eq!(parts[2].len(), 4);
            assert_eq!(parts[3].len(), 4);
            assert_eq!(parts[4].len(), 12);
            assert!(re_version(parts[2].chars().next().unwrap()), "{id}");
            assert!(re_variant(parts[3].chars().next().unwrap()), "{id}");
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{id}"
            );
        }
    }

    #[test]
    fn next_uuidv7_never_repeats_across_many_calls_in_one_run() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2000 {
            assert!(
                seen.insert(next_uuidv7()),
                "uuidv7 collision within one run"
            );
        }
    }

    #[test]
    fn gap_draft_dedupe_seed_is_deterministic_for_the_same_digest_and_locator() {
        // Design doc §8: identity must be derived from the snapshot digest and the
        // record locator alone -- not from when the marker happened to be built.
        let a = gap_draft(
            Vendor::ClaudeCode,
            "sha256:aaa",
            "line:10",
            "parse_error",
            None,
            "2026-07-29T00:00:00Z",
            TimeBasis::Acquired,
        );
        let b = gap_draft(
            Vendor::ClaudeCode,
            "sha256:aaa",
            "line:10",
            "parse_error",
            None,
            "2026-07-29T05:00:00Z",
            TimeBasis::Acquired,
        );
        assert_eq!(
            a.dedupe_seed, b.dedupe_seed,
            "same snapshot digest + record locator must yield the same dedupe seed"
        );

        let different_locator = gap_draft(
            Vendor::ClaudeCode,
            "sha256:aaa",
            "line:11",
            "parse_error",
            None,
            "2026-07-29T00:00:00Z",
            TimeBasis::Acquired,
        );
        assert_ne!(a.dedupe_seed, different_locator.dedupe_seed);

        let different_digest = gap_draft(
            Vendor::ClaudeCode,
            "sha256:bbb",
            "line:10",
            "parse_error",
            None,
            "2026-07-29T00:00:00Z",
            TimeBasis::Acquired,
        );
        assert_ne!(a.dedupe_seed, different_digest.dedupe_seed);
    }

    #[test]
    fn gap_marker_finalizes_to_the_same_dedupe_key_across_two_separate_runs() {
        // The property re-running import actually needs: two independent `finalize`
        // calls (different id, different observed_at -- exactly what changes between
        // two real import runs) over the same bad record must still land on the same
        // `cclogdedupekey`, so the ledger's `ON CONFLICT(cclogdedupekey)` collapses
        // them instead of accumulating a duplicate gap marker per re-run.
        let obs1 = finalize(
            gap_draft(
                Vendor::ClaudeCode,
                "sha256:aaa",
                "line:10",
                "parse_error",
                None,
                "2026-07-29T00:00:00Z",
                TimeBasis::Acquired,
            ),
            "line:10",
            "dev_x",
            "2026-07-29T00:00:00Z",
        );
        let obs2 = finalize(
            gap_draft(
                Vendor::ClaudeCode,
                "sha256:aaa",
                "line:10",
                "parse_error",
                None,
                "2026-07-29T01:00:00Z",
                TimeBasis::Acquired,
            ),
            "line:10",
            "dev_x",
            "2026-07-29T01:00:00Z",
        );
        assert_ne!(obs1.id, obs2.id, "each run mints its own fresh id");
        assert_eq!(
            obs1.cclogdedupekey, obs2.cclogdedupekey,
            "a re-run over the same bad record must produce the same dedupe key"
        );
    }

    #[test]
    fn gap_marker_reason_and_detail_are_never_the_offending_content() {
        let draft = gap_draft(
            Vendor::ClaudeCode,
            "sha256:aaa",
            "line:10",
            "unmapped_kind",
            Some("mode".to_string()),
            "2026-07-29T00:00:00Z",
            TimeBasis::Occurred,
        );
        assert_eq!(draft.data["reason"], "unmapped_kind");
        assert_eq!(draft.data["detail"], "mode");
    }

    // -- end-to-end regression tests over a synthetic archive ---------------------
    //
    // These build a throwaway cclog root under the system temp directory, archive
    // bytes the test itself wrote into it through the public `Ledger` API, and run
    // the real `run_import` against it. No vendor directory is read and no real
    // transcript is involved.

    use std::path::PathBuf;

    const TEST_LOCATOR: &str = ".claude/projects/synthetic/session.jsonl";
    const TEST_SESSION: &str = "11111111-1111-4111-8111-111111111111";
    /// The synthetic home the identity tests resolve their cwds against. Paired with
    /// `/Users/dev/ghq/...` paths: an obviously-fake user, never a real one.
    const TEST_HOME: &str = "/Users/dev";

    /// A throwaway cclog root, removed when the test's binding drops (including on
    /// an assertion panic, so a failing test does not leave the directory behind).
    ///
    /// `pub(crate)` so `report.rs`'s tests build their synthetic ledgers the same way
    /// rather than growing a second copy of this that could drift out of step with it.
    pub(crate) struct TempRoot(PathBuf);

    impl TempRoot {
        pub(crate) fn new(name: &str) -> Self {
            let unique = next_uuidv7();
            let path = std::env::temp_dir().join(format!("cclog-import-{name}-{unique}"));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One synthetic human-prompt transcript record, serialized as it would appear
    /// on a single JSONL line.
    fn prompt_line(uuid: &str, timestamp: &str) -> String {
        prompt_line_at(uuid, timestamp, "/SYNTHETIC/work/repo")
    }

    /// [`prompt_line`] with the `cwd` supplied, for the tests that care where the
    /// work happened rather than merely that it did.
    fn prompt_line_at(uuid: &str, timestamp: &str, cwd: &str) -> String {
        json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": TEST_SESSION,
            "timestamp": timestamp,
            "cwd": cwd,
            "isSidechain": false,
            "isMeta": false,
            "message": { "role": "user", "content": "SYNTHETIC prompt" },
        })
        .to_string()
    }

    fn archive_snapshot(root: &Path, bytes: &[u8], acquired_at: &str) {
        archive_snapshot_as(root, TEST_LOCATOR, bytes, acquired_at);
    }

    fn archive_snapshot_as(root: &Path, locator: &str, bytes: &[u8], acquired_at: &str) {
        let mut ledger = Ledger::open(root).expect("open ledger");
        ledger
            .archive_file(CLAUDE_SOURCE_KIND, locator, bytes, acquired_at, None)
            .expect("archive snapshot");
    }

    /// [`archive_snapshot`] for a named vendor, using that vendor's own synthetic
    /// locator -- a locator belongs to exactly one vendor, so the two are chosen
    /// together rather than passed separately.
    fn archive_snapshot_for(
        root: &Path,
        source_kind: &str,
        bytes: impl AsRef<[u8]>,
        acquired_at: &str,
    ) {
        let locator = if source_kind == CODEX_SOURCE_KIND {
            CODEX_LOCATOR
        } else {
            TEST_LOCATOR
        };
        let mut ledger = Ledger::open(root).expect("open ledger");
        ledger
            .archive_file(source_kind, locator, bytes.as_ref(), acquired_at, None)
            .expect("archive snapshot");
    }

    /// [`archive_snapshot_for`] for a Codex locator the caller names, rather than the
    /// vendor's one fixed synthetic locator ([`CODEX_LOCATOR`]) -- for the tests that
    /// need more than one Codex file archived at once, which `archive_snapshot_for`
    /// cannot model (it always writes to that same single locator).
    fn archive_codex_snapshot(
        root: &Path,
        locator: &str,
        bytes: impl AsRef<[u8]>,
        acquired_at: &str,
    ) {
        let mut ledger = Ledger::open(root).expect("open ledger");
        ledger
            .archive_file(
                CODEX_SOURCE_KIND,
                locator,
                bytes.as_ref(),
                acquired_at,
                None,
            )
            .expect("archive snapshot");
    }

    /// The two vendors' wire names, spelled out here rather than taken from
    /// [`Vendor::source_kind`]: these are what a snapshot row on disk actually says,
    /// so a test that took them from the code under test could not catch that code
    /// renaming one.
    const CLAUDE_SOURCE_KIND: &str = "claude-code";
    const CODEX_SOURCE_KIND: &str = "codex";

    // -- synthetic Codex transcripts ----------------------------------------------
    //
    // Shaped after `adapters/codex/shapes/*.shape.json`, which reproduce the real
    // record shapes with no real content. Every value here is synthetic.

    /// A synthetic Codex locator, in the shape `cclogger archive` discovers under
    /// `.codex/sessions` (see `discover.rs`).
    const CODEX_LOCATOR: &str = ".codex/sessions/2026/08/01/synthetic.jsonl";
    /// The session id the synthetic `session_meta` names. Deliberately not
    /// [`TEST_SESSION`], so a vendor mix-up cannot pass by accident.
    const CODEX_SESSION: &str = "22222222-2222-4222-8222-222222222222";

    /// The one record kind that carries a Codex session's identity and its `cwd`.
    ///
    /// `id` differs from `session_id` on purpose: measured over the real corpus,
    /// `session_meta` is re-announced up to 30 times per file with 2--3 distinct `id`
    /// values, while `session_id` takes exactly one. A pre-scan that keyed on `id`
    /// would split this file's one session into several.
    fn codex_session_meta(cwd: &str) -> String {
        codex_session_meta_with(
            "2026-07-20T02:00:00.000Z",
            cwd,
            "ffffffff-6666-4666-8666-666666666666",
            Some(CODEX_SESSION),
        )
    }

    /// [`codex_session_meta`] with the two id fields supplied, for the tests that turn
    /// on which of them names the session. `session_id: None` is the older-CLI variant
    /// `session-meta-variants.shape.json`'s first record reproduces -- 4 of the 328
    /// real files carry no `session_id` at all.
    pub(crate) fn codex_session_meta_with(
        timestamp: &str,
        cwd: &str,
        id: &str,
        session_id: Option<&str>,
    ) -> String {
        codex_session_meta_extra(timestamp, cwd, id, session_id, json!({}))
    }

    /// [`codex_session_meta_with`] plus whatever lineage metadata the case under test
    /// needs merged into the payload (`forked_from_id`, `parent_thread_id`,
    /// `history_base`, `subagent_history_start_ordinal`, an object-valued `source`).
    ///
    /// Merged rather than spelled out per case so every one of those variants is the
    /// *same* `session_meta` in every other respect: a rule test that also varied
    /// `cwd` or `cli_version` could pass for the wrong reason.
    pub(crate) fn codex_session_meta_extra(
        timestamp: &str,
        cwd: &str,
        id: &str,
        session_id: Option<&str>,
        extra: Value,
    ) -> String {
        let mut payload = json!({
            "id": id,
            "cwd": cwd,
            "cli_version": "0.144.2",
            "originator": "cli",
            "source": "interactive",
        });
        if let Some(session_id) = session_id {
            payload["session_id"] = json!(session_id);
        }
        for (key, value) in extra.as_object().expect("extra must be a JSON object") {
            payload[key] = value.clone();
        }
        json!({ "type": "session_meta", "timestamp": timestamp, "payload": payload }).to_string()
    }

    /// One `event_msg:user_message` -- the human prompt, and the primary signal.
    ///
    /// It carries **no session field at all**: measured over all 328 real transcript
    /// files, this payload's keys are exactly `type`, `message`, `local_images`,
    /// `text_elements`, `images`, `client_id`, `audio`, `local_audio`. That is what
    /// makes the pre-scan load-bearing rather than an optimization.
    pub(crate) fn codex_user_message(timestamp: &str, text: &str) -> String {
        codex_user_message_from(timestamp, text, "SYNTHETIC-client-a")
    }

    /// The per-turn record that carries a `cwd` but no session, and whose kind this
    /// adapter does not map. Present in the benign fixtures because it is one of the
    /// three records a real deferred first flush writes in a single millisecond.
    pub(crate) fn codex_turn_context(timestamp: &str, cwd: &str) -> String {
        json!({
            "type": "turn_context",
            "timestamp": timestamp,
            "payload": {
                "turn_id": "turn_synthetic1",
                "cwd": cwd,
                "model": "synthetic-model",
                "approval_policy": "on-request",
            },
        })
        .to_string()
    }

    /// [`codex_user_message`] with `client_id` supplied. One of the four real
    /// re-announcement groups differs in nothing else.
    fn codex_user_message_from(timestamp: &str, text: &str, client: &str) -> String {
        json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "user_message",
                "message": text,
                "client_id": client,
                "local_images": [],
            },
        })
        .to_string()
    }

    fn codex_agent_message(timestamp: &str, text: &str) -> String {
        json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": { "type": "agent_message", "message": text },
        })
        .to_string()
    }

    /// One `event_msg:sub_agent_activity` -- a *parent* file's own record of having
    /// spawned a subagent, naming the *child's* thread id. The child's own file
    /// carries nothing of the kind; this is the only place the relation is ever
    /// stated (see [`codex_subagent_thread_ids`]).
    fn codex_sub_agent_activity(
        timestamp: &str,
        agent_thread_id: &str,
        agent_path: &str,
    ) -> String {
        json!({
            "type": "event_msg",
            "timestamp": timestamp,
            "payload": {
                "type": "sub_agent_activity",
                "kind": "started",
                "agent_thread_id": agent_thread_id,
                "agent_path": agent_path,
            },
        })
        .to_string()
    }

    /// A minimal but complete Codex transcript: the `session_meta` that names the
    /// session and its `cwd`, then one human prompt that names neither.
    fn codex_bytes(cwd: &str) -> String {
        format!(
            "{}\n{}\n",
            codex_session_meta(cwd),
            codex_user_message("2026-07-20T02:00:05.000Z", "SYNTHETIC prompt"),
        )
    }

    /// [`codex_bytes`] with the prompt re-announced on a later, non-adjacent line --
    /// the shape of all four real collision groups.
    ///
    /// The re-announcement carries a **different `client_id`**, which is the hardest of
    /// the four real groups and the only one this fixture can be built from: two
    /// byte-identical lines would collapse under any identity rule at all, including one
    /// that hashed the whole payload, so a fixture made of those could not fail when the
    /// rule is broken. Everything else about the two records is identical, exactly as
    /// measured.
    fn codex_bytes_with_repeated_prompt(cwd: &str) -> String {
        let announced =
            codex_user_message_from("2026-07-20T02:00:05.000Z", "SYNTHETIC prompt", "client-a");
        let re_announced =
            codex_user_message_from("2026-07-20T02:00:05.000Z", "SYNTHETIC prompt", "client-b");
        format!(
            "{}\n{announced}\n{}\n{re_announced}\n",
            codex_session_meta(cwd),
            codex_agent_message("2026-07-20T02:00:06.000Z", "SYNTHETIC response"),
        )
    }

    // -- forked and benign rollouts -----------------------------------------------
    //
    // Shared with `report.rs`'s and `log.rs`'s test modules rather than copied into
    // them: the two day views and the importer must agree about which lines of a
    // forked rollout are copied history, and three fixtures that could drift apart
    // is exactly how they would come to disagree.

    /// The child thread a forked rollout *is* -- the canonical thread id, which is
    /// what upstream's own reader (`codex-rs/state/src/extract.rs`) keys the
    /// distinction off.
    pub(crate) const CODEX_CHILD_SESSION: &str = "33333333-3333-4333-8333-333333333333";
    /// The parent thread a forked rollout copied its history *from*.
    pub(crate) const CODEX_PARENT_SESSION: &str = "44444444-4444-4444-8444-444444444444";

    /// A synthetic forked Codex rollout: one copy flush that re-wrote the parent's
    /// history into the child's file, then the child's own live turn.
    ///
    /// The first three lines share one timestamp, because that is what a flush does --
    /// Codex stamps every record it serializes with `now_utc()` and drains its buffer
    /// in a tight loop, so one flush writes one millisecond across the batch. Two of
    /// those three are copies. The third, line 1, is the child's own `session_meta`,
    /// for which that millisecond is a true event time: it is when the fork happened.
    /// The last line is the child's own turn, two hours later.
    pub(crate) fn codex_forked_rollout(cwd: &str, copied_at: &str, live_at: &str) -> Vec<String> {
        vec![
            // The child's own `session_meta`, written when the file was created. It
            // names the parent it forked from; `parent_thread_id` is absent, which is
            // what makes this a user fork rather than a subagent spawn.
            codex_session_meta_extra(
                copied_at,
                cwd,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
                json!({ "forked_from_id": CODEX_PARENT_SESSION }),
            ),
            // The parent's `session_meta`, copied in verbatim -- a foreign `id` inside
            // the child's file, which is the marker the rule turns on.
            codex_session_meta_with(
                copied_at,
                cwd,
                CODEX_PARENT_SESSION,
                Some(CODEX_PARENT_SESSION),
            ),
            // The parent's human prompt, re-stamped with the copy's write time. Its
            // real original sits in the parent's own file, hours earlier.
            codex_user_message(copied_at, "SYNTHETIC copied prompt"),
            // And then the child's own live turn.
            codex_user_message(live_at, "SYNTHETIC live prompt"),
        ]
    }

    /// [`codex_forked_rollout`] with the parent's *assistant* turn copied in behind its
    /// prompt -- the shape a reaction time has to survive.
    ///
    /// The copied `agent_message` becomes a `response.completed` carrying the copy's
    /// write time, sitting between the fork and the child's own live prompt. Admitting
    /// it would report the live prompt as a reaction to output the person was shown
    /// hours earlier in another session and never answered here at all -- a gap of
    /// exactly `live_at - copied_at`, which is a property of when the fork ran.
    pub(crate) fn codex_forked_rollout_with_copied_response(
        cwd: &str,
        copied_at: &str,
        live_at: &str,
    ) -> Vec<String> {
        let mut lines = codex_forked_rollout(cwd, copied_at, live_at);
        // Behind the copied prompt and still inside the flush's one millisecond, so it
        // is copied by the same rule that copies the prompt above it.
        lines.insert(
            3,
            codex_agent_message(copied_at, "SYNTHETIC copied response"),
        );
        lines
    }

    /// A synthetic subagent rollout that states its own boundary: rule 2's
    /// `subagent_history_start_ordinal`, with an `ordinal` on every line.
    ///
    /// Unlike [`codex_forked_rollout`] the copied block here includes the child's own
    /// `session_meta` -- Codex has said ordinals below `S` are the inherited
    /// projection, and rule 2 does not carve exceptions into a number upstream
    /// computed. That is what makes this the fixture for a day holding *nothing* but
    /// copies, which a fork's opening flush can no longer produce.
    pub(crate) fn codex_subagent_rollout(cwd: &str, copied_at: &str, live_at: &str) -> Vec<String> {
        let with_ordinal = |ordinal: u64, line: String| {
            let mut record: Value = serde_json::from_str(&line).expect("a record");
            record["ordinal"] = json!(ordinal);
            record.to_string()
        };
        vec![
            with_ordinal(
                0,
                codex_session_meta_extra(
                    copied_at,
                    cwd,
                    CODEX_CHILD_SESSION,
                    Some(CODEX_CHILD_SESSION),
                    json!({
                        "parent_thread_id": CODEX_PARENT_SESSION,
                        "subagent_history_start_ordinal": 2,
                    }),
                ),
            ),
            with_ordinal(
                1,
                codex_user_message(copied_at, "SYNTHETIC projected prompt"),
            ),
            with_ordinal(2, codex_user_message(live_at, "SYNTHETIC live prompt")),
        ]
    }

    /// The benign look-alike a careless implementation destroys: an ordinary session
    /// whose file was not created until its first user message, so `session_meta`,
    /// `turn_context` and that first prompt share one timestamp.
    ///
    /// Nothing here was copied. Every line is a record of something that happened when
    /// it says, and the first prompt is the primary signal.
    pub(crate) fn codex_deferred_first_flush(cwd: &str, flushed_at: &str) -> Vec<String> {
        vec![
            codex_session_meta_with(
                flushed_at,
                cwd,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
            ),
            codex_turn_context(flushed_at, cwd),
            codex_user_message(flushed_at, "SYNTHETIC first prompt"),
        ]
    }

    /// The harder benign case: a rollout that *does* name a parent, whose first flush
    /// nevertheless copied nothing.
    ///
    /// A subagent whose history was not persisted into its own file still writes a
    /// `session_meta` carrying `parent_thread_id`, and still defers file creation to
    /// its first message -- so it has both a lineage marker and a one-millisecond
    /// opening run. What it does not have is a foreign `session_meta` embedded in that
    /// run, and that is the whole of the difference.
    pub(crate) fn codex_parented_deferred_first_flush(cwd: &str, flushed_at: &str) -> Vec<String> {
        vec![
            codex_session_meta_extra(
                flushed_at,
                cwd,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
                json!({
                    "parent_thread_id": CODEX_PARENT_SESSION,
                    "source": { "subagent": { "thread_spawn": { "agent_nickname": "SYNTHETIC" } } },
                }),
            ),
            codex_turn_context(flushed_at, cwd),
            codex_user_message(flushed_at, "SYNTHETIC first prompt"),
        ]
    }

    /// The Claude Code counterpart of [`codex_bytes`], for the mixed-vendor test.
    fn claude_bytes(cwd: &str) -> String {
        format!(
            "{}\n",
            prompt_line_at("u-0001", "2026-07-20T02:00:00.000Z", cwd)
        )
    }

    /// The `cclogdedupekey` `run_import` would give the `prompt.submitted`
    /// observation derived from the record `uuid`, reconstructed the same way
    /// `ObservationDraft::finalize` builds it.
    fn prompt_dedupe_key(root: &Path, uuid: &str) -> String {
        dedupe_key(root, "prompt.submitted", uuid)
    }

    /// [`prompt_dedupe_key`] for any event the adapter seeds with
    /// `[session, event, uuid]`.
    fn dedupe_key(root: &Path, event: &str, uuid: &str) -> String {
        let device = std::fs::read_to_string(root.join("device_id")).expect("device_id");
        format!(
            "claude-code|{}|{}|{event}|{uuid}",
            device.trim(),
            pseudonymize("ses", TEST_SESSION)
        )
    }

    /// A cwd resolving under this machine's `$HOME`, which is what `run_import`
    /// strips. Built at run time rather than written down: nothing derived from a
    /// real machine is committed, and it only ever reaches a `TempRoot`.
    fn ghq_cwd(repo: &str) -> String {
        format!("{}/ghq/{repo}", std::env::var("HOME").unwrap_or_default())
    }

    #[test]
    fn a_record_torn_across_two_snapshots_is_imported_once_a_later_snapshot_completes_it() {
        // `cclogger archive` reads a live session file with a plain `fs::read` while
        // Claude Code may be mid-append, so a snapshot can end partway through a
        // record. With a line-count cursor that counts the torn line as read, the
        // completed record on that same line number is never transformed again --
        // and if it was a human prompt, the primary signal is silently lost while
        // the report says "gaps 0".
        let root = TempRoot::new("torn-tail");
        let line1 = prompt_line("u-0001", "2026-07-20T02:00:00.000Z");
        let line2 = prompt_line("u-0002", "2026-07-20T02:01:00.000Z");
        let line3 = prompt_line("u-0003", "2026-07-20T02:02:00.000Z");

        let torn = &line2[..line2.len() / 2];
        let snapshot_a = format!("{line1}\n{torn}");
        let snapshot_b = format!("{line1}\n{line2}\n{line3}\n");

        archive_snapshot(root.path(), snapshot_a.as_bytes(), "2026-07-31T00:00:00Z");
        run_import(root.path(), false).expect("first import");
        archive_snapshot(root.path(), snapshot_b.as_bytes(), "2026-07-31T01:00:00Z");
        let report = run_import(root.path(), false).expect("second import");

        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        assert!(
            ledger
                .observation_present(&prompt_dedupe_key(root.path(), "u-0002"))
                .unwrap(),
            "the human prompt torn in half by snapshot A must be imported once snapshot B \
             completes it, not skipped forever by a cursor that counted the torn line as read \
             ({report:?})"
        );
        assert!(
            ledger
                .observation_present(&prompt_dedupe_key(root.path(), "u-0003"))
                .unwrap(),
            "the records appended after the torn one must be imported too ({report:?})"
        );
        assert_eq!(
            ledger
                .observation_count(Some("dev.cclog.source.gap.v1"))
                .unwrap(),
            0,
            "an incomplete final line is not a parse failure -- it must leave no gap marker \
             behind, least of all one that outlives the record's arrival ({report:?})"
        );
        assert_eq!(
            ledger
                .observation_count(Some("dev.cclog.prompt.submitted.v1"))
                .unwrap(),
            3,
            "each of the three prompts must be imported exactly once ({report:?})"
        );
        assert_eq!(
            report.checkpoints_reset, 0,
            "growth by append must satisfy the prefix check, not trip a rescan ({report:?})"
        );
    }

    #[test]
    fn a_snapshot_that_is_not_its_predecessor_grown_by_append_resets_the_cursor_and_rescans() {
        // The prefix property the line-count cursor rests on is checked, not assumed.
        // When it does not hold -- a session file rewritten rather than appended to --
        // the safe move is to rescan from 0 and let dedupe collapse the overlap,
        // rather than skip `cursor` lines of content that is not what was counted.
        let root = TempRoot::new("prefix-break");
        let original = prompt_line("u-0001", "2026-07-20T02:00:00.000Z");
        let replacement = prompt_line("u-0009", "2026-07-20T03:00:00.000Z");
        let appended = prompt_line("u-0002", "2026-07-20T03:01:00.000Z");

        archive_snapshot(
            root.path(),
            format!("{original}\n").as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        run_import(root.path(), false).expect("first import");
        archive_snapshot(
            root.path(),
            format!("{replacement}\n{appended}\n").as_bytes(),
            "2026-07-31T01:00:00Z",
        );
        let report = run_import(root.path(), false).expect("second import");

        assert_eq!(
            report.checkpoints_reset, 1,
            "a snapshot whose line 1 differs from the one the cursor was counted against \
             must reset the cursor ({report:?})"
        );
        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        for uuid in ["u-0001", "u-0009", "u-0002"] {
            assert!(
                ledger
                    .observation_present(&prompt_dedupe_key(root.path(), uuid))
                    .unwrap(),
                "{uuid} must be in the ledger after the rescan ({report:?})"
            );
        }
    }

    #[test]
    fn a_locator_whose_only_new_line_is_incomplete_still_advances_its_checkpoint() {
        // Otherwise step 1 of the per-locator algorithm never short-circuits again
        // and the same bytes are re-read on every future run, forever.
        //
        // The load-bearing assertion is the checkpoint row's own `snapshot_id`, not
        // the third run's counters: with the old `start_line >= lines.len()` early
        // continue restored, the third run *also* reports "unchanged 1 / processed 0"
        // -- it just re-reads the bytes to get there, which is indistinguishable from
        // a genuine short-circuit at the counter level. Only the second run's
        // `locators_processed` and the persisted checkpoint tell the two apart.
        let root = TempRoot::new("advance-on-empty");
        let line1 = prompt_line("u-0001", "2026-07-20T02:00:00.000Z");
        let line2 = prompt_line("u-0002", "2026-07-20T02:01:00.000Z");

        archive_snapshot(
            root.path(),
            format!("{line1}\n").as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        run_import(root.path(), false).expect("first import");
        archive_snapshot(
            root.path(),
            format!("{line1}\n{}", &line2[..line2.len() / 2]).as_bytes(),
            "2026-07-31T01:00:00Z",
        );
        let second = run_import(root.path(), false).expect("second import");
        assert_eq!(
            second.locators_processed, 1,
            "a snapshot with no new complete line must still be processed through to \
             ingest, not short-circuited before the checkpoint advance ({second:?})"
        );
        assert_eq!(second.locators_unchanged, 0, "{second:?}");

        {
            let ledger = Ledger::open(root.path()).expect("reopen ledger");
            let latest = ledger
                .latest_snapshot(TEST_LOCATOR)
                .expect("latest_snapshot")
                .expect("a snapshot must exist");
            let checkpoint = ledger
                .checkpoint(CLAUDE_SOURCE_KIND, TEST_LOCATOR)
                .expect("checkpoint")
                .expect("a checkpoint must exist");
            assert_eq!(
                checkpoint.snapshot_id, latest.snapshot_id,
                "the checkpoint must name the newest snapshot even though that snapshot \
                 contributed no new complete line ({second:?})"
            );
        }

        let third = run_import(root.path(), false).expect("third import");
        assert_eq!(
            third.locators_unchanged, 1,
            "with the checkpoint on the newest snapshot, a third run short-circuits \
             instead of re-reading the bytes ({third:?})"
        );
        assert_eq!(third.locators_processed, 0, "{third:?}");
    }

    #[test]
    fn a_snapshot_ending_mid_record_is_counted_so_the_deferral_is_never_silent() {
        // Deferring a torn final line to the next snapshot is correct, but it is not
        // free: if the vendor deletes the session file before another `cclogger archive`
        // runs, that record is never imported and never gapped either -- a fragment
        // cannot be parsed or identified well enough to diagnose one. This counter is
        // the only place that outcome is visible.
        let root = TempRoot::new("incomplete-tail");
        let line1 = prompt_line("u-0001", "2026-07-20T02:00:00.000Z");
        let line2 = prompt_line("u-0002", "2026-07-20T02:01:00.000Z");

        archive_snapshot(
            root.path(),
            format!("{line1}\n{}", &line2[..line2.len() / 2]).as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        let torn = run_import(root.path(), false).expect("import of a torn snapshot");
        assert_eq!(
            torn.lines_incomplete, 1,
            "a snapshot ending mid-record must say so ({torn:?})"
        );
        assert_eq!(
            torn.total_gaps(),
            0,
            "and must not be reported as a gap -- the record is deferred, not lost ({torn:?})"
        );

        // The same file, completed: nothing is torn any more, so nothing is counted.
        archive_snapshot(
            root.path(),
            format!("{line1}\n{line2}\n").as_bytes(),
            "2026-07-31T01:00:00Z",
        );
        let whole = run_import(root.path(), false).expect("import of the completed snapshot");
        assert_eq!(
            whole.lines_incomplete, 0,
            "a snapshot ending on a record boundary must not be counted ({whole:?})"
        );
        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        assert!(
            ledger
                .observation_present(&prompt_dedupe_key(root.path(), "u-0002"))
                .unwrap(),
            "and the deferred record must land once it arrives whole ({whole:?})"
        );
    }

    #[test]
    fn a_mapped_kind_record_missing_a_required_field_is_gapped_not_silently_dropped() {
        // `type: "user"` is a kind the adapter understands, so the importer's
        // MAPPED_KINDS check treats zero drafts as "understood, no event here". For
        // an instance that is malformed rather than eventless -- here, a human
        // prompt with no `timestamp` -- that turns the gap machinery off exactly
        // where it is needed and the record vanishes with no trace at all.
        let root = TempRoot::new("missing-field");
        let good = prompt_line("u-0001", "2026-07-20T02:00:00.000Z");
        let malformed = json!({
            "type": "user",
            "uuid": "u-0002",
            "sessionId": TEST_SESSION,
            "cwd": "/SYNTHETIC/work/repo",
            "isSidechain": false,
            "isMeta": false,
            "message": { "role": "user", "content": "SYNTHETIC prompt" },
        })
        .to_string();

        let bytes = format!("{good}\n{malformed}\n");
        archive_snapshot(root.path(), bytes.as_bytes(), "2026-07-31T00:00:00Z");
        let report = run_import(root.path(), false).expect("import");

        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        assert_eq!(
            ledger
                .observation_count(Some("dev.cclog.source.gap.v1"))
                .unwrap(),
            1,
            "a record of a mapped kind that is missing a field that kind requires must be \
             diagnosed as a gap, not dropped ({report:?})"
        );
        assert!(
            ledger
                .observation_present(&prompt_dedupe_key(root.path(), "u-0001"))
                .unwrap(),
            "the well-formed record alongside it must still be imported ({report:?})"
        );
        assert_eq!(
            report.gap_missing_field.get("timestamp").copied(),
            Some(1),
            "the report must name the field that was missing ({report:?})"
        );
    }

    #[test]
    fn a_mapped_kind_record_that_legitimately_carries_no_event_is_counted_as_skipped() {
        // The other half of I3's classification: this record is not malformed, it
        // just is not evidence of anything schema v0 has an event for. It must not be
        // gapped -- but it must not be invisible either, or records-in can never be
        // reconciled against observations-out.
        let root = TempRoot::new("skipped");
        let sidechain = json!({
            "type": "user",
            "uuid": "u-0002",
            "sessionId": TEST_SESSION,
            "timestamp": "2026-07-20T02:01:00.000Z",
            "cwd": "/SYNTHETIC/work/repo",
            "isSidechain": true,
            "message": { "role": "user", "content": "SYNTHETIC subagent turn" },
        })
        .to_string();

        archive_snapshot(
            root.path(),
            format!("{sidechain}\n").as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        assert_eq!(
            report.records_skipped.get("user").copied(),
            Some(1),
            "a sidechain non-tool-result user record must be counted, not vanish ({report:?})"
        );
        assert_eq!(
            report.total_gaps(),
            0,
            "it is not a gap either ({report:?})"
        );
    }

    #[test]
    fn dry_run_against_a_root_with_no_ledger_creates_nothing() {
        // "(dry run -- nothing written)" was printed after `Ledger::open` had already
        // created the root, `archive/`, and `ledger.db`, and `device_id` had minted
        // and persisted a file.
        let root = TempRoot::new("dry-run-empty");
        let report = run_import(root.path(), true).expect("dry run");

        assert!(report.ledger_missing, "{report:?}");
        assert!(
            !root.path().exists(),
            "--dry-run must not bring the cclog root into existence"
        );
    }

    #[test]
    fn dry_run_against_an_out_of_date_ledger_refuses_rather_than_upgrading_it_in_place() {
        // `Ledger::open` reconciles an out-of-date schema and stamps `user_version`
        // forward as a side effect of opening. That is a write, and the one write a
        // dry run cannot apologise for afterwards: an older cclog refuses a ledger
        // stamped past what it understands, so there is no going back.
        let root = TempRoot::new("dry-run-schema-upgrade");
        let bytes = format!("{}\n", prompt_line("u-0001", "2026-07-20T02:00:00.000Z"));
        archive_snapshot(root.path(), bytes.as_bytes(), "2026-07-31T00:00:00Z");

        // Put the ledger back into the shape it had before `repository_ref` existed.
        // Reached with SQL because it cannot be reached through `Ledger`: opening one
        // is precisely the write under test.
        {
            let db = rusqlite::Connection::open(root.path().join("ledger.db")).unwrap();
            db.execute_batch(
                "DROP INDEX IF EXISTS observation_repository_ref;
                 ALTER TABLE observation DROP COLUMN repository_ref;
                 DROP TABLE IF EXISTS workspace_identity;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        let report = run_import(root.path(), true).expect("dry run");
        assert!(
            report.ledger_needs_upgrade,
            "the dry run must report that it stopped, not silently do nothing ({report:?})"
        );
        assert!(
            !report.ledger_missing,
            "there is a ledger -- it is out of date, which is a different thing ({report:?})"
        );
        assert!(
            report.observations_created.is_empty() && report.locators_scanned == 0,
            "and must not have reported on content it never read ({report:?})"
        );

        let db = rusqlite::Connection::open(root.path().join("ledger.db")).unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 1,
            "the dry run must leave the on-disk schema version exactly where it was"
        );
        let columns: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('observation') WHERE name = 'repository_ref'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(columns, 0, "and must not have reconciled the schema either");

        // The real run is what performs it -- so the guard defers the upgrade rather
        // than blocking it forever.
        drop(db);
        run_import(root.path(), false).expect("real import");
        let db = rusqlite::Connection::open(root.path().join("ledger.db")).unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2, "a real run upgrades it");
    }

    #[test]
    fn dry_run_counts_what_a_real_run_would_create_rather_than_recounting_what_is_there() {
        // Dry run used to mark every transformed observation `Created`, so its
        // "observations created" overstated a real run. There are two ways that
        // happens and the test covers both, because fixing only the first is what the
        // re-review caught: observations already in the ledger, and observations
        // repeated within the same run.
        let root = TempRoot::new("dry-run-counts");
        let bytes = format!("{}\n", prompt_line("u-0001", "2026-07-20T02:00:00.000Z"));
        archive_snapshot(root.path(), bytes.as_bytes(), "2026-07-31T00:00:00Z");

        let first = run_import(root.path(), true).expect("dry run before any import");
        assert_eq!(
            first
                .observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied(),
            Some(1),
            "nothing is in the ledger yet, so this observation would be created ({first:?})"
        );

        // A resumed session: Claude Code copies the earlier session's records verbatim
        // into the new session file, so the same record reaches the importer twice
        // under two locators. On a *first* dry run -- an empty observation table, which
        // is when someone actually runs `--dry-run` -- the ledger has neither copy, so
        // only an in-run set of already-counted keys can get this right. This is the
        // case the 500 real resume duplicates fall into.
        const RESUMED: &str = ".claude/projects/synthetic/resumed.jsonl";
        archive_snapshot_as(
            root.path(),
            RESUMED,
            bytes.as_bytes(),
            "2026-07-31T01:00:00Z",
        );

        let both = run_import(root.path(), true).expect("dry run over both copies");
        assert_eq!(
            both.observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied(),
            Some(1),
            "the two copies share one dedupe key, so a real run creates one row -- a dry \
             run must not report two ({both:?})"
        );
        assert_eq!(
            both.observations_already_present, 1,
            "and must account for the second copy as the duplicate it is ({both:?})"
        );

        // The same numbers a real run produces, which is the property the name claims.
        let real = run_import(root.path(), false).expect("real import");
        assert_eq!(
            real.observations_created, both.observations_created,
            "dry run and real run must agree on what gets created ({real:?} vs {both:?})"
        );
        assert_eq!(
            real.observations_already_present, both.observations_already_present,
            "and on what is already present ({real:?} vs {both:?})"
        );

        // And once the rows really are in the ledger, a further dry run says so.
        archive_snapshot_as(
            root.path(),
            ".claude/projects/synthetic/resumed-again.jsonl",
            bytes.as_bytes(),
            "2026-07-31T02:00:00Z",
        );
        let second = run_import(root.path(), true).expect("dry run after the import");
        assert!(
            second.observations_created.is_empty(),
            "the observation is already in the ledger, so a real run would create nothing \
             ({second:?})"
        );
        assert_eq!(second.observations_already_present, 1, "{second:?}");
    }

    #[test]
    fn an_implausible_vendor_type_string_becomes_neither_gap_detail_nor_a_report_label() {
        // The gap path exists precisely for records that do not look like the
        // surveyed corpus, so its one vendor-controlled field is validated before it
        // becomes a T1 ledger row and a line on stdout.
        let root = TempRoot::new("hostile-kind");
        let hostile = json!({
            "type": "SYNTHETIC free text /Users/someone/notes about a merger",
            "sessionId": TEST_SESSION,
        })
        .to_string();
        archive_snapshot(
            root.path(),
            format!("{hostile}\n").as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        assert_eq!(
            report.gap_unmapped_kind.get(UNPRINTABLE_KIND).copied(),
            Some(1),
            "an implausible type string must be bucketed under a fixed label ({report:?})"
        );
        for key in report.gap_unmapped_kind.keys() {
            assert!(
                !key.contains("merger"),
                "report key leaked vendor text: {key}"
            );
        }

        let draft = classify_line(
            Vendor::ClaudeCode,
            &hostile,
            &Keystore::new(),
            "sha256:aaa",
            "2026-07-31T00:00:00Z",
            1,
            false,
        );
        let LineOutcome::Gap { draft, .. } = draft else {
            panic!("an unmapped kind must produce a gap");
        };
        assert_eq!(
            draft.data["detail"],
            Value::Null,
            "an implausible type string must not become the marker's detail"
        );
        assert_eq!(
            kind_detail("file-history_snapshot").as_deref(),
            Some("file-history_snapshot"),
            "a plausible kind name is still carried through verbatim"
        );
    }

    /// Golden test for `adapters/claude-code/fixtures/import-gap.fixture.json`.
    ///
    /// Gap markers are the importer's product, not the adapter's, so they appear in
    /// none of the adapter golden fixtures -- which left the highest-volume event
    /// class this branch emits pinned by nothing and schema-validated by nothing. The
    /// fixture closes both holes at once: `tools/conformance` validates its `expected`
    /// entries against the canonical schema and the leak scan, and this test asserts
    /// the importer still produces exactly them -- one line per `GapBucket` variant,
    /// plus a second unmapped-kind line so both `time_basis` values are covered.
    #[test]
    fn import_gap_fixture_matches_what_the_importer_produces_for_each_gap_reason() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../adapters/claude-code/fixtures/import-gap.fixture.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let fx: Value = serde_json::from_str(&raw).expect("parse fixture json");

        let source = &fx["source"];
        let digest = source["snapshot_digest"].as_str().expect("snapshot_digest");
        let acquired_at = source["acquired_at"].as_str().expect("acquired_at");
        let device = source["device"].as_str().expect("device");
        let lines = source["lines"].as_array().expect("lines");
        let expected = fx["expected"].as_array().expect("expected");
        assert_eq!(
            lines.len(),
            expected.len(),
            "every line in this fixture must gap"
        );

        // The same whole-file pre-scan `run_import` does, so the classification runs
        // against a keystore that has already resolved what it can.
        let mut scan = PreScan::default();
        for line in lines {
            if let Ok(record) = serde_json::from_str::<Value>(line.as_str().expect("line")) {
                scan.observe(&record, TEST_HOME);
            }
        }
        let (keystore, _) = scan.finish();

        for (i, (line, want)) in lines.iter().zip(expected).enumerate() {
            let line_no = i + 1;
            let outcome = classify_line(
                Vendor::ClaudeCode,
                line.as_str().expect("line"),
                &keystore,
                digest,
                acquired_at,
                line_no,
                false,
            );
            let LineOutcome::Gap { draft, .. } = outcome else {
                panic!("line {line_no} must produce a gap, got {outcome:?}");
            };
            let got = finalize_with_id(
                *draft,
                want["id"].as_str().expect("id").to_string(),
                &format!("line:{line_no}"),
                device,
                want["cclogobservedat"].as_str().expect("cclogobservedat"),
            );
            assert_eq!(
                &serde_json::to_value(&got).unwrap(),
                want,
                "import-gap.fixture.json[{i}]: canonical observation mismatch"
            );
        }
    }

    #[test]
    fn the_pre_scan_carries_a_tool_use_start_time_to_the_adapters_tool_result_arm() {
        // The importer and the adapter agree on the Keystore entry `tool_started_at`
        // and nothing else enforces that: the adapter's own golden fixtures spell the
        // entry out by hand, so a rename on either side would leave every historical
        // `duration_ms` silently null while both crates' tests still passed.
        let started = json!({
            "type": "assistant",
            "uuid": "a-0001",
            "sessionId": TEST_SESSION,
            "timestamp": "2026-07-20T02:00:12.000Z",
            "message": {
                "content": [{ "type": "tool_use", "id": "toolu_1", "name": "Bash" }],
                "usage": { "output_tokens": 3 },
            },
        });
        let finished = json!({
            "type": "user",
            "uuid": "u-0001",
            "sessionId": TEST_SESSION,
            "timestamp": "2026-07-20T02:00:19.000Z",
            "toolUseResult": { "stdout": "SYNTHETIC", "exit_code": 0 },
            "message": { "content": [{ "type": "tool_result", "tool_use_id": "toolu_1" }] },
        });

        let mut scan = PreScan::default();
        scan.observe(&started, TEST_HOME);
        scan.observe(&finished, TEST_HOME);
        let (keystore, _) = scan.finish();

        let drafts = claude_code_history::transform(&finished, &keystore);
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].data["duration_ms"],
            json!(7000),
            "the tool_use record's timestamp must reach the tool-result arm through the \
             pre-scan, so the emitted duration is measured rather than fabricated"
        );
    }

    #[test]
    fn the_pre_scan_is_deterministic_across_two_independent_scans() {
        let cwd = "/Users/dev/ghq/github.com/acme/api";
        let record = json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "cwd": cwd,
            "message": {
                "content": [{ "type": "tool_use", "id": "toolu_1", "name": "Bash" }]
            }
        });
        let scan_once = || {
            let mut scan = PreScan::default();
            scan.observe(&record, TEST_HOME);
            scan.finish().0
        };
        let ks1 = scan_once();
        let ks2 = scan_once();
        assert!(
            ks1.resolve("workspace", cwd).is_some(),
            "the cwd must actually resolve, or the equalities below compare None to None"
        );
        assert_eq!(
            ks1.resolve("session", "sess-1"),
            ks2.resolve("session", "sess-1")
        );
        assert_eq!(ks1.resolve("workspace", cwd), ks2.resolve("workspace", cwd));
        assert_eq!(
            ks1.resolve("repository", cwd),
            ks2.resolve("repository", cwd)
        );
        assert_eq!(
            ks1.resolve("tool", "toolu_1"),
            ks2.resolve("tool", "toolu_1")
        );
        assert_eq!(
            ks1.resolve("tool_family", "toolu_1").as_deref(),
            Some("shell")
        );
    }

    // -- workspace / repository identity ------------------------------------------

    #[test]
    fn a_subdirectory_and_its_repository_root_share_one_repository_ref() {
        let mut scan = PreScan::default();
        for cwd in [
            "/Users/dev/ghq/github.com/acme/api",
            "/Users/dev/ghq/github.com/acme/api/src",
        ] {
            scan.observe(
                &json!({ "type": "user", "sessionId": "s1", "cwd": cwd }),
                "/Users/dev",
            );
        }
        let (ks, _) = scan.finish();
        assert!(
            ks.resolve("repository", "/Users/dev/ghq/github.com/acme/api")
                .is_some(),
            "both must resolve to a ref, not merely agree on having none"
        );
        assert_eq!(
            ks.resolve("repository", "/Users/dev/ghq/github.com/acme/api"),
            ks.resolve("repository", "/Users/dev/ghq/github.com/acme/api/src"),
            "a subdirectory is the same repository"
        );
    }

    #[test]
    fn a_worktree_shares_its_repository_but_not_its_workspace() {
        let mut scan = PreScan::default();
        let root = "/Users/dev/ghq/github.com/acme/api";
        let tree = "/Users/dev/ghq/github.com/acme/api/.worktrees/issue-62";
        for cwd in [root, tree] {
            scan.observe(
                &json!({ "type": "user", "sessionId": "s1", "cwd": cwd }),
                "/Users/dev",
            );
        }
        let (ks, _) = scan.finish();
        assert!(ks.resolve("repository", root).is_some());
        assert!(ks.resolve("workspace", tree).is_some());
        assert_eq!(
            ks.resolve("repository", root),
            ks.resolve("repository", tree),
            "a worktree belongs to its repository"
        );
        assert_ne!(
            ks.resolve("workspace", root),
            ks.resolve("workspace", tree),
            "but it is its own workspace"
        );
    }

    #[test]
    fn a_session_split_across_subdirectories_votes_as_one_repository() {
        // The vote is over *resolved* identities, not raw cwd strings. Counting raw
        // strings makes a repository split its own vote across its subdirectories
        // while a rival concentrates its own: here acme/api holds 4 of the 7 located
        // records but no single cwd beats acme/other's 3, so every cwd-less record in
        // the session would be attributed to the repository it was mostly *not* in --
        // deterministically, and permanently, since `ON CONFLICT DO NOTHING` never
        // rewrites the row.
        let mut scan = PreScan::default();
        let api_src = "/Users/dev/ghq/github.com/acme/api/src";
        let api_lib = "/Users/dev/ghq/github.com/acme/api/lib";
        let other = "/Users/dev/ghq/github.com/acme/other";
        for cwd in [api_src, api_src, api_lib, api_lib, other, other, other] {
            scan.observe(
                &json!({ "type": "user", "sessionId": "s1", "cwd": cwd }),
                "/Users/dev",
            );
        }
        let (ks, identities) = scan.finish();
        assert!(
            ks.resolve("session_repository", "s1").is_some(),
            "the session must be attributed at all, or the equality below is vacuous"
        );
        assert_eq!(
            ks.resolve("session_repository", "s1"),
            ks.resolve("repository", api_src),
            "acme/api holds 4 of the 7 located records; acme/other only 3"
        );
        assert_ne!(
            ks.resolve("session_repository", "s1"),
            ks.resolve("repository", other),
            "the rival that merely concentrated its cwds must not win"
        );
        assert_session_pair_is_coherent(&ks, &identities, "s1");
    }

    #[test]
    fn the_winning_workspace_always_sits_inside_the_winning_repository() {
        // A workspace identity is *defined* as living inside a repository -- it is
        // literally `repository` or `repository@branch`. So the two votes cannot be
        // taken independently: a pair whose workspace is not under its repository is
        // not a vaguer answer, it is an impossible one, and any consumer joining the
        // two would read a single record as being in two different projects.
        //
        // The repository settles first, and the workspace vote then runs over only
        // that repository's records. Here acme/api wins 4 to 3; the workspace vote is
        // then between api@one and api@two at 2 each, tie-broken to the smallest as
        // everywhere else. acme/other's 3 records take no part in it -- counting
        // workspaces across repositories is not a meaningful comparison to begin with.
        let mut scan = PreScan::default();
        let one = "/Users/dev/ghq/github.com/acme/api/.worktrees/one";
        let two = "/Users/dev/ghq/github.com/acme/api/.worktrees/two";
        let other = "/Users/dev/ghq/github.com/acme/other";
        for cwd in [one, one, two, two, other, other, other] {
            scan.observe(
                &json!({ "type": "user", "sessionId": "s1", "cwd": cwd }),
                "/Users/dev",
            );
        }
        let (ks, identities) = scan.finish();
        assert_eq!(
            ks.resolve("session_repository", "s1"),
            ks.resolve("repository", one),
            "the repository majority counts both worktrees as acme/api: 4 against 3"
        );
        assert_eq!(
            ks.resolve("session_workspace", "s1"),
            ks.resolve("workspace", one),
            "the workspace vote runs only over acme/api's records, and api@one wins \
             the 2-2 tie as the smallest"
        );
        assert_ne!(
            ks.resolve("session_workspace", "s1"),
            ks.resolve("workspace", other),
            "the losing repository's workspace must not be borrowed, however many \
             records named it"
        );
        assert_session_pair_is_coherent(&ks, &identities, "s1");
    }

    #[test]
    fn the_workspace_vote_counts_records_rather_than_taking_the_first_identity() {
        // The test above settles its workspace stage on a 2-2 tie, so it passes just
        // as well against an implementation that ignores counts entirely and returns
        // the smallest key. This one cannot: the majority workspace is deliberately
        // NOT the lexicographic minimum. `github.com/acme/api` (the main checkout,
        // 1 record) sorts before `github.com/acme/api@zz` (the worktree, 3 records),
        // so a first-key-wins stage 2 answers with the main checkout and fails here.
        //
        // The coherence invariant alone would not catch it either -- both candidates
        // sit inside `github.com/acme/api`, so the pair stays coherent while the
        // attribution is wrong.
        let mut scan = PreScan::default();
        let main = "/Users/dev/ghq/github.com/acme/api";
        let worktree = "/Users/dev/ghq/github.com/acme/api/.worktrees/zz";
        for cwd in [main, worktree, worktree, worktree] {
            scan.observe(
                &json!({ "type": "user", "sessionId": "s1", "cwd": cwd }),
                "/Users/dev",
            );
        }
        let (ks, identities) = scan.finish();
        assert_eq!(
            ks.resolve("session_workspace", "s1"),
            ks.resolve("workspace", worktree),
            "api@zz has 3 records against the main checkout's 1, and must win on count \
             even though it sorts last"
        );
        assert_session_pair_is_coherent(&ks, &identities, "s1");
    }

    /// The invariant behind [`the_winning_workspace_always_sits_inside_the_winning_repository`],
    /// asserted directly on the registry's display names: whenever a session gets both
    /// a fallback repository and a fallback workspace, the workspace must be that
    /// repository or a worktree of it. Cheap enough to call from every vote test, and
    /// it catches variants of this bug that a fixture-specific assertion would not.
    fn assert_session_pair_is_coherent(
        ks: &Keystore,
        identities: &BTreeMap<String, (String, String)>,
        session: &str,
    ) {
        let display = |opaque: Option<String>| -> Option<String> {
            identities.get(&opaque?).map(|(_, display)| display.clone())
        };
        let (Some(repository), Some(workspace)) = (
            display(ks.resolve("session_repository", session)),
            display(ks.resolve("session_workspace", session)),
        ) else {
            return;
        };
        assert!(
            workspace == repository || workspace.starts_with(&format!("{repository}@")),
            "session {session}'s workspace `{workspace}` does not sit inside its \
             repository `{repository}`"
        );
    }

    #[test]
    fn a_session_takes_the_cwd_most_of_its_records_name() {
        // The majority is deliberately *not* the lexicographically smallest of the
        // two: a `majority()` that ignored counts and returned the first key of an
        // ordered map would otherwise pass this test while claiming to count.
        let mut scan = PreScan::default();
        let major = "/Users/dev/ghq/github.com/acme/zulu";
        let minor = "/Users/dev/ghq/github.com/acme/alpha";
        for _ in 0..3 {
            scan.observe(
                &json!({ "type": "user", "sessionId": "s1", "cwd": major }),
                "/Users/dev",
            );
        }
        scan.observe(
            &json!({ "type": "user", "sessionId": "s1", "cwd": minor }),
            "/Users/dev",
        );
        let (ks, _) = scan.finish();
        assert!(
            ks.resolve("session_repository", "s1").is_some(),
            "the session must be attributed at all, or the equality below is vacuous"
        );
        assert_eq!(
            ks.resolve("session_repository", "s1"),
            ks.resolve("repository", major),
            "the majority cwd wins the session"
        );
        assert_ne!(
            ks.resolve("session_repository", "s1"),
            ks.resolve("repository", minor),
            "and the minority one does not"
        );
    }

    #[test]
    fn a_tied_majority_vote_resolves_the_same_way_on_every_run() {
        // Re-imports must assign the same refs, or dedupe keys change and every row
        // is inserted a second time.
        let a = "/Users/dev/ghq/github.com/acme/alpha";
        let z = "/Users/dev/ghq/github.com/acme/zulu";
        let run = |order: [&str; 2]| {
            let mut scan = PreScan::default();
            for cwd in order {
                scan.observe(
                    &json!({ "type": "user", "sessionId": "s1", "cwd": cwd }),
                    "/Users/dev",
                );
            }
            scan.finish().0.resolve("session_repository", "s1")
        };
        assert_eq!(run([a, z]), run([z, a]));
        // Pinning *which* way it resolves is what makes the equality above mean
        // something: a tie-break that depended on iteration order (a hash map) or on
        // the last equal count seen (`>=`) would still be self-consistent within one
        // run while assigning a different ref on the next one.
        let smallest = pseudonymize("rep", "github.com/acme/alpha");
        assert_eq!(
            run([a, z]).as_deref(),
            Some(smallest.as_str()),
            "a tie resolves to the lexicographically smallest cwd, on every run"
        );
    }

    #[test]
    fn a_dry_run_registers_no_identity() {
        // `--dry-run` prints "nothing written". The registry is a write like any other.
        let root = TempRoot::new("dry-run-registry");
        // The importer resolves a cwd against *this* machine's `$HOME`, so the
        // synthetic ghq path is built from it at run time rather than written down.
        // Nothing derived from a real machine is committed, and the record itself
        // never leaves the throwaway root above.
        let cwd = format!(
            "{}/ghq/github.com/acme/api",
            std::env::var("HOME").unwrap_or_default()
        );
        let bytes = format!(
            "{}\n",
            prompt_line_at("u-0001", "2026-07-20T02:00:00.000Z", &cwd)
        );
        // Archiving already opens a `Ledger`, which creates `ledger.db`, so the dry
        // run below does not short-circuit on `ledger_missing`. No real import runs
        // first -- that would populate the registry and leave this test asserting
        // against its own setup rather than against the dry run.
        archive_snapshot(root.path(), bytes.as_bytes(), "2026-07-31T00:00:00Z");

        let report = run_import(root.path(), true).unwrap();
        assert!(
            !report.ledger_missing,
            "the dry run must have had a ledger to skip writing to"
        );
        assert!(
            report.observations_created.values().sum::<u64>() > 0,
            "the fixture must actually produce observations, or this test proves nothing"
        );
        {
            let ledger = Ledger::open(root.path()).expect("reopen ledger");
            assert_eq!(
                ledger.identities("repository").unwrap(),
                vec![],
                "a dry run must not write the identity registry"
            );
        }

        // The other half of the claim: the same fixture through a *real* run does
        // register one. Without this the emptiness above would also hold for a cwd
        // that simply never resolved, and the dry-run guard would be untested.
        run_import(root.path(), false).expect("real import");
        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        assert_eq!(
            ledger
                .identities("repository")
                .unwrap()
                .iter()
                .map(|(_, display)| display.as_str())
                .collect::<Vec<_>>(),
            vec!["github.com/acme/api"],
            "a real run over the same bytes must register the repository the dry run skipped"
        );
    }

    #[test]
    fn a_gap_marker_is_not_counted_as_an_unattributed_observation() {
        // A gap stands for a line that could not be transformed at all, so it never
        // had an attribution to lose. Counting gaps here would peg the number at no
        // less than the gap count and make it useless: on the real corpus that is
        // 36,842 gaps against 11,255 genuinely unattributed observations.
        let root = TempRoot::new("gap-not-unattributed");
        let home = std::env::var("HOME").expect("HOME");
        let resolvable = format!("{home}/ghq/github.com/acme/api");
        let lines = format!(
            "{}\n{}\n",
            prompt_line_at("u-0001", "2026-07-20T02:00:00.000Z", &resolvable),
            r#"{"type":"mode","sessionId":"s1","timestamp":"2026-07-20T02:00:01.000Z","uuid":"u-0002"}"#
        );
        archive_snapshot(root.path(), lines.as_bytes(), "2026-07-31T00:00:00Z");
        let report = run_import(root.path(), false).expect("import");
        assert!(
            report.gap_unmapped_kind.values().sum::<u64>() > 0,
            "the fixture must actually produce a gap, or this test proves nothing ({report:?})"
        );
        assert_eq!(
            report.observations_unattributed, 0,
            "the prompt resolved, and the gap must not be counted ({report:?})"
        );
    }

    #[test]
    fn an_observation_whose_cwd_never_resolved_is_counted_rather_than_merely_absent() {
        // A cwd outside the ghq tree stays unresolved by design (§10 rule 4), so its
        // observations are missing from every per-repository total. That is only
        // honest if the run says how many there were.
        let root = TempRoot::new("unattributed");
        let outside = format!(
            "{}\n",
            prompt_line_at("u-0001", "2026-07-20T02:00:00.000Z", "/SYNTHETIC/work/repo")
        );
        archive_snapshot(root.path(), outside.as_bytes(), "2026-07-31T00:00:00Z");
        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report.observations_created.values().sum::<u64>(),
            report.observations_unattributed,
            "every observation from an unresolvable cwd must be counted ({report:?})"
        );
        assert!(report.observations_unattributed > 0, "{report:?}");

        // The same record under a resolvable cwd is attributed, so the counter is
        // measuring resolution rather than counting every observation there is.
        let inside_root = TempRoot::new("attributed");
        let inside = format!(
            "{}\n",
            prompt_line_at(
                "u-0001",
                "2026-07-20T02:00:00.000Z",
                &format!(
                    "{}/ghq/github.com/acme/api",
                    std::env::var("HOME").unwrap_or_default()
                ),
            )
        );
        archive_snapshot(
            inside_root.path(),
            inside.as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        let attributed = run_import(inside_root.path(), false).expect("import");
        assert!(
            attributed.observations_created.values().sum::<u64>() > 0,
            "{attributed:?}"
        );
        assert_eq!(
            attributed.observations_unattributed, 0,
            "a cwd inside the ghq tree must not be counted as unattributed ({attributed:?})"
        );
    }

    #[test]
    fn a_cwd_less_record_reaches_the_ledger_carrying_its_sessions_repository() {
        // The importer and the adapter agree on the Keystore kinds
        // `session_workspace` / `session_repository` and nothing else enforces it:
        // each side spells the literals out in its own unit tests, so a rename on
        // either side would leave every cwd-less record silently unattributed while
        // both crates stayed green. This is the only test that runs the composition.
        let root = TempRoot::new("session-backfill");
        let prompt = prompt_line_at(
            "u-0001",
            "2026-07-20T02:00:00.000Z",
            &ghq_cwd("github.com/acme/api"),
        );
        let cwd_less = json!({
            "type": "assistant",
            "uuid": "a-0001",
            "sessionId": TEST_SESSION,
            "timestamp": "2026-07-20T02:00:10.000Z",
            "message": {
                "content": [{ "type": "text", "text": "SYNTHETIC response" }],
                "usage": { "output_tokens": 3 },
            },
        })
        .to_string();
        archive_snapshot(
            root.path(),
            format!("{prompt}\n{cwd_less}\n").as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        let repositories = ledger.identities("repository").unwrap();
        assert_eq!(
            repositories.len(),
            1,
            "the fixture names exactly one repository ({report:?})"
        );
        let (expected_ref, display) = &repositories[0];
        assert_eq!(display, "github.com/acme/api");

        let (workspace, repository) = ledger
            .observation_identity(&dedupe_key(root.path(), "response.completed", "a-0001"))
            .unwrap()
            .expect("the cwd-less record must have produced a stored observation");
        assert_eq!(
            repository.as_deref(),
            Some(expected_ref.as_str()),
            "a record with no cwd of its own must inherit the repository its session \
             was mostly in ({report:?})"
        );
        assert_eq!(
            workspace.as_deref(),
            ledger
                .identities("workspace")
                .unwrap()
                .first()
                .map(|(r, _)| r.as_str()),
            "and its workspace likewise ({report:?})"
        );
        assert_eq!(
            report.observations_unattributed, 0,
            "so nothing in this fixture is left unattributed ({report:?})"
        );
    }

    #[test]
    fn the_registry_records_the_normalized_identity_and_never_a_cwd() {
        let mut scan = PreScan::default();
        scan.observe(
            &json!({
                "type": "user",
                "sessionId": "s1",
                "cwd": "/Users/dev/ghq/github.com/acme/api/.worktrees/issue-62"
            }),
            "/Users/dev",
        );
        let (_, identities) = scan.finish();
        let displays: Vec<&str> = identities.values().map(|(_, d)| d.as_str()).collect();
        assert!(displays.contains(&"github.com/acme/api"));
        assert!(displays.contains(&"github.com/acme/api@issue-62"));
        for d in displays {
            assert!(
                !d.contains("/Users/"),
                "a cwd contains the username; the ledger stays metadata-only: {d}"
            );
        }
    }

    // -- Codex ---------------------------------------------------------------------

    #[test]
    fn a_codex_locator_is_imported_alongside_claude_ones() {
        let root = TempRoot::new("codex-both-vendors");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        archive_snapshot_for(
            root.path(),
            CLAUDE_SOURCE_KIND,
            claude_bytes(&cwd),
            "2026-07-31T00:00:00Z",
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            codex_bytes(&cwd),
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");
        assert!(
            report
                .observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied()
                .unwrap_or(0)
                >= 2,
            "one prompt from each vendor must be imported ({report:?})"
        );
    }

    #[test]
    fn a_codex_prompt_is_attributed_through_its_session_meta_cwd() {
        // Codex prompt records carry no `cwd` at all -- it lives on `session_meta`. If the
        // pre-scan does not carry it across, every Codex observation is unattributed.
        let root = TempRoot::new("codex-workspace");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            codex_bytes(&cwd),
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");
        // Without this the assertion below is vacuous, and vacuous in the exact
        // direction that matters: a Codex prompt whose session never resolved
        // produces *no draft at all*, so `observations_unattributed` stays 0 while
        // the primary signal is missing entirely.
        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied(),
            Some(1),
            "the prompt must reach the ledger before its attribution can mean anything \
             ({report:?})"
        );
        assert_eq!(
            report.observations_unattributed, 0,
            "every Codex observation must inherit its session's workspace ({report:?})"
        );
    }

    #[test]
    fn a_re_announced_codex_prompt_is_ingested_once() {
        // The real corpus re-announces the same user_message within one file. End to end,
        // that must be one observation, not two.
        let root = TempRoot::new("codex-reannounce");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            codex_bytes_with_repeated_prompt(&cwd),
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied()
                .unwrap_or(0),
            1,
            "the re-announcement must collapse ({report:?})"
        );
        assert_eq!(report.observations_already_present, 1);
    }

    #[test]
    fn a_re_announced_session_meta_with_a_fresh_id_is_still_one_codex_session() {
        // The decision `codex_session_id` documents, pinned end to end.
        // `session-meta-variants.shape.json` advises keying identity off `id`; the
        // corpus survey that came later contradicts it. `session_meta` is re-announced
        // up to 30 times per file with 2--3 distinct `id` values and exactly one
        // `session_id`, so keying on `id` splits one session into three -- and with it
        // every per-session aggregate, permanently, since `ON CONFLICT DO NOTHING`
        // never rewrites a row.
        let root = TempRoot::new("codex-session-reannounce");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        let bytes = format!(
            "{}\n{}\n{}\n",
            codex_session_meta_with(
                "2026-07-20T02:00:00.000Z",
                &cwd,
                "aaaaaaaa-1111-4111-8111-111111111111",
                Some(CODEX_SESSION),
            ),
            codex_user_message("2026-07-20T02:00:05.000Z", "SYNTHETIC prompt"),
            // Re-announced with a fresh `id` -- and the same `session_id`, which is the
            // whole point.
            codex_session_meta_with(
                "2026-07-20T02:10:00.000Z",
                &cwd,
                "bbbbbbbb-2222-4222-8222-222222222222",
                Some(CODEX_SESSION),
            ),
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            bytes,
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.session.started.v1")
                .copied(),
            Some(1),
            "two announcements of one session must produce one session.started ({report:?})"
        );
        assert_eq!(
            report.observations_already_present, 1,
            "and the second must be accounted for as the duplicate it is ({report:?})"
        );
        assert_eq!(
            report.total_gaps(),
            0,
            "neither announcement may end up naming a session nothing registered \
             ({report:?})"
        );
        assert_eq!(report.observations_unattributed, 0, "{report:?}");
    }

    #[test]
    fn an_older_codex_session_meta_with_no_session_id_still_names_its_session() {
        // The other half of the same decision, and what
        // `session-meta-variants.shape.json` is genuinely warning about: 4 of the 328
        // real files predate `session_id` entirely. Falling back to `id` -- taken from
        // the first `session_meta` only -- is what keeps their prompts attributable
        // instead of gapped.
        let root = TempRoot::new("codex-session-old-cli");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        let bytes = format!(
            "{}\n{}\n",
            codex_session_meta_with(
                "2026-07-20T02:00:00.000Z",
                &cwd,
                "aaaaaaaa-1111-4111-8111-111111111111",
                None,
            ),
            codex_user_message("2026-07-20T02:00:05.000Z", "SYNTHETIC prompt"),
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            bytes,
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied(),
            Some(1),
            "a transcript from before `session_id` existed must still import its prompt \
             ({report:?})"
        );
        assert_eq!(
            report.total_gaps(),
            0,
            "and must not gap for want of a field that CLI version never wrote \
             ({report:?})"
        );
        assert_eq!(report.observations_unattributed, 0, "{report:?}");
    }

    #[test]
    fn a_codex_record_whose_file_never_named_a_session_is_gapped_not_silently_skipped() {
        // The failure mode the two tests above exist to prevent, seen from the other
        // side. A Codex prompt carries no session field of its own, so when the file
        // names none either the adapter can produce nothing at all -- and "a mapped
        // kind that produced nothing" is `records_skipped` by default, which would file
        // the loss of the primary signal under "legitimately carries no event".
        let root = TempRoot::new("codex-no-session-meta");
        let bytes = format!(
            "{}\n",
            codex_user_message("2026-07-20T02:00:05.000Z", "SYNTHETIC prompt")
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            bytes,
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        assert_eq!(
            report.gap_missing_field.get("session_id").copied(),
            Some(1),
            "an unplaceable human prompt must be diagnosed ({report:?})"
        );
        assert!(
            report.records_skipped.is_empty(),
            "and must not be filed as a legitimate non-event ({report:?})"
        );
    }

    #[test]
    fn a_codex_gap_marker_is_labelled_with_its_own_vendor_and_record_kind() {
        // Two ways the coverage report goes quietly wrong if the gap path is not
        // vendor-aware: every unmapped Codex kind lands in Claude Code's column, and
        // -- because a Codex kind is two `type` fields joined by a colon -- every one
        // of them collapses into the single `(unprintable)` bucket, which is what
        // `kind_detail` does to punctuation it does not expect.
        let root = TempRoot::new("codex-gap-label");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        let bytes = format!(
            "{}\n{}\n",
            codex_session_meta(&cwd),
            json!({
                "type": "event_msg",
                "timestamp": "2026-07-20T02:00:05.000Z",
                "payload": { "type": "token_count", "info": { "total_tokens": 1 } },
            }),
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            bytes,
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report
                .gap_unmapped_kind
                .get("event_msg:token_count")
                .copied(),
            Some(1),
            "the gap must name the Codex kind, not collapse into a catch-all ({report:?})"
        );

        // Read with SQL because `Ledger` exposes no per-source-kind observation query,
        // and adding one to the library for a single assertion would be the tail
        // wagging the dog.
        let db = rusqlite::Connection::open(root.path().join("ledger.db")).unwrap();
        let stored: String = db
            .query_row(
                "SELECT source_kind FROM observation WHERE event_type = 'dev.cclog.source.gap.v1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, "codex",
            "a gap from a Codex transcript must be attributed to Codex, not to the \
             vendor whose adapter constants the gap path happened to hardcode \
             ({report:?})"
        );
    }

    #[test]
    fn the_codex_pre_scan_pairs_a_tool_call_with_its_output_across_both_id_spellings() {
        // The same cross-crate contract the Claude Code version of this test pins, plus
        // one hazard that path does not have. A Codex *call* record carries both `id`
        // (`ctc_…`) and `call_id`; its *output* record carries only `call_id`, and the
        // adapter's lookup prefers `id`. So a pre-scan that registered the call under
        // `id` alone would leave the output resolving nothing: a null `duration_ms` on
        // every Codex tool call (the exact defect #13 fixed for Claude Code, where all
        // 49,508 historical durations had been a hardcoded 0), a `tool_family` of
        // "other", and a scope the started/finished pair cannot be joined on.
        let call = json!({
            "type": "response_item",
            "timestamp": "2026-07-20T02:00:12.000Z",
            "payload": {
                "type": "custom_tool_call",
                "id": "ctc_1",
                "call_id": "call_1",
                "name": "shell",
                "input": "SYNTHETIC",
            },
        });
        let output = json!({
            "type": "response_item",
            "timestamp": "2026-07-20T02:00:19.000Z",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "call_1",
                "output": "SYNTHETIC",
            },
        });

        let mut scan = CodexPreScan::default();
        scan.observe(&call, TEST_HOME);
        scan.observe(&output, TEST_HOME);
        let (keystore, _) = scan.finish(&std::collections::HashSet::new());

        let started = codex_history::transform(&call, &keystore);
        let finished = codex_history::transform(&output, &keystore);
        assert_eq!(started.len(), 1);
        assert_eq!(finished.len(), 1);
        assert_eq!(
            finished[0].data["duration_ms"],
            json!(7000),
            "the call record's timestamp must reach the output arm through the pre-scan, \
             so the emitted duration is measured rather than absent"
        );
        assert_eq!(
            finished[0].data["tool_family"],
            json!("shell"),
            "and the call's tool name likewise, or every finished Codex tool event \
             reports a family its own started event contradicts"
        );
        assert_eq!(
            started[0].subject, finished[0].subject,
            "the started/finished pair must land on one tool scope, or nothing can join \
             them back together"
        );
    }

    /// A prior version of this test hand-built a `Keystore` (`Keystore::new().map(...)`)
    /// and asserted directly on it. That cannot catch the bug this one exists to catch:
    /// the fact that a subagent's own thread id is *only* ever named in a different
    /// file's `sub_agent_activity` record, and [`Vendor::pre_scan`] builds and discards
    /// one snapshot's `Keystore` before the next snapshot is even read. A hand-built
    /// `Keystore` cannot observe whether the importer, reading two real archived files
    /// through the real per-locator loop, ever gets the fact from one into the other's.
    /// So this test archives two files and asks the same production functions
    /// [`run_import`] itself calls, fed nothing this test typed by hand -- only bytes
    /// read back out of the same `Ledger` `run_import` just wrote to.
    ///
    /// (`codex_history::transform_user_message` is not asserted on directly: making it
    /// *act* on this fact is a later task's job. What this task owns, and what this
    /// test pins, is that the fact reaches the child locator's own `Keystore` at all.)
    #[test]
    fn a_subagent_rollout_is_recognised_from_its_parents_file() {
        // parent.jsonl says it spawned a subagent whose thread is "thr-child".
        // child.jsonl IS that thread -- its own first `session_meta.id` is
        // "thr-child" -- and carries a prompt of its own. `session_id` deliberately
        // differs from `id` on both files, so a pre-scan that matched on the wrong
        // one of the two could not pass by accident (see `CodexPreScan::thread_id`).
        let cwd = "/SYNTHETIC/work/repo";
        let parent_bytes = format!(
            "{}\n{}\n",
            codex_session_meta_with(
                "2026-08-05T00:00:00.000Z",
                cwd,
                "thr-parent",
                Some("ses-parent"),
            ),
            codex_sub_agent_activity("2026-08-05T00:00:01.000Z", "thr-child", "reviewer"),
        );
        let child_bytes = format!(
            "{}\n{}\n",
            codex_session_meta_with(
                "2026-08-05T00:00:00.500Z",
                cwd,
                "thr-child",
                Some("ses-child"),
            ),
            codex_user_message("2026-08-05T00:00:02.000Z", "SYNTHETIC prompt"),
        );

        let root = TempRoot::new("codex-subagent-cross-file");
        archive_codex_snapshot(
            root.path(),
            "parent.jsonl",
            &parent_bytes,
            "2026-08-05T00:00:05Z",
        );
        archive_codex_snapshot(
            root.path(),
            "child.jsonl",
            &child_bytes,
            "2026-08-05T00:00:05Z",
        );
        // The real importer, end to end -- so a regression that breaks the pipeline
        // around the new pre-pass shows up here too, not only in the checks below.
        run_import(root.path(), false).expect("import");

        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        let latest_by_locator: BTreeMap<String, cclogger_archive::Snapshot> = ledger
            .find_snapshots(&SnapshotFilter {
                source_kind: Some(CODEX_SOURCE_KIND),
                ..Default::default()
            })
            .expect("find snapshots")
            .into_iter()
            .map(|s| (s.source_locator.clone(), s))
            .collect();

        let subagent_threads = codex_subagent_thread_ids(&ledger, &latest_by_locator);
        assert_eq!(
            subagent_threads,
            std::collections::HashSet::from(["thr-child".to_string()]),
            "the importer must learn from parent.jsonl a fact about child.jsonl, and \
             nothing else"
        );

        let child_snapshot = latest_by_locator
            .get("child.jsonl")
            .expect("child archived");
        let child_raw = ledger.read(&child_snapshot.object_id).expect("read child");
        let child_text = String::from_utf8_lossy(&child_raw);
        let child_lines = complete_lines(&child_text, &child_raw);
        let (child_keystore, _) =
            Vendor::Codex.pre_scan(&child_lines, TEST_HOME, &subagent_threads);
        assert!(
            child_keystore
                .resolve("codex_subagent_session", codex_history::FILE_SESSION)
                .is_some(),
            "child.jsonl's own pre-scan must recognise its thread was named by \
             parent.jsonl"
        );

        let parent_snapshot = latest_by_locator
            .get("parent.jsonl")
            .expect("parent archived");
        let parent_raw = ledger
            .read(&parent_snapshot.object_id)
            .expect("read parent");
        let parent_text = String::from_utf8_lossy(&parent_raw);
        let parent_lines = complete_lines(&parent_text, &parent_raw);
        let (parent_keystore, _) =
            Vendor::Codex.pre_scan(&parent_lines, TEST_HOME, &subagent_threads);
        assert!(
            parent_keystore
                .resolve("codex_subagent_session", codex_history::FILE_SESSION)
                .is_none(),
            "parent.jsonl named a subagent, but is not one itself"
        );
    }

    // -- copied history --------------------------------------------------------------
    //
    // The rule itself, one test per branch of `codex_inherited`, plus the two benign
    // shapes it must decline. `SYNTHETIC_CWD` is a fixed fake path rather than
    // `ghq_cwd`: none of these turn on workspace resolution, and a rule test that also
    // depended on `$HOME` would be measuring two things.

    const SYNTHETIC_CWD: &str = "/SYNTHETIC/work/repo";

    /// The lines of a rollout, borrowed the way [`Vendor::inherited_lines`] takes them.
    fn refs(lines: &[String]) -> Vec<&str> {
        lines.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_forked_rollouts_first_flush_is_inherited_when_it_embeds_a_foreign_session_meta() {
        // Rule 4, the branch that catches 78% of the corpus's Codex human prompts. The
        // fork copied the parent's history in one write, so the first three lines share
        // a millisecond and one of them is the *parent's* `session_meta` -- a thread id
        // that is not this file's, which is exactly what upstream's own reader keys off.
        let lines = codex_forked_rollout(
            SYNTHETIC_CWD,
            "2026-07-20T05:00:00.000Z",
            "2026-07-20T07:00:00.000Z",
        );
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Flush {
                through: 3,
                self_meta: 1
            },
            "the copy flush ends where the child's own first turn begins"
        );
        assert_eq!(
            Inherited::Flush {
                through: 3,
                self_meta: 1
            }
            .per_line(&refs(&lines)),
            vec![false, true, true, false],
            "the child's own live turn is not part of it -- and neither is its own \
             session_meta on line 1, which is when the fork happened"
        );
    }

    #[test]
    fn a_forked_childs_own_session_meta_is_live_because_that_is_when_the_fork_happened() {
        // The one timestamp in a copy flush that is real. The child's `session_meta` is
        // written *at the moment the fork happens*, so it is exactly when this session
        // began -- only the records copied in behind it stand in for event times that
        // are no longer recoverable. Marking it too would throw away the child's
        // `session.started` and its day's claim to having been worked at all, which is
        // a second data-correctness defect introduced while fixing the first.
        let root = TempRoot::new("codex-fork-own-meta-live");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        let bytes = format!(
            "{}\n",
            codex_forked_rollout(&cwd, "2026-07-20T05:00:00.000Z", "2026-07-20T07:00:00.000Z")
                .join("\n")
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            bytes,
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        let db = rusqlite::Connection::open(root.path().join("ledger.db")).unwrap();
        let basis: Option<String> = db
            .query_row(
                "SELECT json_extract(body, '$.data.time_basis') FROM observation
                 WHERE event_type = 'dev.cclog.session.started.v1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            basis, None,
            "the fork's own session start says nothing about its time basis, because \
             its timestamp needs no qualifying ({report:?})"
        );
        assert_eq!(
            report.observations_inherited, 2,
            "only the embedded parent's session_meta and the prompt behind it were \
             copied ({report:?})"
        );
    }

    #[test]
    fn a_deferred_first_flush_is_kept_because_no_foreign_session_meta_is_in_it() {
        // The benign look-alike, and the one a careless implementation destroys. Codex
        // does not create the rollout file until the first user message, so an ordinary
        // session's `session_meta`, `turn_context` and first prompt are written in one
        // flush and share one millisecond -- the same *timestamp* shape as a fork, with
        // none of the copying. Nothing here names a parent either, so rule 3 stops
        // before rule 4 is even asked.
        let lines = codex_deferred_first_flush(SYNTHETIC_CWD, "2026-07-20T05:00:00.000Z");
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Nothing,
            "three records written in one millisecond are not evidence of a copy"
        );
    }

    #[test]
    fn a_deferred_first_flush_that_names_a_parent_is_still_kept() {
        // The harder half of the same case, and the one rule 3 cannot reach: a subagent
        // whose history was never persisted into its own file still carries
        // `parent_thread_id`, and still opens with a one-millisecond flush. It is
        // exactly the shape a timestamp-only heuristic gets wrong -- 6 real files, which
        // it flagged and this rule declines. What it lacks is a foreign `session_meta`
        // inside that opening run, and that is the whole of the difference.
        let lines = codex_parented_deferred_first_flush(SYNTHETIC_CWD, "2026-07-20T05:00:00.000Z");
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Nothing,
            "naming a parent is not the same as having copied one's records"
        );
    }

    #[test]
    fn a_rollout_that_names_no_parent_at_all_is_all_live() {
        // Rule 3. Every file written before `forked_from_id` / `parent_thread_id`
        // existed takes this branch, and so does every ordinary session written since.
        let lines = vec![
            codex_session_meta_with(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
            ),
            // A second `session_meta` with a *different* id -- the re-announcement the
            // corpus shows up to 30 times per file. Without rule 3 stopping first, rule
            // 4 would read this as an embedded parent.
            codex_session_meta_with(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_PARENT_SESSION,
                Some(CODEX_CHILD_SESSION),
            ),
            codex_user_message("2026-07-20T05:00:00.000Z", "SYNTHETIC prompt"),
        ];
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Nothing,
            "a re-announced session_meta is not an embedded parent"
        );
    }

    #[test]
    fn a_rollout_carrying_history_base_is_all_live_even_though_it_names_a_parent() {
        // Rule 1. `history_base` means the parent's prefix is *pointed at* -- a
        // `ForkPersistence::Referenced` fork -- so no record was re-written and no
        // timestamp was overwritten, however forked the thread is.
        let lines = vec![
            codex_session_meta_extra(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
                json!({
                    "forked_from_id": CODEX_PARENT_SESSION,
                    "history_base": { "thread_id": CODEX_PARENT_SESSION, "end_ordinal_exclusive": 12 },
                }),
            ),
            // A foreign `session_meta` in the opening flush, which rule 4 would seize
            // on. Rule 1 has to win, or a referenced fork is mismarked as a copied one.
            codex_session_meta_with(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_PARENT_SESSION,
                Some(CODEX_PARENT_SESSION),
            ),
            codex_user_message("2026-07-20T05:00:00.000Z", "SYNTHETIC prompt"),
        ];
        assert_eq!(codex_inherited(&refs(&lines)), Inherited::Nothing);
    }

    #[test]
    fn a_subagent_history_start_ordinal_splits_the_file_at_exactly_that_ordinal() {
        // Rule 2, and the only exact one: Codex states the boundary itself. No file in
        // the surveyed corpus carries an `ordinal` -- the field ships from 0.145.0 --
        // so this branch is written for data that has not arrived, which is why it is
        // pinned rather than left to be discovered.
        let with_ordinal = |ordinal: u64, line: String| {
            let mut record: Value = serde_json::from_str(&line).expect("a record");
            record["ordinal"] = json!(ordinal);
            record.to_string()
        };
        let lines = vec![
            with_ordinal(
                0,
                codex_session_meta_extra(
                    "2026-07-20T05:00:00.000Z",
                    SYNTHETIC_CWD,
                    CODEX_CHILD_SESSION,
                    Some(CODEX_CHILD_SESSION),
                    json!({
                        "parent_thread_id": CODEX_PARENT_SESSION,
                        "subagent_history_start_ordinal": 2,
                    }),
                ),
            ),
            with_ordinal(
                1,
                codex_user_message("2026-07-20T05:00:00.000Z", "SYNTHETIC inherited"),
            ),
            with_ordinal(
                2,
                codex_user_message("2026-07-20T05:00:00.000Z", "SYNTHETIC live"),
            ),
            // Same millisecond as everything above, so a rule that fell through to the
            // timestamp run would swallow it. The stated ordinal says otherwise.
            with_ordinal(
                3,
                codex_user_message("2026-07-20T05:00:00.000Z", "SYNTHETIC also live"),
            ),
        ];
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::BelowOrdinal(2),
            "the boundary is the ordinal Codex stated, not one derived from timestamps"
        );
        assert_eq!(
            Inherited::BelowOrdinal(2).per_line(&refs(&lines)),
            vec![true, true, false, false],
            "`< S` is inherited and `>= S` is live, exactly"
        );
    }

    #[test]
    fn a_line_with_no_ordinal_is_not_placed_below_one() {
        // The absence this rule refuses to fill in. A line carrying no `ordinal` has no
        // position on that axis, and taking its line number for one would be a value
        // the file never wrote.
        let lines = vec![
            codex_user_message("2026-07-20T05:00:00.000Z", "SYNTHETIC no ordinal"),
            codex_user_message("2026-07-20T05:00:01.000Z", "SYNTHETIC also none"),
        ];
        assert_eq!(
            Inherited::BelowOrdinal(99).per_line(&refs(&lines)),
            vec![false, false]
        );
    }

    #[test]
    fn the_copied_run_ends_at_the_first_line_more_than_a_second_from_the_opening_write() {
        // What bounds `B`. A flush stamps its whole batch with one millisecond; the
        // tolerance covers a batch straddling a tick, and must not reach the next thing
        // the session did. The third line here is 1.001s out and is live; the second, at
        // exactly the tolerance, is not.
        let lines = vec![
            codex_session_meta_extra(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
                json!({ "forked_from_id": CODEX_PARENT_SESSION }),
            ),
            codex_session_meta_with(
                "2026-07-20T05:00:01.000Z",
                SYNTHETIC_CWD,
                CODEX_PARENT_SESSION,
                Some(CODEX_PARENT_SESSION),
            ),
            codex_user_message("2026-07-20T05:00:01.001Z", "SYNTHETIC live"),
        ];
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Flush {
                through: 2,
                self_meta: 1
            },
            "1.000s in, 1.001s out"
        );
    }

    #[test]
    fn the_copied_run_is_the_opening_one_and_does_not_resume_after_a_line_outside_it() {
        // `B` is a *contiguous* run from line 1, not the set of lines that happen to
        // share the opening millisecond. The two come apart the moment a timestamp goes
        // backwards -- a clock adjustment, or records written out of order -- and the
        // difference is that a set would reach past the live turn on line 3 and mark it.
        // Rollout timestamps are write times and normally only increase, which is
        // exactly why this would go unnoticed until it did not.
        let lines = vec![
            codex_session_meta_extra(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
                json!({ "forked_from_id": CODEX_PARENT_SESSION }),
            ),
            codex_session_meta_with(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_PARENT_SESSION,
                Some(CODEX_PARENT_SESSION),
            ),
            codex_user_message("2026-07-20T05:10:00.000Z", "SYNTHETIC live"),
            codex_user_message("2026-07-20T05:00:00.500Z", "SYNTHETIC out of order"),
        ];
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Flush {
                through: 2,
                self_meta: 1
            },
            "the run ends at line 3 and does not pick line 4 back up"
        );
        assert_eq!(
            Inherited::Flush {
                through: 2,
                self_meta: 1
            }
            .per_line(&refs(&lines)),
            vec![false, true, false, false]
        );
    }

    #[test]
    fn the_copied_run_spares_the_files_own_session_meta_wherever_it_sits() {
        // The exception follows the *reason* -- this thread's own metadata, written when
        // the fork happened -- rather than the position. In every shape Codex writes the
        // two coincide, because `session_meta` is the file's first line. Pinned against a
        // file where they do not, so a later tidy-up that hardcodes line 1 keeps a copied
        // line live instead, which is this change's own defect pointed backwards.
        let lines = vec![
            // Something ahead of the meta. Not a shape Codex produces today; the point
            // is that the rule does not depend on it never producing one.
            codex_user_message("2026-07-20T05:00:00.000Z", "SYNTHETIC copied"),
            codex_session_meta_extra(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
                json!({ "forked_from_id": CODEX_PARENT_SESSION }),
            ),
            codex_session_meta_with(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_PARENT_SESSION,
                Some(CODEX_PARENT_SESSION),
            ),
            codex_user_message("2026-07-20T07:00:00.000Z", "SYNTHETIC live"),
        ];
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Flush {
                through: 3,
                self_meta: 2
            }
        );
        assert_eq!(
            Inherited::Flush {
                through: 3,
                self_meta: 2
            }
            .per_line(&refs(&lines)),
            vec![true, false, true, false],
            "the line ahead of the meta is copied like any other, and only the meta \
             itself is spared"
        );
    }

    #[test]
    fn a_forked_rollout_whose_own_session_meta_names_no_id_is_left_live() {
        // Upstream's rule is "`session_meta` lines that don't match the canonical thread
        // ID". A file whose first `session_meta` carries no `id` has no canonical thread
        // id to compare against, so the discriminator is unavailable -- and marking live
        // records on a guess is the failure this whole change exists to stop, pointed the
        // other way.
        let mut first: Value = serde_json::from_str(&codex_session_meta_extra(
            "2026-07-20T05:00:00.000Z",
            SYNTHETIC_CWD,
            CODEX_CHILD_SESSION,
            Some(CODEX_CHILD_SESSION),
            json!({ "forked_from_id": CODEX_PARENT_SESSION }),
        ))
        .expect("a record");
        first["payload"]
            .as_object_mut()
            .expect("a payload")
            .remove("id");
        let lines = vec![
            first.to_string(),
            codex_session_meta_with(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_PARENT_SESSION,
                Some(CODEX_PARENT_SESSION),
            ),
        ];
        assert_eq!(codex_inherited(&refs(&lines)), Inherited::Nothing);
    }

    #[test]
    fn a_rollout_with_no_session_meta_at_all_is_all_live() {
        let lines = vec![codex_user_message(
            "2026-07-20T05:00:00.000Z",
            "SYNTHETIC prompt",
        )];
        assert_eq!(codex_inherited(&refs(&lines)), Inherited::Nothing);
        assert_eq!(codex_inherited(&[]), Inherited::Nothing);
    }

    #[test]
    fn a_claude_code_snapshot_has_no_copied_lines() {
        // Claude Code duplicates records into a resumed session's new file too, but with
        // their *original* timestamps -- which is why the ledger's dedupe collapses a
        // resume copy onto the row already there. A copy that kept the original time is
        // not a wrong time, so nothing on this vendor's path may be marked.
        //
        // What this pins is that answer, not the code path taken to it: routing Claude
        // Code through `codex_inherited` is observationally equivalent, because Claude
        // Code writes no `session_meta` at all and the rule stops on the first branch.
        // Mutation-checked -- an arm that marked these lines fails here and in 41 other
        // tests. The two lines below share a timestamp on purpose: that is the shape the
        // Codex rule looks at, so a vendor mix-up would have something to bite on.
        let lines = vec![
            prompt_line("u-0001", "2026-07-20T05:00:00.000Z"),
            prompt_line("u-0002", "2026-07-20T05:00:00.000Z"),
        ];
        assert_eq!(
            Vendor::ClaudeCode.inherited_lines(&refs(&lines)),
            vec![false, false]
        );
        assert_eq!(
            codex_inherited(&refs(&lines)),
            Inherited::Nothing,
            "and the Codex rule would say the same of them, for want of a session_meta"
        );
    }

    #[test]
    fn a_forked_codex_rollouts_copied_records_are_marked_rather_than_dropped() {
        // End to end. The copied prompt must reach the ledger -- every one in the
        // surveyed corpus has a live original in the parent's file, but that is a
        // property of that corpus and not a guarantee -- and it must carry a
        // `time_basis` that keeps its timestamp off every clock.
        let root = TempRoot::new("codex-inherited");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        let bytes = format!(
            "{}\n",
            codex_forked_rollout(&cwd, "2026-07-20T05:00:00.000Z", "2026-07-20T07:00:00.000Z")
                .join("\n")
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            bytes,
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied(),
            Some(2),
            "both prompts are imported -- the copy is marked, never dropped ({report:?})"
        );
        assert_eq!(
            report.observations_inherited, 2,
            "the embedded parent's session_meta (a gap, for want of a session this file \
             registers) and the copied prompt behind it -- but not the child's own \
             session_meta, which is when the fork happened ({report:?})"
        );

        let db = rusqlite::Connection::open(root.path().join("ledger.db")).unwrap();
        let mut stmt = db
            .prepare(
                "SELECT occurred_at, json_extract(body, '$.data.time_basis')
                 FROM observation
                 WHERE event_type = 'dev.cclog.prompt.submitted.v1'
                 ORDER BY occurred_at ASC",
            )
            .unwrap();
        let prompts: Vec<(String, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            prompts,
            vec![
                (
                    "2026-07-20T05:00:00.000Z".to_string(),
                    Some("copied_at".to_string())
                ),
                ("2026-07-20T07:00:00.000Z".to_string(), None),
            ],
            "the copy says which clock its time came from; the live turn has nothing to \
             qualify and says nothing"
        );
    }

    #[test]
    fn a_benign_deferred_first_flush_leaves_its_prompt_unmarked() {
        // The other half of the test above, and the half without which it is worth
        // nothing: an implementation that marked every one-millisecond opening run would
        // pass that test and quietly take the first prompt of every ordinary Codex
        // session off the clock.
        let root = TempRoot::new("codex-deferred-flush");
        let home = std::env::var("HOME").expect("HOME");
        let cwd = format!("{home}/ghq/github.com/acme/api");
        let bytes = format!(
            "{}\n",
            codex_deferred_first_flush(&cwd, "2026-07-20T05:00:00.000Z").join("\n")
        );
        archive_snapshot_for(
            root.path(),
            CODEX_SOURCE_KIND,
            bytes,
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");

        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.prompt.submitted.v1")
                .copied(),
            Some(1),
            "the session's first prompt is still imported ({report:?})"
        );
        assert_eq!(
            report.observations_inherited, 0,
            "and nothing in an ordinary session was copied ({report:?})"
        );

        let db = rusqlite::Connection::open(root.path().join("ledger.db")).unwrap();
        let basis: Option<String> = db
            .query_row(
                "SELECT json_extract(body, '$.data.time_basis') FROM observation
                 WHERE event_type = 'dev.cclog.prompt.submitted.v1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            basis, None,
            "its timestamp is when it happened, and nothing may say otherwise"
        );
    }

    /// Golden test for `adapters/codex/fixtures/inherited-history.fixture.json`.
    ///
    /// The same job `import_gap_fixture_matches_what_the_importer_produces_for_each_gap_reason`
    /// does for gap markers, for the other event class only the importer can produce: a
    /// copied record. `tools/conformance` validates the fixture's `expected` entries
    /// against the canonical schema and the leak scan -- which is the only thing that
    /// checks the new `copied_at` member of `time_basis` against the schema at all --
    /// and this asserts the importer still produces exactly them.
    #[test]
    fn inherited_history_fixture_matches_what_the_importer_produces_for_a_forked_rollout() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../adapters/codex/fixtures/inherited-history.fixture.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let fx: Value = serde_json::from_str(&raw).expect("parse fixture json");

        let source = &fx["source"];
        let digest = source["snapshot_digest"].as_str().expect("snapshot_digest");
        let acquired_at = source["acquired_at"].as_str().expect("acquired_at");
        let device = source["device"].as_str().expect("device");
        let home = source["home"].as_str().expect("home");
        let lines: Vec<&str> = source["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| line.as_str().expect("line"))
            .collect();
        let expected = fx["expected"].as_array().expect("expected");
        assert_eq!(
            lines.len(),
            expected.len(),
            "every line in this fixture must produce exactly one observation"
        );

        // The same two whole-file passes `run_import` makes, in the same order. This
        // fixture is one file with no subagent lineage, so an empty set is correct,
        // not merely convenient.
        let (keystore, _) = Vendor::Codex.pre_scan(&lines, home, &std::collections::HashSet::new());
        let inherited = Vendor::Codex.inherited_lines(&lines);
        assert_eq!(
            inherited,
            vec![false, true, true, false],
            "the fixture's point is the boundary: the child's own session_meta live on \
             line 1, a copied middle, and the live turn after it. If that moved, the \
             expectations below are pinning something else"
        );

        for (i, (line, want)) in lines.iter().zip(expected).enumerate() {
            let line_no = i + 1;
            let outcome = classify_line(
                Vendor::Codex,
                line,
                &keystore,
                digest,
                acquired_at,
                line_no,
                inherited[i],
            );
            let draft = match outcome {
                LineOutcome::Drafts { mut drafts, .. } => {
                    assert_eq!(drafts.len(), 1, "line {line_no} must produce one draft");
                    drafts.remove(0)
                }
                LineOutcome::Gap { draft, .. } => *draft,
                LineOutcome::Skipped { kind } => {
                    panic!("line {line_no} must not be skipped, got kind {kind}")
                }
            };
            let got = finalize_with_id(
                draft,
                want["id"].as_str().expect("id").to_string(),
                &format!("line:{line_no}"),
                device,
                want["cclogobservedat"].as_str().expect("cclogobservedat"),
            );
            assert_eq!(
                &serde_json::to_value(&got).unwrap(),
                want,
                "inherited-history.fixture.json[{i}]: canonical observation mismatch"
            );
        }
    }

    #[test]
    fn every_codex_draft_carries_an_object_data_so_the_copied_mark_lands() {
        // `mark_inherited` writes into `data`, and a draft whose `data` were not a JSON
        // object would be silently left unmarked -- which is a wrong timestamp on a
        // clock, the exact failure this change exists to fix. The canonical schema
        // requires an object; this pins that the adapter actually emits one, for every
        // kind it maps.
        let ks = Keystore::new()
            .map("session", codex_history::FILE_SESSION, "ses_TEST")
            .map("session", CODEX_CHILD_SESSION, "ses_TEST");
        let records = [
            codex_session_meta_with(
                "2026-07-20T05:00:00.000Z",
                SYNTHETIC_CWD,
                CODEX_CHILD_SESSION,
                Some(CODEX_CHILD_SESSION),
            ),
            codex_user_message("2026-07-20T05:00:01.000Z", "SYNTHETIC prompt"),
            codex_agent_message("2026-07-20T05:00:02.000Z", "SYNTHETIC response"),
            json!({
                "type": "response_item",
                "timestamp": "2026-07-20T05:00:03.000Z",
                "payload": { "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "shell" },
            })
            .to_string(),
            json!({
                "type": "response_item",
                "timestamp": "2026-07-20T05:00:04.000Z",
                "payload": { "type": "custom_tool_call_output", "call_id": "call_1", "output": "SYNTHETIC" },
            })
            .to_string(),
            json!({
                "type": "response_item",
                "timestamp": "2026-07-20T05:00:05.000Z",
                "payload": { "type": "function_call", "id": "fc_1", "call_id": "call_2", "name": "apply_patch" },
            })
            .to_string(),
            json!({
                "type": "response_item",
                "timestamp": "2026-07-20T05:00:06.000Z",
                "payload": { "type": "function_call_output", "call_id": "call_2", "output": "SYNTHETIC" },
            })
            .to_string(),
        ];
        let mut seen = 0;
        for line in &records {
            let record: Value = serde_json::from_str(line).expect("a record");
            let mut drafts = codex_history::transform(&record, &ks);
            assert!(
                !drafts.is_empty(),
                "this fixture must exercise a mapped kind: {line}"
            );
            mark_inherited(&mut drafts);
            for draft in &drafts {
                assert_eq!(
                    draft.data["time_basis"],
                    json!("copied_at"),
                    "the mark must land on every draft: {line}"
                );
                seen += 1;
            }
        }
        assert_eq!(
            seen,
            codex_history::MAPPED_KINDS.len(),
            "one draft per mapped Codex kind, or a kind has grown a shape this test does \
             not cover"
        );
    }

    // -- the hook spool as a third import source ---------------------------------
    //
    // These drive the *real* receiver (`crate::hook::run_hook`) to write the spool,
    // then the real `run_import` to drain it, so what is asserted is the whole path
    // from a hook payload to a ledger row. No vendor directory is read; the spool is
    // written into the same throwaway `TempRoot` the ledger lives in.

    /// The synthetic cwd hook lines name -- inside the ghq tree, so it resolves.
    fn hook_cwd() -> String {
        ghq_cwd("github.com/acme/api")
    }

    /// Feed one hook payload through the receiver, at a stated arrival time.
    fn spool(root: &Path, received_at: &str, payload: Value) -> u8 {
        crate::hook::run_hook(root, payload.to_string().as_bytes(), received_at)
    }

    /// A `UserPromptSubmit` payload carrying the prompt the human typed. Every value
    /// is synthetic; `SYNTHETICSECRET` is the marker the leak assertions look for.
    fn hook_prompt(prompt_id: &str) -> Value {
        json!({
            "session_id": HOOK_SESSION,
            "prompt_id": prompt_id,
            "cwd": hook_cwd(),
            "hook_event_name": "UserPromptSubmit",
            "transcript_path": "/SYNTHETIC/home/.claude/projects/p/s.jsonl",
            "prompt_text": "SYNTHETICSECRET the human typed this",
        })
    }

    fn hook_tool(event: &str, tool_use_id: &str, extra: Value) -> Value {
        let mut record = json!({
            "session_id": HOOK_SESSION,
            "prompt_id": "prm-1",
            "cwd": hook_cwd(),
            "hook_event_name": event,
            "tool_name": "Bash",
            "tool_use_id": tool_use_id,
            "tool_input": { "command": "echo SYNTHETICSECRET" },
            "tool_response": "SYNTHETICSECRET",
        });
        for (k, v) in extra.as_object().expect("extra is an object") {
            record[k] = v.clone();
        }
        record
    }

    /// Deliberately not [`TEST_SESSION`], so a spool line cannot be mistaken for a
    /// transcript record's session by accident.
    const HOOK_SESSION: &str = "33333333-3333-4333-8333-333333333333";

    /// Every observation body in the ledger, as stored. Read straight out of SQLite
    /// because the point is what was *written*, not what a typed read hands back.
    fn observation_bodies(root: &Path) -> Vec<Value> {
        let db = rusqlite::Connection::open(root.join("ledger.db")).expect("open ledger.db");
        let mut stmt = db.prepare("SELECT body FROM observation").expect("prepare");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.map(|r| serde_json::from_str(&r.expect("row")).expect("body is json"))
            .collect()
    }

    fn bodies_of_type(root: &Path, event_type: &str) -> Vec<Value> {
        observation_bodies(root)
            .into_iter()
            .filter(|b| b["type"] == event_type)
            .collect()
    }

    #[test]
    fn the_spool_is_drained_exactly_once_however_many_times_import_runs() {
        // Two mechanisms have to hold for this, and the assertions separate them: the
        // checkpoint cursor means the second run does not re-read the lines, and the
        // dedupe key means that if it ever did, the rows would collapse rather than
        // double. Asserting only the row count would pass with the cursor removed.
        let root = TempRoot::new("spool-once");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        spool(
            root.path(),
            "2026-08-04T01:00:02.000Z",
            hook_tool("PreToolUse", "toolu-1", json!({})),
        );
        spool(
            root.path(),
            "2026-08-04T01:00:03.500Z",
            hook_tool("PostToolUse", "toolu-1", json!({ "duration_ms": 1500 })),
        );

        let first = run_import(root.path(), false).expect("first import");
        assert_eq!(
            first.spool_lines_drained, 3,
            "every spool line must be read once: {first:?}"
        );
        assert_eq!(
            first.observations_created.values().sum::<u64>(),
            3,
            "{first:?}"
        );
        assert_eq!(first.observations_already_present, 0, "{first:?}");

        let second = run_import(root.path(), false).expect("second import");
        assert_eq!(
            second.spool_lines_drained, 0,
            "the cursor must stop the second run from re-reading a line it already \
             accounted for -- dedupe collapsing the rows afterwards is a second net, \
             not the first: {second:?}"
        );
        assert_eq!(
            second.observations_created.values().sum::<u64>(),
            0,
            "{second:?}"
        );
        assert_eq!(
            second.observations_already_present, 0,
            "nothing was re-transformed, so nothing was re-offered either: {second:?}"
        );

        let ledger = Ledger::open(root.path()).expect("reopen ledger");
        assert_eq!(
            ledger.observation_count(None).unwrap(),
            3,
            "three hook events, three rows, after two runs"
        );
    }

    #[test]
    fn a_spool_that_grew_between_runs_contributes_only_its_new_lines() {
        let root = TempRoot::new("spool-grew");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        let first = run_import(root.path(), false).expect("first import");
        assert_eq!(first.spool_lines_drained, 1, "{first:?}");

        spool(
            root.path(),
            "2026-08-04T01:05:00.000Z",
            hook_prompt("prm-2"),
        );
        let second = run_import(root.path(), false).expect("second import");
        assert_eq!(
            second.spool_lines_drained, 1,
            "only the appended line is new: {second:?}"
        );
        assert_eq!(second.checkpoints_reset, 0, "append is not a rewrite");
        assert_eq!(
            Ledger::open(root.path())
                .unwrap()
                .observation_count(Some("dev.cclog.prompt.submitted.v1"))
                .unwrap(),
            2
        );
    }

    #[test]
    fn prompt_text_never_reaches_an_observation() {
        // The ledger is metadata-only and `UserPromptSubmit` carries the
        // prompt itself. This walks the whole path -- payload, receiver, spool, import,
        // ledger row -- and asserts the text is in none of it, including the raw
        // snapshot bytes the archive keeps of the spool.
        let root = TempRoot::new("spool-no-prompt");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        spool(
            root.path(),
            "2026-08-04T01:00:02.000Z",
            hook_tool("PostToolUse", "toolu-1", json!({ "duration_ms": 12 })),
        );
        run_import(root.path(), false).expect("import");

        let bodies = observation_bodies(root.path());
        assert_eq!(bodies.len(), 2, "both events were imported: {bodies:?}");
        for body in &bodies {
            let text = body.to_string();
            assert!(
                !text.contains("SYNTHETICSECRET"),
                "content reached an observation: {text}"
            );
        }

        // And not in the archive either: `ingest` publishes the spool's own bytes as a
        // snapshot, so if the receiver had spooled the prompt it would be sitting in
        // the object store even with a clean ledger row.
        let archive = root.path().join("archive");
        let mut found = 0;
        let mut stack = vec![archive];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = std::fs::read(&path).expect("read object");
                    let text = String::from_utf8_lossy(&bytes);
                    assert!(
                        !text.contains("SYNTHETICSECRET"),
                        "content reached the archived spool snapshot: {text}"
                    );
                    found += 1;
                }
            }
        }
        assert!(
            found > 0,
            "the spool snapshot must actually be in the store"
        );
    }

    #[test]
    fn a_missing_duration_reaches_the_ledger_as_null_rather_than_zero() {
        // `0` is a real measurement of a tool that returned within a millisecond, and
        // the transcript channel writes genuine nulls into this same column, so the two
        // must stay distinguishable all the way into the row.
        let root = TempRoot::new("spool-duration");
        spool(
            root.path(),
            "2026-08-04T01:00:01.000Z",
            hook_tool("PostToolUse", "toolu-none", json!({})),
        );
        spool(
            root.path(),
            "2026-08-04T01:00:02.000Z",
            hook_tool("PostToolUse", "toolu-zero", json!({ "duration_ms": 0 })),
        );
        spool(
            root.path(),
            "2026-08-04T01:00:03.000Z",
            hook_tool("PostToolUse", "toolu-real", json!({ "duration_ms": 842 })),
        );
        run_import(root.path(), false).expect("import");

        let mut durations: Vec<Value> = bodies_of_type(root.path(), "dev.cclog.tool.finished.v1")
            .into_iter()
            .map(|b| b["data"]["duration_ms"].clone())
            .collect();
        durations.sort_by_key(|v| v.to_string());
        assert_eq!(
            durations,
            vec![json!(0), json!(842), json!(null)],
            "an unmeasured duration must be null, a measured zero must be 0, and a real \
             one must survive"
        );
    }

    #[test]
    fn a_spool_line_the_receiver_could_not_route_becomes_a_diagnosed_gap() {
        // `cclogger hook` exits 0 on every internal error so it can never fail a
        // session. This is where that silence stops: the receiver's own reason arrives
        // as a `source.gap` marker with the reason on it, not as a line nobody reads.
        let root = TempRoot::new("spool-receiver-error");
        assert_eq!(
            crate::hook::run_hook(
                root.path(),
                b"not json at all".as_slice(),
                "2026-08-04T01:00:00.000Z"
            ),
            0
        );
        spool(
            root.path(),
            "2026-08-04T01:00:05.000Z",
            hook_prompt("prm-1"),
        );

        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report.gap_receiver_error.get("payload_not_an_object"),
            Some(&1),
            "the receiver's swallowed failure must be counted under its own reason, not \
             as an unmapped kind: {report:?}"
        );
        assert_eq!(report.total_gaps(), 1, "{report:?}");

        let gaps = bodies_of_type(root.path(), "dev.cclog.source.gap.v1");
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0]["data"]["reason"], "payload_not_an_object");
        assert_eq!(
            gaps[0]["data"]["time_basis"], "received_at",
            "the marker is dated to when the receiver ran, and says so"
        );
        assert_eq!(
            gaps[0]["cclogintegritystate"], "gap",
            "so every clock excludes it"
        );
        // The event that did work is unaffected.
        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.prompt.submitted.v1"),
            Some(&1),
            "{report:?}"
        );
    }

    #[test]
    fn a_spool_line_naming_an_unmapped_hook_event_is_gapped_rather_than_dropped() {
        let root = TempRoot::new("spool-unmapped");
        let mut record = hook_prompt("prm-1");
        record["hook_event_name"] = json!("SubagentStop");
        spool(root.path(), "2026-08-04T01:00:00.000Z", record);

        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report.gap_unmapped_kind.get("SubagentStop"),
            Some(&1),
            "a hook event this build does not map must be diagnosed by name: {report:?}"
        );
    }

    #[test]
    fn a_mapped_hook_event_missing_a_field_it_needs_is_gapped_by_field_name() {
        let root = TempRoot::new("spool-missing-field");
        let mut record = hook_prompt("prm-1");
        record.as_object_mut().unwrap().remove("prompt_id");
        spool(root.path(), "2026-08-04T01:00:00.000Z", record);

        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report.gap_missing_field.get("prompt_id"),
            Some(&1),
            "a prompt with nothing to correlate it to is a diagnosed loss, not a skip: \
             {report:?}"
        );
    }

    #[test]
    fn a_hook_observation_is_attributed_to_the_repository_its_cwd_names() {
        let root = TempRoot::new("spool-attribution");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report.observations_unattributed, 0,
            "a cwd inside the ghq tree resolves: {report:?}"
        );

        let bodies = bodies_of_type(root.path(), "dev.cclog.prompt.submitted.v1");
        let repository = bodies[0]["cclogrepositoryref"]
            .as_str()
            .expect("a repository ref");
        let ledger = Ledger::open(root.path()).expect("reopen");
        assert_eq!(
            ledger.identity_display(repository).unwrap().as_deref(),
            Some("github.com/acme/api"),
            "and the registry names it, without ever storing the cwd"
        );
    }

    #[test]
    fn a_hook_observation_says_its_time_is_an_arrival_stamp() {
        let root = TempRoot::new("spool-basis");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        run_import(root.path(), false).expect("import");
        let bodies = bodies_of_type(root.path(), "dev.cclog.prompt.submitted.v1");
        assert_eq!(bodies[0]["time"], "2026-08-04T01:00:00.000Z");
        assert_eq!(
            bodies[0]["data"]["time_basis"], "received_at",
            "no hook event carries a timestamp, and the row has to say whose clock this is"
        );
        assert_eq!(
            bodies[0]["cclogsourceversion"], "claude-code-hook/1",
            "and which channel it arrived through"
        );
    }

    #[test]
    fn the_import_report_says_where_hook_capture_begins_rather_than_implying_it_covers_all() {
        // A spool that starts mid-history is the normal case: hooks record from the
        // moment they are installed and Claude Code replays nothing. The first line's
        // arrival stamp is the only thing that says so.
        let root = TempRoot::new("spool-begins");
        spool(
            root.path(),
            "2026-08-04T09:15:00.000Z",
            hook_prompt("prm-1"),
        );
        spool(
            root.path(),
            "2026-08-04T09:16:00.000Z",
            hook_prompt("prm-2"),
        );
        let first = run_import(root.path(), false).expect("import");
        assert_eq!(
            first.spool_begins_at.as_deref(),
            Some("2026-08-04T09:15:00.000Z"),
            "{first:?}"
        );

        // And it keeps saying so on a run that drains nothing -- the question is how
        // far back capture reaches, not what this run happened to do.
        let second = run_import(root.path(), false).expect("second import");
        assert_eq!(second.spool_lines_drained, 0, "{second:?}");
        assert_eq!(
            second.spool_begins_at.as_deref(),
            Some("2026-08-04T09:15:00.000Z"),
            "{second:?}"
        );
    }

    #[test]
    fn a_root_with_no_spool_reports_nothing_about_one() {
        let root = TempRoot::new("spool-absent");
        archive_snapshot(
            root.path(),
            format!("{}\n", prompt_line("u-0001", "2026-07-20T02:00:00.000Z")).as_bytes(),
            "2026-07-31T00:00:00Z",
        );
        let report = run_import(root.path(), false).expect("import");
        assert_eq!(report.spool_begins_at, None, "{report:?}");
        assert_eq!(report.spool_lines_drained, 0, "{report:?}");
        assert_eq!(
            report.locators_scanned, 1,
            "an installation with no hooks has one locator, not two: {report:?}"
        );
        assert_eq!(report.locators_unreadable, 0, "{report:?}");
    }

    #[test]
    fn a_dry_run_over_the_spool_writes_nothing_and_counts_what_a_real_run_would_create() {
        let root = TempRoot::new("spool-dry-run");
        // A ledger has to exist, or `--dry-run` refuses before it reaches the spool.
        drop(Ledger::open(root.path()).expect("create ledger"));
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        spool(
            root.path(),
            "2026-08-04T01:00:02.000Z",
            hook_tool("PreToolUse", "toolu-1", json!({})),
        );

        let dry = run_import(root.path(), true).expect("dry run");
        assert_eq!(
            dry.observations_created.values().sum::<u64>(),
            2,
            "the dry run must count what a real run would create: {dry:?}"
        );
        assert_eq!(
            Ledger::open(root.path())
                .unwrap()
                .observation_count(None)
                .unwrap(),
            0,
            "and must have written none of them"
        );
        assert!(
            Ledger::open(root.path())
                .unwrap()
                .checkpoint(crate::hook::SPOOL_SOURCE_KIND, crate::hook::SPOOL_LOCATOR)
                .unwrap()
                .is_none(),
            "nor advanced a checkpoint, which would strand the lines it only counted"
        );

        let real = run_import(root.path(), false).expect("real run");
        assert_eq!(
            real.observations_created.values().sum::<u64>(),
            2,
            "so the real run still has both to create: {real:?}"
        );
    }

    #[test]
    fn a_spool_line_still_being_written_is_deferred_and_picked_up_once_it_completes() {
        // Hooks on one event run in parallel and several sessions can be active at
        // once, so an import can read this file mid-append. The torn line is neither
        // transformed nor counted as read -- the same rule the transcript channel
        // follows.
        let root = TempRoot::new("spool-torn");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        let whole = json!({
            "v": 1,
            "hook_event_name": "UserPromptSubmit",
            "session_id": HOOK_SESSION,
            "prompt_id": "prm-2",
            "cwd": hook_cwd(),
            "received_at": "2026-08-04T01:01:00.000Z",
        })
        .to_string();
        let path = crate::hook::spool_path(root.path());
        let torn = format!(
            "{}{}",
            std::fs::read_to_string(&path).unwrap(),
            &whole[..whole.len() / 2]
        );
        std::fs::write(&path, torn.as_bytes()).unwrap();

        let first = run_import(root.path(), false).expect("first import");
        assert_eq!(first.lines_incomplete, 1, "{first:?}");
        assert_eq!(first.spool_lines_drained, 1, "{first:?}");
        assert_eq!(
            first.total_gaps(),
            0,
            "a fragment is not a parse failure: {first:?}"
        );

        std::fs::write(
            &path,
            format!(
                "{}{whole}\n",
                std::fs::read_to_string(&path)
                    .unwrap()
                    .trim_end_matches(&whole[..whole.len() / 2])
            )
            .as_bytes(),
        )
        .unwrap();
        let second = run_import(root.path(), false).expect("second import");
        assert_eq!(
            second.spool_lines_drained, 1,
            "the completed line arrives as new: {second:?}"
        );
        assert_eq!(
            Ledger::open(root.path())
                .unwrap()
                .observation_count(Some("dev.cclog.prompt.submitted.v1"))
                .unwrap(),
            2
        );
    }

    #[test]
    fn a_rewritten_spool_resets_the_cursor_and_re_imports_without_duplicating() {
        // Nothing enforces that the spool only grows -- a person can rotate or truncate
        // it. The prefix check catches that, and dedupe makes the rescan free.
        let root = TempRoot::new("spool-rewritten");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        spool(
            root.path(),
            "2026-08-04T01:01:00.000Z",
            hook_prompt("prm-2"),
        );
        run_import(root.path(), false).expect("first import");

        // Drop the first line and append a third: the cursor's prefix no longer holds.
        let path = crate::hook::spool_path(root.path());
        let kept: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect();
        std::fs::write(&path, format!("{}\n", kept.join("\n")).as_bytes()).unwrap();
        spool(
            root.path(),
            "2026-08-04T01:02:00.000Z",
            hook_prompt("prm-3"),
        );

        let report = run_import(root.path(), false).expect("second import");
        assert_eq!(
            report.checkpoints_reset, 1,
            "a spool that is not its predecessor grown by append must reset: {report:?}"
        );
        assert_eq!(
            report.observations_already_present, 1,
            "the line that survived the rewrite is re-transformed and collapses: \
             {report:?}"
        );
        assert_eq!(
            Ledger::open(root.path())
                .unwrap()
                .observation_count(Some("dev.cclog.prompt.submitted.v1"))
                .unwrap(),
            3,
            "three distinct prompts, none duplicated by the rescan"
        );
    }

    #[test]
    fn a_deleted_spool_leaves_what_was_imported_and_starts_the_new_one_over() {
        // The README tells people they may delete the spool. This is what happens when
        // they do: the rows already committed stay, and the fresh file is imported from
        // its first line rather than from a cursor counted against a file that is gone.
        let root = TempRoot::new("spool-deleted");
        spool(
            root.path(),
            "2026-08-04T01:00:00.000Z",
            hook_prompt("prm-1"),
        );
        spool(
            root.path(),
            "2026-08-04T01:01:00.000Z",
            hook_prompt("prm-2"),
        );
        run_import(root.path(), false).expect("first import");

        std::fs::remove_file(crate::hook::spool_path(root.path())).expect("delete the spool");
        let empty = run_import(root.path(), false).expect("import with no spool");
        assert_eq!(
            empty.spool_begins_at, None,
            "there is no spool to say where capture begins: {empty:?}"
        );

        spool(
            root.path(),
            "2026-08-04T02:00:00.000Z",
            hook_prompt("prm-3"),
        );
        let fresh = run_import(root.path(), false).expect("import the fresh spool");
        assert_eq!(
            fresh.checkpoints_reset, 1,
            "the old cursor cannot describe a file that no longer exists: {fresh:?}"
        );
        assert_eq!(fresh.spool_lines_drained, 1, "{fresh:?}");
        assert_eq!(
            Ledger::open(root.path())
                .unwrap()
                .observation_count(Some("dev.cclog.prompt.submitted.v1"))
                .unwrap(),
            3,
            "the two already committed are kept, and the new one is added"
        );
    }

    #[test]
    fn a_subagents_tool_call_is_told_apart_from_the_main_loops() {
        // The one thing the transcript path cannot do reliably, and the reason the
        // subagent fields are on the allowlist at all.
        let root = TempRoot::new("spool-subagent");
        spool(
            root.path(),
            "2026-08-04T01:00:01.000Z",
            hook_tool("PostToolUse", "toolu-main", json!({ "duration_ms": 5 })),
        );
        spool(
            root.path(),
            "2026-08-04T01:00:02.000Z",
            hook_tool(
                "PostToolUse",
                "toolu-sub",
                json!({ "duration_ms": 7, "agent_id": "agent-1", "agent_type": "Explore" }),
            ),
        );
        run_import(root.path(), false).expect("import");

        let bodies = bodies_of_type(root.path(), "dev.cclog.tool.finished.v1");
        let subagent: Vec<&Value> = bodies
            .iter()
            .filter(|b| b["data"].get("agent_ref").is_some())
            .collect();
        assert_eq!(
            subagent.len(),
            1,
            "exactly one call named a subagent: {bodies:?}"
        );
        assert_eq!(subagent[0]["data"]["agent_type"], "Explore");
        assert!(
            subagent[0]["data"]["agent_ref"]
                .as_str()
                .unwrap()
                .starts_with("agt_"),
            "and the agent itself is a pseudonym, not its vendor id"
        );
        assert!(
            !format!("{bodies:?}").contains("agent-1"),
            "the raw agent id must not reach a row: {bodies:?}"
        );
    }

    // -- git: commits as evidence -------------------------------------------------
    //
    // These build synthetic git repositories under a throwaway home with `git init`,
    // because the behaviour under test *is* git's -- what `--author` matches, what
    // `--shortstat` prints for a merge, what a repository with an unborn HEAD does. No
    // repository of the person running the tests is read: `run_git` is called with the
    // synthetic home, never with `$HOME`.

    use crate::git::tests::{TempHome, commit_as, git, init_repository};
    use crate::git::{RepositoryScan, Scan};
    use std::collections::BTreeSet;

    pub(crate) const GIT_IDENTITY: &str = "github.com/acme/api";
    pub(crate) const GIT_AUTHOR: &str = "alice@example.test";
    /// When these tests pretend the import ran. Later than every synthetic commit, as a
    /// real collection is.
    const GIT_OBSERVED_AT: &str = "2026-08-04T00:00:00Z";

    /// A scan wide enough that the synthetic repositories' fixed commit dates fall
    /// inside it, whenever the tests happen to run.
    fn git_scan() -> Scan {
        Scan {
            // Bounded at both ends. Wide enough that a window measured from *now* cannot
            // slide past the synthetic commits' fixed 2026 dates and turn every test
            // below into a vacuous pass -- and narrow enough that `now - since` stays
            // after the Unix epoch, which `git log --since` needs: asked for a date
            // before 1970 it matches nothing at all, which looks exactly like a
            // repository that yielded no commits.
            since_days: 18_000,
            ..Scan::default()
        }
    }

    fn git_run<'a>(root: &'a Path, home: &'a str, dry_run: bool) -> Run<'a> {
        Run {
            root,
            home,
            device: "dev_test",
            observed_at: GIT_OBSERVED_AT,
            dry_run,
        }
    }

    /// Run the git half of an import against `home`, with `identity` already registered
    /// in the ledger the way a transcript import would have registered it.
    pub(crate) fn import_commits(
        root: &Path,
        home: &Path,
        identities: &[&str],
        dry_run: bool,
    ) -> ImportReport {
        let mut ledger = Ledger::open(root).expect("open the ledger");
        for identity in identities {
            ledger
                .register_identity(
                    &pseudonymize("rep", identity),
                    "repository",
                    identity,
                    GIT_OBSERVED_AT,
                )
                .expect("register the repository identity");
        }
        let mut report = ImportReport::default();
        let mut seen = std::collections::HashSet::new();
        run_git(
            &mut ledger,
            &git_run(root, home.to_str().expect("a utf-8 home"), dry_run),
            &git_scan(),
            &mut seen,
            &mut report,
        )
        .expect("collect commits");
        report
    }

    fn commit_bodies(root: &Path) -> Vec<Value> {
        bodies_of_type(root, "dev.cclog.commit.observed.v1")
    }

    #[test]
    fn commits_from_a_repository_the_ledger_knows_become_observations() {
        let root = TempRoot::new("git-commits");
        let home = TempHome::new("git-commits");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&repo, GIT_AUTHOR, "a.txt", 3, "SYNTHETIC first");
        commit_as(&repo, GIT_AUTHOR, "b.txt", 40, "SYNTHETIC second");

        let report = import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);

        assert_eq!(report.git_repositories_scanned, 1);
        assert_eq!(report.git_repositories_collected, 1);
        assert_eq!(report.git_commits_collected, 2);
        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.commit.observed.v1"),
            Some(&2),
            "created {:?}",
            report.observations_created
        );
        assert_eq!(report.total_gaps(), 0);

        let bodies = commit_bodies(root.path());
        assert_eq!(bodies.len(), 2);
        for body in &bodies {
            assert_eq!(body["cclogsourcekind"], "git");
            assert_eq!(
                body["cclogrepositoryref"],
                json!(pseudonymize("rep", GIT_IDENTITY)),
                "a commit is attributed to the repository it was collected from"
            );
            assert_eq!(body["cclogworkspaceref"], Value::Null);
            assert_eq!(body["data"]["message_ref"], Value::Null);
        }
        let buckets: Vec<&Value> = bodies
            .iter()
            .map(|b| &b["data"]["insertions_bucket"])
            .collect();
        assert!(
            buckets.contains(&&json!("1-9")) && buckets.contains(&&json!("10-99")),
            "the line counts are bucketed, not exact: {buckets:?}"
        );
    }

    #[test]
    fn re_importing_the_same_commits_creates_nothing_new() {
        // The window is re-walked on every run by design (a git snapshot is not an
        // append-only file), so the *only* thing standing between a daily import and a
        // ledger full of duplicate commits is the (repository, sha) dedupe key.
        let root = TempRoot::new("git-reimport");
        let home = TempHome::new("git-reimport");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&repo, GIT_AUTHOR, "a.txt", 3, "SYNTHETIC first");
        commit_as(&repo, GIT_AUTHOR, "b.txt", 4, "SYNTHETIC second");

        let first = import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);
        assert_eq!(
            first
                .observations_created
                .get("dev.cclog.commit.observed.v1"),
            Some(&2)
        );
        assert_eq!(commit_bodies(root.path()).len(), 2);

        let second = import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);
        assert_eq!(
            second
                .observations_created
                .get("dev.cclog.commit.observed.v1"),
            None,
            "a re-import must create nothing: {:?}",
            second.observations_created
        );
        assert_eq!(
            second.git_repositories_unchanged, 1,
            "identical bytes and a checkpoint on them: nothing to re-read"
        );
        assert_eq!(
            commit_bodies(root.path()).len(),
            2,
            "and the ledger still holds two commits, not four"
        );

        // A third run *after* a new commit: the short-circuit must not become a reason
        // the ledger stops noticing work.
        commit_as(&repo, GIT_AUTHOR, "c.txt", 5, "SYNTHETIC third");
        let third = import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);
        assert_eq!(
            third
                .observations_created
                .get("dev.cclog.commit.observed.v1"),
            Some(&1),
            "the new commit is created: {:?}",
            third.observations_created
        );
        assert_eq!(
            third.observations_already_present, 2,
            "and the two it already had collapsed onto their existing rows"
        );
        assert_eq!(commit_bodies(root.path()).len(), 3);
    }

    #[test]
    fn a_repository_that_is_no_longer_on_disk_is_reported_rather_than_skipped_silently() {
        // The ledger says work happened in this repository. The repository is not where
        // the identity says it is -- moved, renamed, or deleted. That is a gap in the
        // evidence, and it has to be visible as one rather than looking like a
        // repository nothing was committed to.
        let root = TempRoot::new("git-moved");
        let home = TempHome::new("git-moved");
        let present = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&present, GIT_AUTHOR, "a.txt", 1, "SYNTHETIC first");

        let report = import_commits(
            root.path(),
            home.path(),
            &[GIT_IDENTITY, "github.com/acme/moved-away"],
            false,
        );

        assert_eq!(report.git_repositories_scanned, 2);
        assert_eq!(report.git_repositories_collected, 1);
        assert_eq!(
            report.git_repositories_unresolved.get("missing"),
            Some(&1),
            "the moved repository must be counted and named: {:?}",
            report.git_repositories_unresolved
        );
        assert_eq!(
            commit_bodies(root.path()).len(),
            1,
            "and it contributes no observations of its own"
        );
    }

    #[test]
    fn a_colleagues_commits_never_reach_the_ledger() {
        let root = TempRoot::new("git-authors");
        let home = TempHome::new("git-authors");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&repo, GIT_AUTHOR, "mine.txt", 3, "SYNTHETIC mine");
        commit_as(
            &repo,
            "bob@example.test",
            "theirs.txt",
            400,
            "SYNTHETIC theirs",
        );

        import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);

        let bodies = commit_bodies(root.path());
        assert_eq!(bodies.len(), 1, "only my own commit: {bodies:?}");
        assert_eq!(
            bodies[0]["data"]["insertions_bucket"],
            json!("1-9"),
            "the 400-line commit is the colleague's"
        );
    }

    #[test]
    fn no_commit_message_author_or_email_reaches_the_ledger_or_the_archive() {
        // The leak test, end to end and in both zones. The observation is metadata-only
        // by design; the archived snapshot is cclog's own file here rather
        // than a vendor transcript, so the message and the author never even reach the
        // source zone -- `git log` is not asked for them.
        let root = TempRoot::new("git-leak");
        let home = TempHome::new("git-leak");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(
            &repo,
            GIT_AUTHOR,
            "a.txt",
            2,
            "SYNTHETIC-CANARY refactor the billing thing",
        );

        import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);

        let bodies = format!("{:?}", commit_bodies(root.path()));
        assert!(!bodies.is_empty());
        for forbidden in [
            "SYNTHETIC-CANARY",
            "billing",
            GIT_AUTHOR,
            "example.test",
            "Synthetic Author",
        ] {
            assert!(
                !bodies.contains(forbidden),
                "{forbidden:?} reached an observation: {bodies}"
            );
        }

        // And the snapshot the observations were derived from, which is what a replay
        // reads and what retention keeps.
        let ledger = Ledger::open(root.path()).expect("open the ledger");
        let snapshots = ledger
            .find_snapshots(&cclogger_archive::SnapshotFilter {
                source_kind: Some("git"),
                ..Default::default()
            })
            .expect("find the git snapshots");
        assert_eq!(snapshots.len(), 1);
        let archived = String::from_utf8_lossy(
            &ledger
                .read(&snapshots[0].object_id)
                .expect("read the snapshot"),
        )
        .into_owned();
        for forbidden in [
            "SYNTHETIC-CANARY",
            "billing",
            GIT_AUTHOR,
            "Synthetic Author",
        ] {
            assert!(
                !archived.contains(forbidden),
                "{forbidden:?} reached the archived snapshot: {archived}"
            );
        }
        assert!(
            archived.contains(GIT_IDENTITY),
            "the snapshot does carry the repository identity it was collected for: {archived}"
        );
    }

    #[test]
    fn a_commit_lands_on_the_same_repository_ref_as_the_sessions_that_produced_it() {
        // The join the whole feature rests on. A commit's repository ref is derived from
        // the normalized identity, and a transcript's from a cwd -- two different
        // inputs, which have to arrive at the same pseudonym or `log` can never put a
        // commit beside the block it landed in.
        let root = TempRoot::new("git-join");
        let home = TempHome::new("git-join");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&repo, GIT_AUTHOR, "a.txt", 3, "SYNTHETIC first");

        // The transcript half, imported the ordinary way: its cwd is under the machine's
        // own home, since that is what the importer resolves cwds against.
        let cwd = format!(
            "{}/ghq/{GIT_IDENTITY}",
            std::env::var("HOME").unwrap_or_default()
        );
        let mut transcript = prompt_line_at("p-1", "2026-07-26T01:00:00.000Z", &cwd);
        transcript.push('\n');
        archive_snapshot(root.path(), transcript.as_bytes(), GIT_OBSERVED_AT);
        run_import(root.path(), false).expect("import the transcript");
        import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);

        let prompts = bodies_of_type(root.path(), "dev.cclog.prompt.submitted.v1");
        let commits = commit_bodies(root.path());
        assert_eq!(prompts.len(), 1);
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0]["cclogrepositoryref"], prompts[0]["cclogrepositoryref"],
            "a commit and the session it came out of must share one repository ref"
        );
        assert!(
            commits[0]["cclogrepositoryref"]
                .as_str()
                .expect("a repository ref")
                .starts_with("rep_")
        );
    }

    #[test]
    fn one_sha_in_two_repositories_is_two_pieces_of_evidence() {
        // A fork, a submodule, or a vendored copy holds commits with the same sha. Each
        // repository's copy is evidence in that repository, so both are kept -- which
        // only works because the dedupe key is scoped by repository.
        let root = TempRoot::new("git-fork");
        let home = TempHome::new("git-fork");
        let origin = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&origin, GIT_AUTHOR, "a.txt", 3, "SYNTHETIC shared");

        let fork_identity = "github.com/acme/fork";
        let fork = crate::git::repository_path(home.path(), fork_identity).expect("a path");
        std::fs::create_dir_all(fork.parent().expect("a parent")).expect("create the parent");
        git(
            home.path(),
            &[
                "clone",
                "--quiet",
                origin.to_str().expect("utf-8"),
                fork.to_str().expect("utf-8"),
            ],
        );
        git(&fork, &["config", "user.email", GIT_AUTHOR]);

        let report = import_commits(
            root.path(),
            home.path(),
            &[GIT_IDENTITY, fork_identity],
            false,
        );
        assert_eq!(report.git_commits_collected, 2, "one commit in each");

        let bodies = commit_bodies(root.path());
        assert_eq!(bodies.len(), 2, "both are kept: {bodies:?}");
        let subjects: BTreeSet<&str> = bodies
            .iter()
            .map(|b| b["subject"].as_str().expect("a subject"))
            .collect();
        assert_eq!(
            subjects.len(),
            1,
            "and they are recognizably the same commit: {subjects:?}"
        );
        let repositories: BTreeSet<&str> = bodies
            .iter()
            .map(|b| b["cclogrepositoryref"].as_str().expect("a repository"))
            .collect();
        assert_eq!(repositories.len(), 2, "in two repositories");
    }

    #[test]
    fn a_dry_run_collects_and_reports_but_writes_nothing() {
        let root = TempRoot::new("git-dry");
        let home = TempHome::new("git-dry");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&repo, GIT_AUTHOR, "a.txt", 3, "SYNTHETIC first");

        let report = import_commits(root.path(), home.path(), &[GIT_IDENTITY], true);
        assert_eq!(
            report
                .observations_created
                .get("dev.cclog.commit.observed.v1"),
            Some(&1),
            "a dry run says what a real one would create"
        );
        assert!(
            commit_bodies(root.path()).is_empty(),
            "and writes none of it"
        );
    }

    #[test]
    fn a_malformed_commit_record_is_diagnosed_rather_than_counted_as_a_commit() {
        // Reached through the shared line classification, so a git line that cannot be
        // transformed produces the same diagnosed marker every other source's does.
        let keystore = Keystore::new().map("repository", GIT_IDENTITY, "rep_TEST");
        let line = json!({
            "repository": GIT_IDENTITY,
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "author_time": "2026-07-26T01:00:00+00:00",
        })
        .to_string();

        match classify_line(
            Vendor::Git,
            &line,
            &keystore,
            "sha256:aaa",
            GIT_OBSERVED_AT,
            1,
            false,
        ) {
            LineOutcome::Gap { draft, bucket } => {
                assert_eq!(bucket, GapBucket::MissingField("files_changed"));
                assert_eq!(draft.data["reason"], "missing_field");
                assert_eq!(draft.data["detail"], "files_changed");
                assert_eq!(draft.source_kind, SourceKind::Git);
                assert_eq!(
                    draft.data["time_basis"], "occurred_at",
                    "the record's own author time still dates the marker"
                );
            }
            other => panic!("a commit with no measured diffstat must be a gap, got {other:?}"),
        }
    }

    #[test]
    fn every_repository_the_ledger_knows_is_looked_for_by_a_full_import() {
        // The wiring: `run_import` must actually reach the git source. The repository
        // identity is deliberately one that cannot exist on the machine running this, so
        // the assertion is about the scan happening, not about what it found.
        let root = TempRoot::new("git-wiring");
        let cwd = format!(
            "{}/ghq/github.com/cclogger-synthetic/no-such-repository",
            std::env::var("HOME").unwrap_or_default()
        );
        let mut transcript = prompt_line_at("p-1", "2026-07-26T01:00:00.000Z", &cwd);
        transcript.push('\n');
        archive_snapshot(root.path(), transcript.as_bytes(), GIT_OBSERVED_AT);

        let report = run_import(root.path(), false).expect("import");
        assert_eq!(
            report.git_repositories_scanned, 1,
            "the one repository the transcript registered"
        );
        assert_eq!(
            report.git_repositories_unresolved.get("missing"),
            Some(&1),
            "and it is not on disk, which is reported: {:?}",
            report.git_repositories_unresolved
        );
    }

    #[test]
    fn a_repository_with_no_commits_of_mine_is_not_reported_as_unreadable() {
        // Nothing to collect is a different fact from nothing collectable, and only the
        // second is a gap in the evidence.
        let root = TempRoot::new("git-none-mine");
        let home = TempHome::new("git-none-mine");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(
            &repo,
            "bob@example.test",
            "theirs.txt",
            3,
            "SYNTHETIC theirs",
        );

        let report = import_commits(root.path(), home.path(), &[GIT_IDENTITY], false);
        assert_eq!(report.git_repositories_collected, 1);
        assert_eq!(report.git_commits_collected, 0);
        assert!(
            report.git_repositories_unresolved.is_empty(),
            "a readable repository with none of my commits is not a gap: {:?}",
            report.git_repositories_unresolved
        );
        assert!(commit_bodies(root.path()).is_empty());
    }

    #[test]
    fn a_repository_that_cannot_say_whose_commits_are_whose_yields_none() {
        let home = TempHome::new("git-no-identity");
        let repo = init_repository(home.path(), GIT_IDENTITY, GIT_AUTHOR);
        commit_as(&repo, GIT_AUTHOR, "a.txt", 1, "SYNTHETIC first");
        assert_eq!(
            unresolved_reason(&RepositoryScan::NoIdentity),
            "no_identity",
            "the reason is a fixed label, printable beside the others"
        );
        // The branch itself is exercised in `crate::git`'s own tests, where the author
        // list can be emptied without depending on the machine's git configuration.
        assert!(repo.exists());
    }
}
