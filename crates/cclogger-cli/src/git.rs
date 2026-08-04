//! Collecting commits from the repositories the ledger already knows about.
//!
//! This is the only part of cclog that reads something other than an archived snapshot
//! or its own spool: it runs `git log` against a working copy. That is a deliberate
//! choice and a bounded one -- see "Bounding it" below -- because a commit is the one
//! signal in this domain that says work reached a durable artefact, it carries its own
//! author timestamp, and it attributes itself to a repository with certainty rather than
//! by resolving a working directory.
//!
//! # Which repositories
//!
//! The ones the ledger already holds an identity for: `workspace_identity` has a row per
//! repository the person has worked in, as a normalized `host/owner/repo`. Walking those
//! rather than the disk means cclog only ever looks at repositories it has already
//! observed AI work in -- it does not go discovering the rest of someone's filesystem --
//! and it needs no configuration to say which repositories matter.
//!
//! The cost is that a normalized identity is **not a path**. It is turned back into one
//! by the inverse of the rule that produced it (`cclogger_domain::workspace::resolve`):
//! `<home>/ghq/<host>/<owner>/<repo>`. That inverse is exact for a repository still
//! sitting where ghq puts it, and simply wrong for one that has been moved, renamed or
//! deleted -- which is why every outcome below that is not "collected" is *counted and
//! reported* rather than skipped. A repository that has moved is a stated gap in the
//! evidence, not an absence of commits.
//!
//! # Which commits
//!
//! - **Author time, not commit time.** They agree until a rebase, an amend or a
//!   cherry-pick; after one, the author time is still when the work was done and the
//!   commit time is when the history was last rewritten.
//! - **Local branches, tags and remote-tracking branches** ([`REF_SCOPES`]) -- not
//!   `--all`, which also walks `refs/stash`, and a stash is explicitly work that did
//!   *not* land.
//! - **Non-merge commits only.** `git log` prints no diffstat for a merge, so a merge
//!   would have to claim `changed_paths_count: 0` -- a fabricated measurement, and one
//!   that would be indistinguishable from the empty commits that really do measure
//!   zero. The work a merge brings in is already present as the commits it merges. This
//!   is a stated exclusion: it is printed with the import's git section.
//! - **A bounded window** ([`Scan::since_days`], 90 days by default). A first import
//!   that walks the whole history of a dozen repositories is a different operation from
//!   one that walks a quarter -- unbounded in time, in output size, and in how long it
//!   holds the ledger. The window is re-walked on every import (commits dedupe on
//!   `(repository, sha)`), so a commit rebased or cherry-picked into the window later
//!   is still picked up; one authored before it never is, and that is the stated limit.
//!
//! # Whose commits
//!
//! Only the person's own, and cclog does not presume to know who that is: the author
//! filter is built from `git config --get-all user.email` **in that repository**, which
//! is every identity git itself would commit as there (system, global and repo-local
//! values, so a work address configured for one repository is included). Each is matched
//! as the fixed string `<email>` -- angle brackets included, so `<a@b.test>` cannot match
//! `<a@b.testing>` -- against the `Name <email>` line git compares `--author` to.
//!
//! A repository with no configured identity yields **no commits at all**, reported as
//! such. Guessing (the first author in the log, say) would either drop the person's work
//! or count a colleague's, and there is no third answer available from the repository
//! alone.
//!
//! # Bounding it
//!
//! An import that finishes in 25 seconds must not become one that never finishes. A
//! repository on a network mount, a history of a million commits, or a `git` that wants
//! to prompt are all real, so every invocation here is bounded four ways:
//!
//! 1. a wall-clock [`Scan::timeout`] per invocation, after which the child is killed;
//! 2. a byte cap on what is read back ([`MAX_OUTPUT_BYTES`]), so a huge history cannot
//!    fill memory -- and the child is killed once it is hit;
//! 3. [`Scan::max_commits`] and the `--since` window, which bound the work git does;
//! 4. `stdin` closed, `stderr` discarded, `GIT_TERMINAL_PROMPT=0` and no pager, so
//!    nothing can block on a terminal that is not there. Nothing here touches the
//!    network, which is the only thing that would ask for a credential in the first
//!    place.
//!
//! `stderr` is discarded rather than captured on purpose: capturing it means a second
//! pipe that can fill while this side is reading the first, which is the classic way a
//! bounded read deadlocks anyway. Git's own diagnostics are also localized, so matching
//! on them would make the classification depend on the machine's language. What
//! distinguishes the failure modes here is the exit status and nothing else.
//!
//! The one thing not bounded by any of that is `std::fs::metadata` on a repository path,
//! used to tell "moved or deleted" from "not a git repository" without spawning
//! anything. A *dead* network mount can block that call, and no in-process timeout can
//! cancel it. It is stated rather than papered over: the alternative -- letting `git -C`
//! discover the missing path -- moves the same stall into a killable child but pays a
//! subprocess for every repository that is simply gone, and can only tell the two cases
//! apart by parsing localized text.

