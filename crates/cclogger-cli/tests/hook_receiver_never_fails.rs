//! `cclogger hook` must never fail the session it is recording.
//!
//! These drive the built binary rather than calling `hook::run_hook`, because the thing
//! under test is the **process exit code** Claude Code reads. Claude Code runs command
//! hooks synchronously; on `PreToolUse` and `UserPromptSubmit` a hook that exits 2 blocks
//! the action, and any non-zero exit is reported as a hook error. A unit test can assert
//! that one function returns `0`; only a process can assert that nothing between `main`
//! and that function -- argument parsing, an unset variable, a refusal belonging to some
//! other subcommand -- turns it into anything else.
//!
//! The last test is the load-bearing one. `report` and `log` now resolve this machine's
//! UTC offset and **refuse** when they cannot read it, which is right for a command that
//! slices a day and would otherwise slice it somewhere arbitrary. The receiver slices
//! nothing -- it stamps a UTC instant -- so that refusal must not reach it. Today it
//! cannot, because the offset is resolved inside the `Report`/`Log` arms; hoisting that
//! into `main` would be an easy and entirely reasonable-looking refactor, and would turn
//! every hook invocation on a machine without `date` into a silent no-op.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// A cclog root under the system temp directory, removed when the test ends. Never the
/// real `~/.cclog`: nothing here may read or write a person's own ledger, and the
/// receiver's whole job is to write.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cclog-hook-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn spool(&self) -> String {
        std::fs::read_to_string(self.0.join("spool/hooks.jsonl")).unwrap_or_default()
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `PreToolUse` payload in the documented shape. Every value is synthetic.
const PAYLOAD: &str = r#"{
  "session_id": "SYNTHETIC-session-1",
  "prompt_id": "SYNTHETIC-prompt-1",
  "cwd": "/SYNTHETIC/work",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_use_id": "SYNTHETIC-toolu-1",
  "tool_input": { "command": "echo SYNTHETICSECRET" }
}"#;

/// Run the built binary with `stdin`, and hand back the exit code.
///
/// `envs` is applied on top of the inherited environment, so a test states only what it
/// is changing.
fn run(args: &[&Path], stdin: &str, envs: &[(&str, &Path)]) -> Option<i32> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cclogger"));
    for arg in args {
        command.arg(arg);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the built cclogger runs");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(stdin.as_bytes())
        .expect("the receiver reads its payload");
    child.wait().expect("the receiver exits").code()
}

fn hook(root: &TempRoot, stdin: &str, envs: &[(&str, &Path)]) -> Option<i32> {
    run(
        &[Path::new("hook"), Path::new("--cclog-root"), root.path()],
        stdin,
        envs,
    )
}

#[test]
fn the_receiver_exits_zero_on_a_payload_it_can_make_nothing_of() {
    let root = TempRoot::new("malformed");
    for payload in ["", "not json at all", "[1,2,3]", "{", "\u{feff}{}"] {
        assert_eq!(
            hook(&root, payload, &[]),
            Some(0),
            "payload {payload:?} must not fail the session"
        );
    }
    assert_eq!(
        root.spool().lines().count(),
        5,
        "and each one leaves a trace rather than vanishing: {:?}",
        root.spool()
    );
}

#[test]
fn the_receiver_exits_zero_on_a_payload_it_understands() {
    // The mirror of the test above: a receiver that failed on *everything* would pass
    // that one, and record nothing anybody wanted.
    let root = TempRoot::new("well-formed");
    assert_eq!(hook(&root, PAYLOAD, &[]), Some(0));
    let spool = root.spool();
    assert!(
        spool.contains("\"hook_event_name\":\"PreToolUse\""),
        "the event must be recorded: {spool:?}"
    );
    assert!(
        !spool.contains("SYNTHETICSECRET"),
        "and the command it was about must not be: {spool:?}"
    );
}

#[test]
fn the_receiver_records_where_report_refuses_for_want_of_a_machine_offset() {
    // With no `date` on `PATH`, `tz::detect` cannot read this machine's UTC offset, and
    // `report` refuses rather than cutting the day at a guessed one. The receiver must
    // be untouched by that: it stamps a UTC instant and buckets nothing, and it may not
    // acquire a way to fail that belongs to a command doing a different job.
    let root = TempRoot::new("no-offset");
    let empty = root.path().join("nothing-on-this-path");
    std::fs::create_dir_all(&empty).expect("an empty directory to use as PATH");

    assert_eq!(
        hook(&root, PAYLOAD, &[("PATH", &empty)]),
        Some(0),
        "the receiver does not consult the timezone and must not fail for want of one"
    );
    assert!(
        root.spool().contains("\"hook_event_name\":\"PreToolUse\""),
        "and it records rather than merely exiting quietly: {:?}",
        root.spool()
    );

    // The other half, or this would pass on any machine where `date` resolves anyway and
    // prove nothing at all: the same environment really does stop `report`.
    let refused = run(
        &[
            Path::new("report"),
            Path::new("--cclog-root"),
            root.path(),
            Path::new("--home"),
            root.path(),
        ],
        "",
        &[("PATH", &empty)],
    );
    assert_ne!(
        refused,
        Some(0),
        "`report` must still refuse an offset it cannot read -- if it stopped, this test \
         no longer distinguishes the receiver from it"
    );
}

#[test]
fn the_receiver_exits_zero_with_nowhere_to_write_rather_than_reporting_an_error() {
    // No `--cclog-root` and no `HOME`: the spool path cannot be computed, so nothing is
    // recorded. Nothing is interrupted either, which is the trade -- a telemetry tool
    // that cannot record has lost a line, and one that exits non-zero has interrupted
    // someone's work.
    let mut command = Command::new(env!("CARGO_BIN_EXE_cclogger"));
    let mut child = command
        .arg("hook")
        .env_remove("HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the built cclogger runs");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(PAYLOAD.as_bytes())
        .expect("write the payload");
    assert_eq!(
        child.wait().expect("the receiver exits").code(),
        Some(0),
        "an unresolvable root is a lost line, never a failed session"
    );
}
