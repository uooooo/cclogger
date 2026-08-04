//! Which UTC offset `report` and `log` cut a day at when nothing names one.
//!
//! These drive the built binary rather than calling a function, because the thing under
//! test is the *default*: what the tool picks when `--tz-offset-hours` is absent. That is
//! a property of the process's environment, and only a process has one.
//!
//! Every expectation below is derived from a `TZ` the test itself sets, never from the
//! machine the test runs on. That distinction is the whole point: an assertion that the
//! offset is `+09:00` "because that is where this laptop is" passes identically against a
//! hardcoded `9`, which is precisely the defect these tests exist to keep fixed. The `TZ`
//! values are POSIX offset specifications (`XXX-5:30` means UTC+05:30 -- POSIX states the
//! offset as the value *added to local time to reach UTC*, so its sign is inverted), which
//! every libc resolves without a tz database installed.
//!
//! The header carries both halves of the claim, and both are asserted: the offset it says
//! it used, and the UTC instants it actually cut the day at. A tool that printed the right
//! offset over a window sliced somewhere else would pass the first alone.

use cclogger_archive::Ledger;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A cclog root under the system temp directory, removed when the test ends. Never the
/// real `~/.cclog`: nothing here may read or write a person's own ledger.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cclog-tz-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        // An empty ledger is enough: `report` refuses to run without one (an empty report
        // for a missing ledger reads exactly like a day with no work), and every line this
        // file asserts on is the header, which is printed before any observation is.
        Ledger::open(&path).expect("a ledger can be created under a temp root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The first line the command prints -- the header, which names the offset it bucketed
/// the day in and the UTC window that offset resolved to.
fn header(root: &TempRoot, tz: &str, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_cclogger"))
        .args(args)
        .arg("--home")
        .arg(root.path())
        .arg("--cclog-root")
        .arg(root.path())
        .env("TZ", tz)
        .output()
        .expect("the built cclogger runs");
    assert!(
        output.status.success(),
        "TZ={tz} {args:?} exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .expect("the header is the first line of every report")
        .to_string()
}

#[test]
fn the_default_offset_is_the_machine_the_command_runs_on_not_one_zone_written_down() {
    let root = TempRoot::new("default");

    // Three zones, three answers. Any implementation that returns a constant -- the `9`
    // this replaced, or a `0` chosen as a safer-looking constant -- gives one answer to
    // all three and fails here regardless of where the test is run.
    let utc = header(&root, "UTC0", &["report"]);
    let kolkata = header(&root, "XXX-5:30", &["report"]);
    let west = header(&root, "XXX3", &["report"]);

    assert!(utc.contains("UTC+00:00"), "TZ=UTC0 is UTC+00:00: {utc:?}");
    assert!(
        kolkata.contains("UTC+05:30"),
        "a zone half an hour off the hour must print as one, not round to +05:00 or +06:00: \
         {kolkata:?}"
    );
    assert!(
        west.contains("UTC-03:00"),
        "a zone west of Greenwich keeps its sign: {west:?}"
    );

    // And the day was actually cut there, rather than merely labelled. Local midnight in
    // UTC+05:30 is 18:30Z the previous day; in UTC-03:00 it is 03:00Z the same day.
    assert!(
        utc.contains("T00:00:00Z .. ") && utc.ends_with("T00:00:00Z)"),
        "UTC+00:00 cuts the day at midnight UTC: {utc:?}"
    );
    assert!(
        kolkata.contains("T18:30:00Z .. ") && kolkata.ends_with("T18:30:00Z)"),
        "UTC+05:30 cuts the day at 18:30Z, and a whole-hour offset cannot: {kolkata:?}"
    );
    assert!(
        west.contains("T03:00:00Z .. ") && west.ends_with("T03:00:00Z)"),
        "UTC-03:00 cuts the day at 03:00Z: {west:?}"
    );
}

#[test]
fn log_defaults_to_the_same_offset_report_does() {
    // Two flags, two default sites: `log` bucketing "today" differently from `report`
    // would put the same work on two different days depending on which was asked.
    let root = TempRoot::new("log");

    for (tz, expected) in [
        ("UTC0", "UTC+00:00"),
        ("XXX-5:30", "UTC+05:30"),
        ("XXX3", "UTC-03:00"),
    ] {
        let reported = header(&root, tz, &["report"]);
        let logged = header(&root, tz, &["log"]);
        assert!(
            logged.contains(expected),
            "TZ={tz}: log must bucket at {expected}: {logged:?}"
        );
        // Same day, same offset, same window -- not merely the same offset text.
        assert_eq!(
            reported, logged,
            "TZ={tz}: report and log must resolve one day to one window"
        );
    }
}

#[test]
fn the_flag_still_overrides_the_machine_and_now_reaches_zones_off_the_hour() {
    let root = TempRoot::new("override");

    // Given explicitly, the flag wins over the environment -- that is what it is for.
    let given = header(&root, "UTC0", &["report", "--tz-offset-hours", "9"]);
    assert!(
        given.contains("UTC+09:00"),
        "an explicit offset overrides the machine's: {given:?}"
    );

    // Negative whole hours, unchanged from before this fix.
    let negative = header(&root, "UTC0", &["report", "--tz-offset-hours", "-7"]);
    assert!(
        negative.contains("UTC-07:00"),
        "a negative whole-hour offset is still accepted: {negative:?}"
    );

    // And the forms `i32` hours could not express at all. India, Nepal, the parts of
    // Australia on a 45-minute offset, and Newfoundland west of Greenwich.
    for (value, expected) in [
        ("5:30", "UTC+05:30"),
        ("+05:45", "UTC+05:45"),
        ("8:45", "UTC+08:45"),
        ("-3:30", "UTC-03:30"),
    ] {
        let line = header(&root, "UTC0", &["report", "--tz-offset-hours", value]);
        assert!(
            line.contains(expected),
            "--tz-offset-hours {value} must bucket at {expected}: {line:?}"
        );
    }
}