use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The refs a scan walks. Deliberately not `--all`: that also walks `refs/stash`, and a
/// stash entry is a commit whose whole point is that the work did *not* land.
///
/// `HEAD` is deliberately absent too, and that is a stated limitation rather than an
/// oversight: a commit made on a detached `HEAD` and never attached to a branch or a tag
/// is not collected. Naming `HEAD` as a rev would cover it, and would also turn any
/// repository where that rev failed to resolve into an unreadable one -- a whole
/// repository's evidence traded for a case that resolves itself the moment the commit is
/// put on a branch.
const REF_SCOPES: [&str; 3] = ["--branches", "--tags", "--remotes"];

/// How much of one `git log` this will read before giving up on it. A commit record is
/// ~120 bytes here, so this is far past [`Scan::max_commits`] in practice -- it is the
/// backstop for a `git` that streams something unexpected, not the normal bound.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// How long a repository's own history is walked back, how many commits are taken from
/// it, and how long any one `git` invocation may run.
#[derive(Debug, Clone, Copy)]
pub struct Scan {
    /// The `--since` window, in days. Re-walked on every import.
    pub since_days: i64,
    /// The per-repository ceiling. Hitting it is reported, never silently truncating.
    pub max_commits: usize,
    pub timeout: Duration,
}

impl Scan {
    /// A quarter: long enough that a first import has something to say about the periods
    /// a report can be asked for, short enough that it is not a full-history walk of
    /// every repository on the machine. Named so the CLI can state the window it walked
    /// rather than repeating the number.
    pub const DEFAULT_SINCE_DAYS: i64 = 90;
}

impl Default for Scan {
    fn default() -> Self {
        Self {
            since_days: Self::DEFAULT_SINCE_DAYS,
            // ~22 commits a day for the whole window. A repository that exceeds it is
            // reported as truncated rather than quietly cut short.
            max_commits: 2_000,
            timeout: Duration::from_secs(20),
        }
    }
}

/// What one repository's scan produced. Every variant that is not
/// [`Collected`](RepositoryScan::Collected) is a *stated* gap: the importer counts it
/// and the CLI prints it, because "this repository contributed no commits" and "this
/// repository could not be read" are different facts and only one of them is about the
/// work.
#[derive(Debug, PartialEq, Eq)]
pub enum RepositoryScan {
    Collected {
        /// One JSONL record per commit, oldest first -- the bytes that become this
        /// repository's snapshot.
        records: Vec<String>,
        /// Whether [`Scan::max_commits`] cut the history short.
        truncated: bool,
    },
    /// Nothing at the path the identity resolves to: moved, renamed, or deleted.
    Missing,
    /// Something is there, but it is not a git repository.
    NotARepository,
    /// A repository with no commits yet (an unborn `HEAD`). Distinct from
    /// [`NotARepository`](RepositoryScan::NotARepository) so a freshly created
    /// repository does not read as a broken one.
    NoCommitsYet,
    /// No `user.email` is configured for this repository, so there is no way to tell the
    /// person's commits from anyone else's.
    NoIdentity,
    /// `git` ran and failed, or could not be run at all.
    Unreadable,
    /// `git` was still going when [`Scan::timeout`] expired, and was killed.
    TimedOut,
}

/// The filesystem path a normalized repository identity resolves to under `home`, or
/// `None` if the identity is not one this build produced.
///
/// The exact inverse of `cclogger_domain::workspace::resolve`, which is what put the
/// identity in the ledger: `<home>/ghq/<host>/<owner>/<repo>`. Identities come from
/// cclog's own ledger rather than from a user, but they are still validated here --
/// three non-empty segments, none of them `.` or `..` -- because the result is a path
/// this module then runs a subprocess in, and a `..` segment would silently walk out of
/// the ghq tree entirely.
pub fn repository_path(home: &Path, identity: &str) -> Option<PathBuf> {
    let segments: Vec<&str> = identity.split('/').collect();
    if segments.len() != 3 {
        return None;
    }
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == ".." || s.contains('\\'))
    {
        return None;
    }
    let mut path = home.join("ghq");
    for segment in segments {
        path.push(segment);
    }
    Some(path)
}

/// Scan one repository: classify what is at its path, work out whose commits to ask
/// for, and collect them.
pub fn scan_repository(home: &Path, identity: &str, scan: &Scan) -> RepositoryScan {
    let Some(path) = repository_path(home, identity) else {
        return RepositoryScan::Missing;
    };
    // Cheap, and the only classification that does not need a subprocess: a repository
    // that has been moved or deleted is the common case this exists for, and paying
    // `git` for each one would be the bulk of the scan on a machine whose ghq tree has
    // moved on. See the module header for the one caveat this carries.
    if std::fs::metadata(&path).is_err() {
        return RepositoryScan::Missing;
    }
    // `--verify --quiet HEAD` separates the three states apart by exit status alone,
    // with no localized text to match on: 0 is a repository with commits, 1 is a
    // repository whose HEAD is unborn, anything else is not a repository.
    match run(&path, &["rev-parse", "--quiet", "--verify", "HEAD"], scan) {
        Ok(output) if output.code == Some(0) => {}
        Ok(output) if output.code == Some(1) => return RepositoryScan::NoCommitsYet,
        Ok(_) => return RepositoryScan::NotARepository,
        Err(Failure::TimedOut) => return RepositoryScan::TimedOut,
        Err(Failure::Unreadable) => return RepositoryScan::Unreadable,
    }

    let authors = match identities(&path, scan) {
        Ok(authors) => authors,
        Err(Failure::TimedOut) => return RepositoryScan::TimedOut,
        Err(Failure::Unreadable) => return RepositoryScan::Unreadable,
    };
    collect(&path, identity, &authors, scan)
}

/// Every git identity configured for the repository at `path`, most general first.
///
/// `--get-all` rather than `--get`: git resolves `user.email` from system, global and
/// repository config, and a person who has a work address configured for one repository
/// and a personal one globally authors commits under both. Taking only the effective
/// value would drop half of their own work in exactly the repositories where the
/// distinction was deliberate.
///
/// An empty vector means no identity is configured anywhere, which is not an error --
/// it is the answer, and [`collect`] turns it into [`RepositoryScan::NoIdentity`].
fn identities(path: &Path, scan: &Scan) -> Result<Vec<String>, Failure> {
    let output = run(path, &["config", "--get-all", "user.email"], scan)?;
    // Exit 1 is "the key is not set anywhere", which is a real answer, not a failure.
    if output.code != Some(0) && output.code != Some(1) {
        return Err(Failure::Unreadable);
    }
    let mut seen: Vec<String> = Vec::new();
    for line in output.stdout.lines() {
        let email = line.trim();
        if !email.is_empty() && !seen.iter().any(|e| e == email) {
            seen.push(email.to_string());
        }
    }
    Ok(seen)
}

/// Run `git log` for `authors` and normalize what comes back into JSONL records.
///
/// Separate from [`scan_repository`] so the author list is an argument rather than
/// something read from the machine the test happens to run on.
fn collect(path: &Path, identity: &str, authors: &[String], scan: &Scan) -> RepositoryScan {
    if authors.is_empty() {
        return RepositoryScan::NoIdentity;
    }

    let since = format!("--since={}.days.ago", scan.since_days);
    // One past the ceiling, so "the history is exactly this long" and "the ceiling cut
    // it short" are distinguishable. Asking for exactly `max_commits` and calling a full
    // result truncated would report a repository with precisely that many commits as
    // incomplete, which is a claim about missing evidence that is not true.
    let max = format!("--max-count={}", scan.max_commits.saturating_add(1));
    // `%H` and `%aI`: the sha, and the author date in strict ISO 8601 with the author's
    // own offset (the ledger normalizes it to UTC for its indexed column and keeps this
    // form verbatim in the row). Nothing else is asked for -- no `%s`, no `%an`, no
    // `%ae` -- so no message and no author ever reaches even the snapshot this writes.
    let mut args: Vec<String> = vec![
        "log".to_string(),
        "--format=%H%x09%aI".to_string(),
        "--shortstat".to_string(),
        "--no-merges".to_string(),
        // Oldest first, so a snapshot of a later window is an *extension* of an earlier
        // one wherever the window has not slid past a commit -- which is what makes a
        // re-import's diff small and readable, even though correctness rests on the
        // dedupe key rather than on the order.
        "--reverse".to_string(),
        since,
        max,
        // Fixed strings, so a `+` or a `.` in an address is a character and not a
        // regular-expression operator.
        "--fixed-strings".to_string(),
    ];
    args.extend(REF_SCOPES.iter().map(|s| (*s).to_string()));
    for email in authors {
        args.push(format!("--author=<{email}>"));
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();

    let output = match run(path, &borrowed, scan) {
        Ok(output) => output,
        Err(Failure::TimedOut) => return RepositoryScan::TimedOut,
        Err(Failure::Unreadable) => return RepositoryScan::Unreadable,
    };
    // A truncated read killed the child on purpose, so its exit status says only that
    // this side stopped listening. The complete lines already read are real records and
    // are kept, with `truncated` saying the rest was not seen -- checking the status
    // first would throw away a repository's whole history over a bound this code chose.
    if !output.truncated && output.code != Some(0) {
        return RepositoryScan::Unreadable;
    }

    let mut records = parse_log(&output.stdout, identity);
    let truncated = output.truncated || records.len() > scan.max_commits;
    if records.len() > scan.max_commits {
        // `--max-count` selects the newest N+1 and `--reverse` then orders them
        // oldest-first, so the one to drop is at the *front*. Keeping the head instead
        // would mean a repository over the ceiling silently lost its most recent
        // commits -- the ones a report is most likely to be asked about.
        records = records.split_off(records.len() - scan.max_commits);
    }
    RepositoryScan::Collected { records, truncated }
}

/// One commit, as `git log --format=%H%x09%aI --shortstat` describes it.
struct Commit {
    sha: String,
    author_time: String,
    /// `None` until a diffstat line is read. A commit that never gets one is an *empty*
    /// commit -- git prints no stat line for one -- which really did change nothing.
    stat: Option<Option<Stat>>,
}

#[derive(Clone, Copy)]
struct Stat {
    files: u64,
    insertions: u64,
    deletions: u64,
}

/// Turn `git log` output into one JSONL record per commit.
///
/// The output is line-oriented and has exactly three shapes: a `<sha>\t<author date>`
/// header, a blank line, and a ` N files changed, ...` summary. A fourth shape means git
/// is not printing what this was written against, and the commit it belongs to is
/// emitted **without** its counts, so the adapter turns it into a diagnosed gap. The
/// alternative -- treating an unrecognized line as no change -- would report every
/// commit in the ledger as having changed nothing the day git's output drifted, with
/// nothing to show that anything had gone wrong.
fn parse_log(stdout: &str, identity: &str) -> Vec<String> {
    let mut commits: Vec<Commit> = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(commit) = parse_header(line) {
            commits.push(commit);
            continue;
        }
        let Some(current) = commits.last_mut() else {
            // A stat line before any header: git printed something this cannot place.
            continue;
        };
        // Only the first stat line after a header counts; a second would mean the
        // output is not what this was written against, and is recorded as unparsed.
        current.stat = Some(match current.stat {
            None => parse_shortstat(line),
            Some(_) => None,
        });
    }

    commits
        .into_iter()
        .map(|commit| {
            let mut record = json!({
                "repository": identity,
                "commit": commit.sha,
                "author_time": commit.author_time,
            });
            // `None` (a line that could not be parsed) leaves the counts off the record
            // entirely; the adapter has no measurement to report and the importer gaps
            // it. `Some(None)` -- no stat line at all -- is an empty commit, which
            // measured zero.
            let stat = match commit.stat {
                None => Some(Stat {
                    files: 0,
                    insertions: 0,
                    deletions: 0,
                }),
                Some(stat) => stat,
            };
            if let Some(stat) = stat {
                record["files_changed"] = json!(stat.files);
                record["insertions"] = json!(stat.insertions);
                record["deletions"] = json!(stat.deletions);
            }
            record.to_string()
        })
        .collect()
}

/// A `<sha>\t<author date>` header line.
///
/// 40 hex characters for a SHA-1 repository, 64 for one created with
/// `--object-format=sha256`. Both are accepted because rejecting the second would not
/// fail loudly: a header this cannot read is indistinguishable from a diffstat line, so
/// every commit in such a repository would be dropped without a gap marker to show for
/// it -- exactly the silent loss the rest of this module is built to avoid.
fn parse_header(line: &str) -> Option<Commit> {
    let (sha, author_time) = line.split_once('\t')?;
    if !matches!(sha.len(), 40 | 64) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if author_time.is_empty() {
        return None;
    }
    Some(Commit {
        sha: sha.to_string(),
        author_time: author_time.to_string(),
        stat: None,
    })
}

/// ` 4 files changed, 137 insertions(+), 22 deletions(-)`, in any of the forms git
/// prints it: the clauses that are zero are omitted entirely, so a missing clause is a
/// measured zero and not a missing measurement. `None` when the line is not a diffstat
/// this understands -- see [`parse_log`] for why that is not rounded down to zero.
fn parse_shortstat(line: &str) -> Option<Stat> {
    let mut stat = Stat {
        files: 0,
        insertions: 0,
        deletions: 0,
    };
    let mut saw_files = false;
    for clause in line.split(',') {
        let clause = clause.trim();
        let (count, word) = clause.split_once(' ')?;
        let count: u64 = count.parse().ok()?;
        if word.starts_with("file") {
            stat.files = count;
            saw_files = true;
        } else if word.starts_with("insertion") {
            stat.insertions = count;
        } else if word.starts_with("deletion") {
            stat.deletions = count;
        } else {
            return None;
        }
    }
    saw_files.then_some(stat)
}

/// Why an invocation produced nothing usable. Deliberately only two: what separates
/// them is whether the bound was hit, and everything else git can do is the same fact
/// to a caller -- it did not answer.
enum Failure {
    TimedOut,
    Unreadable,
}

struct Output {
    code: Option<i32>,
    stdout: String,
    /// Whether [`MAX_OUTPUT_BYTES`] cut the output short.
    truncated: bool,
}

/// Run one `git` invocation in `dir`, bounded by `scan.timeout` and
/// [`MAX_OUTPUT_BYTES`].
///
/// `GIT_DIR` and friends are removed from the child's environment, not merely left
/// alone: they override `-C` when set, so an import run from inside a git hook or a
/// wrapper that exports them would otherwise read one repository while labelling every
/// commit with another's identity. That is a silent misattribution, which is the failure
/// this project treats as its worst.
fn run(dir: &Path, args: &[&str], scan: &Scan) -> Result<Output, Failure> {
    let mut child = Command::new("git")
        .arg("--no-pager")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Not a language this parses: the diffstat summary is translated in a
        // gettext-enabled build, and `parse_shortstat` reads English.
        .env("LC_ALL", "C")
        // Nothing here touches the network, so nothing should ever want a credential --
        // but a prompt is a hang, and this makes it an error instead.
        .env("GIT_TERMINAL_PROMPT", "0")
        // Reading should not take a lock a concurrent session then waits on.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .spawn()
        .map_err(|_| Failure::Unreadable)?;

    let deadline = Instant::now() + scan.timeout;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Failure::Unreadable);
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // Read on another thread so the wait is bounded. One byte past the cap, so hitting
    // it exactly is distinguishable from stopping there.
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let result = stdout
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut buffer)
            .map(|_| buffer);
        // The receiver is gone on the timeout path; there is nobody to tell.
        let _ = tx.send(result);
    });

    let read = match rx.recv_timeout(scan.timeout) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            kill(&mut child);
            return Err(Failure::Unreadable);
        }
        Err(_) => {
            kill(&mut child);
            return Err(Failure::TimedOut);
        }
    };
    let truncated = read.len() > MAX_OUTPUT_BYTES;
    if truncated {
        // The child is still writing into a pipe nobody is reading. Stop it here rather
        // than leaving it to fill and block until the timeout.
        kill(&mut child);
    }
    let mut stdout = String::from_utf8_lossy(&read).into_owned();
    if truncated {
        // The cut lands mid-line, and a half-read record is not a record. Drop back to
        // the last complete line, exactly as the snapshot importer's torn-tail rule does.
        match stdout.rfind('\n') {
            Some(end) => stdout.truncate(end + 1),
            None => stdout.clear(),
        }
    }

    // Reaped with the same deadline the read had: `wait` on a child that closed stdout
    // and left a grandchild holding it would otherwise block after every bound above
    // has already been honoured.
    let code = wait_bounded(&mut child, deadline);
    Ok(Output {
        code,
        stdout,
        truncated,
    })
}

fn kill(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The child's exit code, waiting no longer than `deadline` before killing it.
/// `None` when it did not exit on its own or was killed by a signal.
fn wait_bounded(child: &mut std::process::Child, deadline: Instant) -> Option<i32> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            kill(child);
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;

    /// A throwaway directory tree to build synthetic repositories in, removed when the
    /// binding drops.
    pub(crate) struct TempHome(PathBuf);

    impl TempHome {
        pub(crate) fn new(name: &str) -> Self {
            let unique = format!("{}-{:?}", std::process::id(), std::thread::current().id());
            let path = std::env::temp_dir().join(format!("cclog-git-{name}-{unique}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the synthetic home");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run one git command in `dir`, hermetically: the synthetic repositories these
    /// tests build must not inherit the machine's own git configuration (a global
    /// `commit.gpgsign`, a `user.email`, a template directory or a hook), or the same
    /// test would behave differently on two machines.
    ///
    /// Deliberately not [`run`]: that is the code under test, and it must *not* be
    /// hermetic -- reading the machine's configured identity is the whole point of it.
    pub(crate) fn git(dir: &Path, args: &[&str]) {
        git_at(dir, args, DEFAULT_COMMIT_DATE);
    }

    /// The date every synthetic commit carries unless a test names another. Fixed and in
    /// the past, so which side of a `--since` window it falls on is a property of the
    /// fixture rather than of when the test runs.
    pub(crate) const DEFAULT_COMMIT_DATE: &str = "2026-07-20T03:20:05+00:00";

    pub(crate) fn git_at(dir: &Path, args: &[&str], date: &str) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_COMMITTER_NAME", "Synthetic Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.test")
            .output()
            .unwrap_or_else(|e| panic!("run git {args:?} in {}: {e}", dir.display()));
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A repository at `<home>/ghq/<identity>` whose configured identity is `email`.
    pub(crate) fn init_repository(home: &Path, identity: &str, email: &str) -> PathBuf {
        let path = repository_path(home, identity).expect("a well-formed identity");
        std::fs::create_dir_all(&path).expect("create the repository directory");
        git(&path, &["init", "--quiet", "--initial-branch=main"]);
        git(&path, &["config", "user.email", email]);
        git(&path, &["config", "user.name", "Synthetic Author"]);
        git(&path, &["config", "commit.gpgsign", "false"]);
        path
    }

    /// One commit in `path`, authored by `email`, touching `file` with `lines` lines.
    pub(crate) fn commit_as(path: &Path, email: &str, file: &str, lines: usize, message: &str) {
        commit_at(path, email, file, lines, message, DEFAULT_COMMIT_DATE);
    }

    /// [`commit_as`] with the author date named, for the tests that care *when* work
    /// landed rather than that it did.
    pub(crate) fn commit_at(
        path: &Path,
        email: &str,
        file: &str,
        lines: usize,
        message: &str,
        date: &str,
    ) {
        let body = "SYNTHETIC line\n".repeat(lines);
        std::fs::write(path.join(file), body).expect("write the file");
        git_at(path, &["add", file], date);
        git_at(
            path,
            &[
                "-c",
                &format!("user.email={email}"),
                "-c",
                "user.name=Synthetic Author",
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                message,
            ],
            date,
        );
    }

    fn scan() -> Scan {
        Scan {
            since_days: 3_650,
            max_commits: 100,
            timeout: Duration::from_secs(20),
        }
    }

    fn records(result: &RepositoryScan) -> Vec<Value> {
        match result {
            RepositoryScan::Collected { records, .. } => records
                .iter()
                .map(|line| serde_json::from_str(line).expect("each record is JSON"))
                .collect(),
            other => panic!("expected collected commits, got {other:?}"),
        }
    }

    const IDENTITY: &str = "github.com/acme/api";
    const MINE: &str = "alice@example.test";
    const THEIRS: &str = "bob@example.test";

    #[test]
    fn a_repository_that_is_no_longer_on_disk_is_reported_rather_than_skipped() {
        let home = TempHome::new("missing");
        // Nothing is created: this is the identity of a repository that has been moved,
        // renamed or deleted since the ledger last saw work in it.
        assert_eq!(
            scan_repository(home.path(), IDENTITY, &scan()),
            RepositoryScan::Missing
        );
    }

    #[test]
    fn a_path_that_is_not_a_git_repository_is_reported_as_that_and_not_as_missing() {
        let home = TempHome::new("not-a-repo");
        let path = repository_path(home.path(), IDENTITY).expect("a well-formed identity");
        std::fs::create_dir_all(&path).expect("create the directory");
        std::fs::write(path.join("README.md"), "not a repository").expect("write a file");
        assert_eq!(
            scan_repository(home.path(), IDENTITY, &scan()),
            RepositoryScan::NotARepository
        );
    }

    #[test]
    fn a_repository_with_no_commits_yet_is_not_reported_as_broken() {
        let home = TempHome::new("unborn");
        init_repository(home.path(), IDENTITY, MINE);
        assert_eq!(
            scan_repository(home.path(), IDENTITY, &scan()),
            RepositoryScan::NoCommitsYet
        );
    }

    #[test]
    fn a_repository_with_no_configured_identity_yields_no_commits_at_all() {
        // Through `collect` with an empty author list rather than through
        // `scan_repository`: git resolves `user.email` from the machine's global config
        // too, so a repository with none of its own still has one on most machines --
        // and a test that passed only where the developer had no git identity would be
        // asserting nothing anywhere else.
        let home = TempHome::new("no-identity");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "a.txt", 3, "SYNTHETIC first");
        assert_eq!(
            collect(&path, IDENTITY, &[], &scan()),
            RepositoryScan::NoIdentity,
            "guessing whose commits these are would either drop the person's work or \
             count a colleague's"
        );
    }

    #[test]
    fn another_authors_commits_are_excluded() {
        let home = TempHome::new("authors");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "mine.txt", 3, "SYNTHETIC mine");
        commit_as(&path, THEIRS, "theirs.txt", 4, "SYNTHETIC theirs");
        commit_as(&path, MINE, "mine2.txt", 5, "SYNTHETIC mine again");

        let collected = collect(&path, IDENTITY, &[MINE.to_string()], &scan());
        let records = records(&collected);
        assert_eq!(
            records.len(),
            2,
            "three commits, two of them mine: {records:?}"
        );
        // Nothing on the record says who authored it, so the exclusion is checked
        // through what only my commits touched.
        let files: Vec<u64> = records
            .iter()
            .map(|r| r["files_changed"].as_u64().expect("files_changed"))
            .collect();
        assert_eq!(files, vec![1, 1]);
        let insertions: Vec<u64> = records
            .iter()
            .map(|r| r["insertions"].as_u64().expect("insertions"))
            .collect();
        assert_eq!(
            insertions,
            vec![3, 5],
            "the 4-line commit is the colleague's and must not be here"
        );
    }

    #[test]
    fn a_colleague_whose_address_extends_mine_is_not_counted_as_me() {
        // `--author` is a substring match against `Name <email>`, so the angle brackets
        // are what make it exact. Without them `alice@example.test` matches
        // `alice@example.test.example.test` -- a different person.
        let home = TempHome::new("prefix-author");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "mine.txt", 3, "SYNTHETIC mine");
        commit_as(
            &path,
            "alice@example.test.example.test",
            "theirs.txt",
            9,
            "SYNTHETIC lookalike",
        );

        let records = records(&collect(&path, IDENTITY, &[MINE.to_string()], &scan()));
        assert_eq!(records.len(), 1, "got {records:?}");
        assert_eq!(records[0]["insertions"], json!(3));
    }

    #[test]
    fn a_commit_message_never_reaches_the_collected_record() {
        // The first defence, and the one that matters: `git log` is never asked for the
        // message, so it is not in the bytes cclog archives either -- not only absent
        // from the observation the adapter builds from them.
        let home = TempHome::new("message");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "a.txt", 2, "SYNTHETIC-CANARY do not store me");

        let RepositoryScan::Collected { records, .. } =
            collect(&path, IDENTITY, &[MINE.to_string()], &scan())
        else {
            panic!("expected collected commits");
        };
        let joined = records.join("\n");
        assert!(
            !joined.contains("SYNTHETIC-CANARY"),
            "the commit message reached the snapshot: {joined}"
        );
        assert!(
            !joined.contains(MINE) && !joined.contains("Synthetic Author"),
            "an author reached the snapshot: {joined}"
        );
    }

    #[test]
    fn a_records_fields_are_the_shape_the_adapter_reads() {
        let home = TempHome::new("shape");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "a.txt", 7, "SYNTHETIC first");

        let records = records(&collect(&path, IDENTITY, &[MINE.to_string()], &scan()));
        assert_eq!(records.len(), 1);
        let mut keys: Vec<&str> = records[0]
            .as_object()
            .expect("a record is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "author_time",
                "commit",
                "deletions",
                "files_changed",
                "insertions",
                "repository"
            ]
        );
        assert_eq!(records[0]["repository"], json!(IDENTITY));
        assert_eq!(records[0]["files_changed"], json!(1));
        assert_eq!(records[0]["insertions"], json!(7));
        assert_eq!(records[0]["deletions"], json!(0));
        let sha = records[0]["commit"].as_str().expect("a sha");
        assert_eq!(sha.len(), 40, "the full sha, so the pseudonym is stable");
    }

    #[test]
    fn a_merge_commit_is_excluded_rather_than_claiming_it_changed_nothing() {
        let home = TempHome::new("merge");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "base.txt", 1, "SYNTHETIC base");
        git(&path, &["checkout", "--quiet", "-b", "feature"]);
        commit_as(&path, MINE, "feature.txt", 2, "SYNTHETIC feature");
        git(&path, &["checkout", "--quiet", "main"]);
        commit_as(&path, MINE, "main.txt", 3, "SYNTHETIC main");
        git(
            &path,
            &[
                "-c",
                &format!("user.email={MINE}"),
                "-c",
                "user.name=Synthetic Author",
                "merge",
                "--quiet",
                "--no-ff",
                "--no-verify",
                "-m",
                "SYNTHETIC merge",
                "feature",
            ],
        );

        let records = records(&collect(&path, IDENTITY, &[MINE.to_string()], &scan()));
        assert_eq!(
            records.len(),
            3,
            "the three non-merge commits, and not the merge: {records:?}"
        );
        for record in &records {
            assert!(
                record["files_changed"].as_u64().expect("files_changed") > 0,
                "no collected commit claims it changed nothing: {record:?}"
            );
        }
    }

    #[test]
    fn an_empty_commit_reports_the_zero_it_measured() {
        let home = TempHome::new("empty-commit");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "a.txt", 1, "SYNTHETIC first");
        git(
            &path,
            &[
                "-c",
                &format!("user.email={MINE}"),
                "-c",
                "user.name=Synthetic Author",
                "commit",
                "--quiet",
                "--no-verify",
                "--allow-empty",
                "-m",
                "SYNTHETIC empty",
            ],
        );

        let records = records(&collect(&path, IDENTITY, &[MINE.to_string()], &scan()));
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[1]["files_changed"],
            json!(0),
            "git prints no diffstat for an empty commit, and zero is what it measured"
        );
    }

    #[test]
    fn commits_outside_the_window_are_not_collected() {
        let home = TempHome::new("window");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "a.txt", 1, "SYNTHETIC first");

        // Both halves, so "the window excluded it" cannot be confused with "nothing was
        // collected at all". The fixture's commit dates are fixed and in the past (see
        // `git`), so which side of a one-day window they fall on does not depend on when
        // this runs.
        let wide = records(&collect(&path, IDENTITY, &[MINE.to_string()], &scan()));
        assert_eq!(wide.len(), 1, "the commit is collectable at all");

        let narrow = Scan {
            since_days: 1,
            ..scan()
        };
        let RepositoryScan::Collected { records, .. } =
            collect(&path, IDENTITY, &[MINE.to_string()], &narrow)
        else {
            panic!("expected a collected (if empty) result");
        };
        assert!(
            records.is_empty(),
            "the window bounds the walk; got {records:?}"
        );
    }

    #[test]
    fn hitting_the_commit_ceiling_keeps_the_newest_commits_and_says_it_did() {
        let home = TempHome::new("ceiling");
        let path = init_repository(home.path(), IDENTITY, MINE);
        for _ in 0..4 {
            commit_as(&path, MINE, "a.txt", 1, "SYNTHETIC one of many");
            commit_as(&path, MINE, "a.txt", 2, "SYNTHETIC one of many");
        }
        let all = records(&collect(&path, IDENTITY, &[MINE.to_string()], &scan()));
        assert_eq!(all.len(), 8, "the whole history is collectable at all");
        assert!(
            matches!(
                collect(&path, IDENTITY, &[MINE.to_string()], &scan()),
                RepositoryScan::Collected {
                    truncated: false,
                    ..
                }
            ),
            "a history that fits under the ceiling is not truncated"
        );

        let capped = Scan {
            max_commits: 3,
            ..scan()
        };
        let result = collect(&path, IDENTITY, &[MINE.to_string()], &capped);
        assert!(
            matches!(
                result,
                RepositoryScan::Collected {
                    truncated: true,
                    ..
                }
            ),
            "got {result:?}"
        );
        let kept_shas: Vec<String> = records(&result)
            .iter()
            .map(|r| r["commit"].as_str().expect("a sha").to_string())
            .collect();
        let newest_shas: Vec<String> = all[all.len() - 3..]
            .iter()
            .map(|r| r["commit"].as_str().expect("a sha").to_string())
            .collect();
        assert_eq!(
            kept_shas, newest_shas,
            "the ceiling must drop the oldest commits, not the newest"
        );
    }

    #[test]
    fn an_invocation_that_outlives_its_timeout_is_killed_and_reported() {
        // The failure this bounds -- a repository on a stalled network mount, a `git`
        // waiting on a credential prompt -- cannot be reproduced in a test, so the bound
        // itself is exercised instead, against a `git` that is genuinely still running
        // when the deadline passes. `run` is the code path every invocation in this
        // module goes through.
        let home = TempHome::new("timeout");
        let path = init_repository(home.path(), IDENTITY, MINE);
        commit_as(&path, MINE, "a.txt", 1, "SYNTHETIC first");
        let brief = Scan {
            timeout: Duration::from_millis(200),
            ..scan()
        };

        let started = Instant::now();
        let result = run(&path, &["-c", "alias.wait=!sleep 5", "wait"], &brief);
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(Failure::TimedOut)),
            "a git that outran its deadline must be reported as timed out, not as \
             an ordinary failure or an empty success"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the deadline did not hold: {elapsed:?}"
        );
    }

    #[test]
    fn an_identity_that_would_walk_out_of_the_ghq_tree_resolves_to_nothing() {
        let home = TempHome::new("traversal");
        assert_eq!(repository_path(home.path(), "github.com/../etc"), None);
        assert_eq!(repository_path(home.path(), "github.com/acme"), None);
        assert_eq!(repository_path(home.path(), "a/b/c/d"), None);
        assert_eq!(repository_path(home.path(), "github.com//api"), None);
        assert_eq!(
            repository_path(home.path(), IDENTITY),
            Some(home.path().join("ghq/github.com/acme/api"))
        );
    }

    #[test]
    fn a_diffstat_git_prints_differently_is_a_missing_measurement_not_a_zero() {
        // The regression this guards is silent: a git whose summary line this cannot
        // read would otherwise make every commit in the ledger claim it changed nothing.
        let unreadable = "0123456789abcdef0123456789abcdef01234567\t2026-07-20T03:20:05+00:00\n\
                          \x201 Datei geändert, 3 Zeilen hinzugefügt(+)\n";
        let records = parse_log(unreadable, IDENTITY);
        assert_eq!(records.len(), 1);
        let record: Value = serde_json::from_str(&records[0]).expect("JSON");
        assert!(
            record.get("files_changed").is_none(),
            "an unreadable diffstat must leave the count absent: {record:?}"
        );
    }

    #[test]
    fn a_sha256_repositorys_commits_are_read_rather_than_silently_dropped() {
        // A 64-character object name is a real git repository shape
        // (`--object-format=sha256`). A header this could not read would fall through to
        // the diffstat branch and be discarded with no gap marker anywhere -- the whole
        // repository's history gone, reported as a repository with no commits of mine.
        let sha256 = "a".repeat(64);
        let log =
            format!("{sha256}\t2026-07-20T03:20:05+00:00\n\n 1 file changed, 2 insertions(+)\n");
        let records = parse_log(&log, IDENTITY);
        assert_eq!(records.len(), 1, "got {records:?}");
        let record: Value = serde_json::from_str(&records[0]).expect("JSON");
        assert_eq!(record["commit"], json!(sha256));
        assert_eq!(record["files_changed"], json!(1));
    }

    #[test]
    fn parse_shortstat_reads_every_form_git_prints_and_rejects_the_rest() {
        let both = parse_shortstat(" 4 files changed, 137 insertions(+), 22 deletions(-)")
            .expect("a full summary");
        assert_eq!((both.files, both.insertions, both.deletions), (4, 137, 22));

        // git omits a clause that is zero, so its absence is a measured zero.
        let added = parse_shortstat(" 1 file changed, 2 insertions(+)").expect("no deletions");
        assert_eq!((added.files, added.insertions, added.deletions), (1, 2, 0));
        let removed = parse_shortstat(" 1 file changed, 2 deletions(-)").expect("no insertions");
        assert_eq!(
            (removed.files, removed.insertions, removed.deletions),
            (1, 0, 2)
        );
        let renamed = parse_shortstat(" 1 file changed").expect("neither");
        assert_eq!(
            (renamed.files, renamed.insertions, renamed.deletions),
            (1, 0, 0)
        );

        assert!(parse_shortstat("commit 0123456789").is_none());
        assert!(parse_shortstat(" 3 wombats changed").is_none());
        assert!(parse_shortstat(" 2 insertions(+)").is_none());
        assert!(parse_shortstat("").is_none());
    }
}
