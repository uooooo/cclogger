//! The hook receiver: one Claude Code hook invocation → one spool line.
//!
//! This runs **inside the user's editing loop**. Claude Code invokes a command hook
//! synchronously and waits for it, and on `PreToolUse` / `UserPromptSubmit` a hook that
//! exits 2 *blocks* the action. So this module is written to two rules that override
//! every other consideration:
//!
//! 1. **Do the minimum.** Read stdin, project the payload onto a fixed field allowlist,
//!    append one line, exit. It never opens SQLite, never stats the archive, never
//!    resolves a repository, never touches the network. The only syscalls on the happy
//!    path are one read of stdin, one `open(O_APPEND|O_CREAT)`, one `write`.
//! 2. **Never fail the session.** [`run_hook`] returns `0` on every internal error --
//!    unwritable spool, malformed payload, missing directory, anything. A telemetry
//!    tool must not be able to interrupt someone's work.
//!
//! Rule 2 without rule 3 would just be silence, so:
//!
//! 3. **A swallowed error leaves a trace.** A payload that is not a JSON object still
//!    produces a spool line -- one carrying `receiver_error` instead of an event -- and
//!    the importer turns that into a `dev.cclog.source.gap.v1` marker with the same
//!    reason. When even the spool cannot be written, the reason goes to a sibling
//!    [`ERROR_LOG_NAME`] beside it. Only if *that* fails too does anything go to
//!    stderr, which Claude Code shows in its debug log.
//!
//! # Arrival time, not event time
//!
//! **No hook event carries a timestamp.** The reference is explicit: "No global
//! `timestamp` field exists in the standard hook input schema." The receiver's own
//! clock is therefore the only time source, and what it records is when *cclog* saw the
//! event, not when the event happened. The spool says so by name -- the field is
//! `received_at`, never `ts` or `timestamp` -- and every observation derived from it
//! carries `data.time_basis = "received_at"`, joining the three bases this codebase
//! already distinguishes (`occurred_at`, `acquired_at`, `copied_at`).
//!
//! The gap between the two is the time Claude Code takes to spawn this process, which
//! is milliseconds. That is why `received_at` is admitted onto clocks where
//! `acquired_at` and `copied_at` are not (see `report::time_is_when_it_happened`): it
//! is a measurement of the event's instant with a small, bounded, stated error, rather
//! than a different event's time standing in for one that was never measured.
//!
//! # The spool holds metadata, never content
//!
//! `UserPromptSubmit` carries `prompt_text`; `Stop` carries `last_assistant_message`;
//! `PreToolUse`/`PostToolUse` carry `tool_input` and `tool_response`, which for an
//! `Edit` is the file being written and for a `Bash` is the command and its output.
//! **None of that is written to the spool at all**, not even transiently: the receiver
//! copies a fixed [`COPIED_STRINGS`] / [`COPIED_INTEGERS`] allowlist out of the payload
//! and drops the rest, so there is never a moment at which a file on this disk holds
//! prompt text. That is a deliberate choice over the cheaper alternative (append the
//! raw payload, filter at import): it costs one small JSON parse per hook and removes
//! the question of how long a file of prompts may live entirely, rather than answering
//! it with a retention policy. It also keeps the hot path *faster* in the common case,
//! since a `Write` tool's `tool_input` can be megabytes and the projected line is a few
//! hundred bytes.
//!
//! What does reach the spool is still owner-only (0700 directory, 0600 file): `cwd` is
//! a real filesystem path and contains the username, exactly as the archive's locators
//! already do (see `discover`'s module comment).

use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(crate) const SPOOL_DIR_NAME: &str = "spool";
pub(crate) const SPOOL_FILE_NAME: &str = "hooks.jsonl";
/// Where the spool sits relative to the cclog root. Also the `source_locator` its
/// snapshots and its import checkpoint are recorded under, so the two can never name
/// different files.
pub(crate) const SPOOL_LOCATOR: &str = "spool/hooks.jsonl";
/// The `source_kind` the spool's snapshots and checkpoint are stored under. Distinct
/// from `claude-code` (the transcript channel) so the two channels keep separate
/// checkpoints and can be told apart in coverage.
pub(crate) const SPOOL_SOURCE_KIND: &str = "claude-code-hook";
pub(crate) const ERROR_LOG_NAME: &str = "hook-receiver-errors.log";

/// The spool line format's own version, so a later receiver can change the shape
/// without the importer having to guess which shape it is reading.
pub(crate) const SPOOL_FORMAT: u64 = 1;

/// The arrival stamp's field name on a spool line. Deliberately not `ts` or
/// `timestamp`: it is when the receiver saw the event, and the name has to say so
/// wherever the line is read -- including by a human running `tail` on the spool.
pub(crate) const RECEIVED_AT: &str = "received_at";

/// The field a spool line carries instead of an event when the receiver could not make
/// one. Its value is one of [`RECEIVER_ERRORS`].
pub(crate) const RECEIVER_ERROR: &str = "receiver_error";

/// Every reason this receiver can decline to record an event, spelled as a gap
/// `reason` (`^[a-z0-9_]+$`, which the canonical schema requires). Fixed strings, never
/// text derived from the payload: this is the one field on a gap marker that a
/// malformed payload could otherwise steer.
pub(crate) const RECEIVER_ERRORS: &[&str] = &[
    "payload_unreadable",
    "payload_not_an_object",
    "payload_oversize",
];

/// String fields copied verbatim from the hook payload onto the spool line.
///
/// An **allowlist, never a denylist**: a field Claude Code adds in a future version is
/// dropped by default rather than carried by default, which is the only way round that
/// fails safe. Every entry here is identity or coarse metadata --
///
/// - `hook_event_name`, `session_id`, `cwd`, `prompt_id`, `agent_id`, `agent_type` are
///   the documented common fields (`prompt_id` is common, not `UserPromptSubmit`-only,
///   so a tool event can be placed in the turn it belongs to; `agent_id`/`agent_type`
///   appear only when the hook fires inside a subagent, which is how this channel can
///   tell subagent work apart at all);
/// - `tool_use_id` and `tool_name` identify and classify a tool call;
/// - `reason` is `SessionEnd`'s termination label.
///
/// Deliberately absent: `prompt_text`, `last_assistant_message`, `tool_input`,
/// `tool_response` (all content), and `transcript_path` (a path this tool has no use
/// for -- import reads the spool, not the transcript the payload points at).
const COPIED_STRINGS: &[&str] = &[
    "agent_id",
    "agent_type",
    "cwd",
    "hook_event_name",
    "prompt_id",
    "reason",
    "session_id",
    "tool_name",
    "tool_use_id",
];

/// Integer fields copied verbatim. `PostToolUse.duration_ms` is the one this channel
/// exists to capture: the reference documents it as "Tool execution time in
/// milliseconds. Excludes time spent in permission prompts and PreToolUse hooks",
/// which is exactly the defect in the transcript path, where a tool "duration" reached
/// 42.8 hours because it included someone walking away from an approval prompt.
///
/// It is optional, so **absence stays absence**: a payload that carries no
/// `duration_ms` produces a line with no `duration_ms` key, never a `0`.
const COPIED_INTEGERS: &[&str] = &["duration_ms"];

/// The largest spool line this receiver will write, in bytes including the newline.
///
/// Concurrency, not thrift: hooks on the same event run in parallel and several
/// sessions can be active at once, so several processes append to this one file
/// simultaneously. Keeping every line to a single small `write(2)` on an `O_APPEND`
/// descriptor keeps appends from interleaving into each other. A line that would
/// exceed this is replaced by a bounded `payload_oversize` marker rather than written
/// long or dropped -- the loss is recorded, which is the rule the whole gap machinery
/// exists to enforce.
const MAX_LINE_BYTES: usize = 4096;

/// The largest stdin payload read. A `Write` tool's `tool_input` can be megabytes, and
/// none of it is copied, so reading it in full would be work done purely to throw away.
/// A payload longer than this is still parsed as far as it goes; if that truncation
/// makes it unparseable it becomes a `payload_unreadable` marker, which is the honest
/// outcome.
const MAX_PAYLOAD_BYTES: u64 = 1 << 20;

pub(crate) fn spool_path(root: &Path) -> PathBuf {
    root.join(SPOOL_DIR_NAME).join(SPOOL_FILE_NAME)
}

pub(crate) fn error_log_path(root: &Path) -> PathBuf {
    root.join(ERROR_LOG_NAME)
}

/// Receive one hook invocation. **Always returns 0.**
///
/// The return type is `u8` rather than `ExitCode` so a test can assert the value; `main`
/// converts. That a caller *cannot* get a non-zero code out of this function is the
/// point, and is pinned by `a_malformed_payload_still_exits_zero_and_leaves_a_trace`.
pub fn run_hook(root: &Path, input: impl Read, received_at: &str) -> u8 {
    let line = build_line(input, received_at);
    if let Err(e) = append(&spool_path(root), &line) {
        // The spool is the primary trace; this is the fallback one. Both can fail --
        // an unwritable cclog root, a full disk -- and then there is nothing left but
        // stderr, which Claude Code writes to its debug log.
        let note = format!("{received_at} spool append failed: {e}\n");
        if let Err(e2) = append(&error_log_path(root), &note) {
            eprintln!("cclogger hook: {e} (and the error log too: {e2})");
        }
    }
    0
}

/// The one line this invocation contributes, newline included.
///
/// Total: every path through it yields a line. A payload that cannot be read, is not
/// JSON, or is not a JSON object yields a `receiver_error` line rather than nothing,
/// because "the receiver ran and could make nothing of this" is itself evidence the
/// user should be able to find later.
fn build_line(input: impl Read, received_at: &str) -> String {
    let mut out = Map::new();
    out.insert("v".to_string(), Value::from(SPOOL_FORMAT));
    out.insert(RECEIVED_AT.to_string(), Value::from(received_at));

    let mut raw = String::new();
    if input
        .take(MAX_PAYLOAD_BYTES)
        .read_to_string(&mut raw)
        .is_err()
    {
        return finish(out, Some("payload_unreadable"));
    }
    let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&raw) else {
        // Covers empty stdin, truncated JSON, and a well-formed JSON array or string:
        // all are payloads this receiver cannot route, and all are said the same way.
        return finish(out, Some("payload_not_an_object"));
    };

    for field in COPIED_STRINGS {
        if let Some(s) = payload.get(*field).and_then(Value::as_str) {
            out.insert((*field).to_string(), Value::from(s));
        }
    }
    for field in COPIED_INTEGERS {
        // `as_i64` and not `as_f64`: a duration is a count of milliseconds. A payload
        // sending `1.5` is not carrying a measurement this field can hold, and dropping
        // it leaves the field absent, which is how "not measured" is said here.
        if let Some(n) = payload.get(*field).and_then(Value::as_i64) {
            out.insert((*field).to_string(), Value::from(n));
        }
    }
    finish(out, None)
}

/// Serialize, and swap in a bounded loss marker if the result is too long to append
/// atomically. See [`MAX_LINE_BYTES`].
fn finish(mut out: Map<String, Value>, error: Option<&'static str>) -> String {
    if let Some(reason) = error {
        out.insert(RECEIVER_ERROR.to_string(), Value::from(reason));
    }
    let line = format!("{}\n", Value::Object(out.clone()));
    if line.len() <= MAX_LINE_BYTES {
        return line;
    }
    // Keep the event name if it is itself short -- it is what makes the marker
    // diagnosable -- and drop everything else, including whichever field was long.
    let mut marker = Map::new();
    marker.insert("v".to_string(), Value::from(SPOOL_FORMAT));
    marker.insert(
        RECEIVED_AT.to_string(),
        out.get(RECEIVED_AT).cloned().unwrap_or(Value::Null),
    );
    if let Some(name) = out.get("hook_event_name").and_then(Value::as_str)
        && name.len() <= 64
    {
        marker.insert("hook_event_name".to_string(), Value::from(name));
    }
    marker.insert(RECEIVER_ERROR.to_string(), Value::from("payload_oversize"));
    format!("{}\n", Value::Object(marker))
}

/// Append `line` to `path`, creating the file (0600) and its parent (0700) if needed.
///
/// One `write_all` of one buffer on an `O_APPEND` descriptor: the kernel resolves the
/// offset and the write together, so concurrent hook processes cannot interleave into
/// each other's lines.
///
/// The parent directory is created only after an open fails with `NotFound`, so the
/// steady-state hot path is a single `open`. It is *not* re-chmodded when it already
/// exists: tightening it on every invocation would be a syscall per hook to fix a
/// condition that cannot arise from anything this program does.
fn append(path: &Path, line: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true).mode(0o600);
    let file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)?;
            }
            opts.open(path)?
        }
        Err(e) => return Err(e),
    };
    let mut file = file;
    file.write_all(line.as_bytes())
}

// -- installation ----------------------------------------------------------------

/// The hook events this channel records. Anything else reaching the spool is an
/// unmapped kind, which the importer diagnoses as a gap rather than dropping.
///
/// `SubagentStop` is deliberately absent. Claude Code converts a subagent's `Stop`
/// into `SubagentStop`, so registering both would put two `response.completed`
/// observations on one turn -- the subagent's and the main loop's -- and a turn count
/// built on that would be wrong in the direction that is hardest to notice.
pub(crate) const REGISTERED_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
];

/// What `cclogger hook-install` prints.
///
/// **It prints; it does not write.** Two reasons, both decisive on their own. The hook
/// registry lives in `~/.claude/settings.json`, a file this project has a standing rule
/// never to write -- and the merge is not a small one: `hooks` is a map of event name to
/// an array of matcher groups, each holding an array of handlers, so a safe merge has to
/// find this project's own entry inside a structure the user may also be using for
/// unrelated hooks, without duplicating it and without disturbing theirs. Getting that
/// wrong silently breaks somebody's editor. Printing puts the diff in front of the
/// person who owns the file.
///
/// Pasting it twice is harmless anyway: Claude Code deduplicates hook handlers by
/// command string, so a second identical entry runs once.
pub(crate) fn install_instructions(binary: &Path) -> String {
    // The one value that varies is a filesystem path, and a path may hold a quote or a
    // backslash, so it is escaped by `serde_json` rather than pasted into a string --
    // a settings block that does not parse takes every one of the user's own hooks down
    // with it. The layout around it is hand-written because `to_string_pretty` would
    // spread each three-line entry over nine, and this is something a person has to
    // read and paste.
    //
    // No `matcher` key: it is unsupported on `Stop` and `UserPromptSubmit`, and
    // omitting it means "match everything" on the events that do support one -- which
    // is what this channel wants everywhere.
    let handlers = serde_json::to_string(&serde_json::json!([{
        "hooks": [{ "type": "command", "command": format!("{} hook", binary.display()) }]
    }]))
    .unwrap_or_else(|_| "[]".to_string());
    let entries: Vec<String> = REGISTERED_EVENTS
        .iter()
        .map(|event| {
            let name = Value::from(*event);
            format!("    {name}: {handlers}")
        })
        .collect();
    let block = format!("{{\n  \"hooks\": {{\n{}\n  }}\n}}", entries.join(",\n"));
    // A line per line, joined -- not one `format!` with `\`-continuations, which strip
    // the leading whitespace of the line they continue and would silently flatten the
    // indentation of every wrapped bullet below.
    let lines = [
        "Add this to the \"hooks\" object in ~/.claude/settings.json (or",
        ".claude/settings.json in one project, to record only that project):",
        "",
        &block,
        "",
        "cclogger does not write that file for you. Its \"hooks\" object is a map of",
        "event name to matcher groups, and merging into one you may already be using",
        "means editing around your entries -- so the merge is yours to make, and to see.",
        "Pasting this twice is harmless: Claude Code deduplicates handlers by command.",
        "",
        "Then run `cclogger import` as usual; it drains the spool alongside the archive.",
        "",
        "What this records, and what it does not:",
        "",
        "  - Hooks start recording when you install them. Nothing before this moment is",
        "    recoverable from them -- Claude Code buffers nothing and replays nothing --",
        "    so `cclogger archive` and `cclogger import` stay exactly as necessary as",
        "    they were for every day that already happened.",
        "  - Turn completion is not exhaustive. `Stop` does not fire when you interrupt",
        "    a turn; an API error fires `StopFailure` instead, and a killed process fires",
        "    nothing at all. A prompt with no matching completion is normal.",
        "  - Only metadata is written. Your prompts, the model's replies, tool arguments",
        "    and tool output are dropped by the receiver and never reach the spool.",
    ];
    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::tests::TempRoot;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    const AT: &str = "2026-08-04T02:11:33.412Z";

    /// A `PreToolUse` payload in the documented shape, carrying exactly the content a
    /// metadata-only ledger must never see. Every value is synthetic.
    fn pre_tool_use() -> Value {
        json!({
            "session_id": "SYNTHETIC-session-1",
            "prompt_id": "SYNTHETIC-prompt-1",
            "transcript_path": "/SYNTHETIC/home/.claude/projects/p/s.jsonl",
            "cwd": "/SYNTHETIC/home/ghq/github.com/acme/api",
            "hook_event_name": "PreToolUse",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_use_id": "SYNTHETIC-toolu-1",
            "tool_input": { "command": "echo SYNTHETICSECRET", "description": "SYNTHETICSECRET" },
        })
    }

    fn post_tool_use() -> Value {
        json!({
            "session_id": "SYNTHETIC-session-1",
            "prompt_id": "SYNTHETIC-prompt-1",
            "cwd": "/SYNTHETIC/home/ghq/github.com/acme/api",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "SYNTHETIC-toolu-1",
            "tool_input": { "command": "echo SYNTHETICSECRET" },
            "tool_response": "SYNTHETICSECRET",
        })
    }

    fn receive(root: &Path, payload: &str) -> u8 {
        run_hook(root, payload.as_bytes(), AT)
    }

    fn spool_lines(root: &Path) -> Vec<Value> {
        let raw = std::fs::read_to_string(spool_path(root)).unwrap_or_default();
        raw.lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("spool line {l:?}: {e}")))
            .collect()
    }

    #[test]
    fn a_malformed_payload_still_exits_zero_and_leaves_a_trace() {
        // Claude Code invokes command hooks synchronously and treats exit 2 as a block
        // on `PreToolUse` and `UserPromptSubmit`. A telemetry tool must never be able
        // to interrupt someone's work, so every internal failure exits 0 -- and says so
        // in the spool rather than vanishing.
        let root = TempRoot::new("hook-malformed");
        for payload in ["", "not json at all", "[1,2,3]", "\"a bare string\"", "{"] {
            assert_eq!(
                receive(root.path(), payload),
                0,
                "payload {payload:?} must not fail the session"
            );
        }
        let lines = spool_lines(root.path());
        assert_eq!(
            lines.len(),
            5,
            "each swallowed error must leave a trace, not silence"
        );
        for line in &lines {
            assert_eq!(
                line[RECEIVER_ERROR], "payload_not_an_object",
                "the trace must name what went wrong: {line}"
            );
            assert_eq!(line[RECEIVED_AT], AT);
        }
    }

    #[test]
    fn an_unwritable_spool_still_exits_zero_and_leaves_a_sibling_error_file() {
        // Not root-safe by design: root ignores permission bits, so the append would
        // succeed and this would pass for the wrong reason -- the same known limitation
        // `main.rs`'s unreadable-file test documents.
        let root = TempRoot::new("hook-unwritable");
        let dir = root.path().join(SPOOL_DIR_NAME);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let code = receive(root.path(), &pre_tool_use().to_string());

        // Restore before asserting, so a failure still leaves a removable directory.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(code, 0, "an unwritable spool must not fail the session");

        let errors = std::fs::read_to_string(error_log_path(root.path()))
            .expect("a spool that cannot be written leaves a sibling error file");
        assert!(
            errors.contains(AT),
            "the error file must say when the loss happened: {errors}"
        );
    }

    #[test]
    fn the_spool_line_carries_only_allowlisted_metadata_and_never_the_prompt() {
        // The ledger is metadata-only, and the spool is the file that feeds
        // it. Filtering at import instead would mean a file of prompt text existed on
        // disk in between, however briefly; nothing here ever writes one.
        let root = TempRoot::new("hook-allowlist");
        let prompt = json!({
            "session_id": "SYNTHETIC-session-1",
            "cwd": "/SYNTHETIC/home/ghq/github.com/acme/api",
            "hook_event_name": "UserPromptSubmit",
            "prompt_id": "SYNTHETIC-prompt-1",
            // Both spellings: `prompt_text` is what the reference documents, `prompt`
            // is what an older or a future build might send. An allowlist drops either
            // without having to know which.
            "prompt_text": "SYNTHETICSECRET the human typed this",
            "prompt": "SYNTHETICSECRET the human typed this",
        });
        let stop = json!({
            "session_id": "SYNTHETIC-session-1",
            "hook_event_name": "Stop",
            "prompt_id": "SYNTHETIC-prompt-1",
            "last_assistant_message": "SYNTHETICSECRET the model replied this",
        });
        for payload in [&prompt, &stop, &pre_tool_use(), &post_tool_use()] {
            assert_eq!(receive(root.path(), &payload.to_string()), 0);
        }

        let raw = std::fs::read_to_string(spool_path(root.path())).unwrap();
        assert!(
            !raw.contains("SYNTHETICSECRET"),
            "prompt text, model output, command text and tool output must never reach \
             the spool: {raw}"
        );
        for banned in [
            "prompt_text",
            "last_assistant_message",
            "tool_input",
            "tool_response",
            "transcript_path",
            "permission_mode",
        ] {
            assert!(
                !raw.contains(banned),
                "{banned} is not on the allowlist and must be dropped: {raw}"
            );
        }

        // And the allowlisted metadata really is carried, or dropping everything would
        // pass the whole test above.
        let lines = spool_lines(root.path());
        assert_eq!(lines[0]["prompt_id"], "SYNTHETIC-prompt-1");
        assert_eq!(lines[0]["hook_event_name"], "UserPromptSubmit");
        assert_eq!(lines[2]["tool_name"], "Bash");
        assert_eq!(lines[2]["tool_use_id"], "SYNTHETIC-toolu-1");
        assert_eq!(lines[2]["cwd"], "/SYNTHETIC/home/ghq/github.com/acme/api");
    }

    #[test]
    fn a_missing_duration_stays_missing_rather_than_becoming_zero() {
        // `duration_ms` is documented optional. `0` is a real measurement of a tool
        // that returned within a millisecond, and this channel's durations are
        // aggregated with the transcript channel's, so a fabricated `0` here would be
        // indistinguishable from a measurement and would bias every aggregate down.
        let root = TempRoot::new("hook-duration");
        assert_eq!(receive(root.path(), &post_tool_use().to_string()), 0);

        let mut measured = post_tool_use();
        measured["duration_ms"] = json!(0);
        assert_eq!(receive(root.path(), &measured.to_string()), 0);

        let mut real = post_tool_use();
        real["duration_ms"] = json!(842);
        assert_eq!(receive(root.path(), &real.to_string()), 0);

        let lines = spool_lines(root.path());
        assert!(
            lines[0].get("duration_ms").is_none(),
            "a payload with no duration must not gain one: {}",
            lines[0]
        );
        assert_eq!(
            lines[1]["duration_ms"],
            json!(0),
            "a measured zero is a measurement and must survive"
        );
        assert_eq!(lines[2]["duration_ms"], json!(842));
    }

    #[test]
    fn the_receiver_stamps_arrival_itself_because_no_hook_event_carries_a_time() {
        let root = TempRoot::new("hook-arrival");
        assert_eq!(receive(root.path(), &pre_tool_use().to_string()), 0);
        let lines = spool_lines(root.path());
        assert_eq!(
            lines[0][RECEIVED_AT], AT,
            "the receiver's clock is the only time source a hook has"
        );
        // Named for what it is. A field called `ts` or `timestamp` would read as the
        // event's own time to everything downstream, which is the confusion the whole
        // `time_basis` machinery exists to prevent.
        for pretending in ["\"ts\"", "\"timestamp\"", "\"occurred_at\""] {
            let raw = std::fs::read_to_string(spool_path(root.path())).unwrap();
            assert!(!raw.contains(pretending), "{pretending} in {raw}");
        }
    }

    #[test]
    fn the_spool_and_its_directory_are_owner_only() {
        let root = TempRoot::new("hook-perms");
        assert_eq!(receive(root.path(), &pre_tool_use().to_string()), 0);
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(root.path().join(SPOOL_DIR_NAME)),
            0o700,
            "the spool directory must be owner-only"
        );
        assert_eq!(
            mode(spool_path(root.path())),
            0o600,
            "the spool file holds cwds, which carry the username"
        );
    }

    #[test]
    fn each_invocation_appends_rather_than_replacing_what_is_there() {
        let root = TempRoot::new("hook-append");
        for _ in 0..3 {
            assert_eq!(receive(root.path(), &pre_tool_use().to_string()), 0);
        }
        assert_eq!(spool_lines(root.path()).len(), 3);
    }

    #[test]
    fn an_oversize_payload_becomes_a_bounded_marker_rather_than_a_line_that_can_tear() {
        // Several hook processes append to one file at once. A line short enough for a
        // single write cannot interleave with theirs; a very long one could. The loss
        // is recorded rather than silently dropped.
        let root = TempRoot::new("hook-oversize");
        let mut huge = pre_tool_use();
        huge["cwd"] = json!("/SYNTHETIC/".to_string() + &"d".repeat(MAX_LINE_BYTES));
        assert_eq!(receive(root.path(), &huge.to_string()), 0);

        let lines = spool_lines(root.path());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][RECEIVER_ERROR], "payload_oversize");
        assert_eq!(
            lines[0]["hook_event_name"], "PreToolUse",
            "the marker keeps what makes it diagnosable"
        );
        assert!(lines[0].get("cwd").is_none(), "and drops what was long");
        let raw = std::fs::read_to_string(spool_path(root.path())).unwrap();
        assert!(
            raw.len() <= MAX_LINE_BYTES,
            "every spool line stays inside one atomic append: {} bytes",
            raw.len()
        );
    }

    #[test]
    fn every_receiver_error_is_a_valid_gap_reason() {
        // These land in a `dev.cclog.source.gap.v1` marker's `reason`, which the
        // canonical schema constrains to `^[a-z0-9_]+$`.
        for reason in RECEIVER_ERRORS {
            assert!(
                !reason.is_empty()
                    && reason
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{reason} is not a usable gap reason"
            );
        }
    }

    #[test]
    fn the_install_text_names_every_registered_event_and_writes_nothing() {
        let root = TempRoot::new("hook-install");
        let text = install_instructions(Path::new("/SYNTHETIC/bin/cclogger"));
        for event in REGISTERED_EVENTS {
            assert!(text.contains(event), "{event} missing from:\n{text}");
        }
        assert!(
            text.contains("/SYNTHETIC/bin/cclogger hook"),
            "the snippet must name the binary it was printed by:\n{text}"
        );
        // The three things the channel cannot do, said where someone is deciding to
        // turn it on rather than discovered later.
        assert!(text.contains("Nothing before this moment"));
        assert!(text.contains("does not fire when you interrupt"));
        assert!(!root.path().exists(), "printing must create nothing");
    }

    /// The `{ "hooks": ... }` block out of the printed instructions.
    fn snippet(text: &str) -> Value {
        let start = text.find("{\n  \"hooks\"").expect("a JSON block");
        let end = text[start..].find("\n}\n").expect("its close") + start + 2;
        serde_json::from_str(&text[start..end]).unwrap_or_else(|e| {
            panic!(
                "the printed settings block must parse: {e}\n{}",
                &text[start..end]
            )
        })
    }

    #[test]
    fn a_binary_path_that_would_break_hand_written_json_is_still_emitted_as_json() {
        // Unusual, but `cargo install --root` takes any directory, and a settings file
        // that fails to parse takes every one of the user's own hooks down with it.
        let awkward = r#"/SYNTHETIC/b"i\n/cclogger"#;
        let parsed = snippet(&install_instructions(Path::new(awkward)));
        assert_eq!(
            parsed["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            Value::from(format!("{awkward} hook")),
            "the path must survive escaping intact"
        );
    }

    #[test]
    fn the_snippet_is_the_json_it_claims_to_be() {
        // A settings block that does not parse is worse than no subcommand at all.
        let text = install_instructions(Path::new("/SYNTHETIC/bin/cclogger"));
        let parsed = snippet(&text);
        let hooks = parsed["hooks"].as_object().expect("hooks is an object");
        assert_eq!(hooks.len(), REGISTERED_EVENTS.len());
        for event in REGISTERED_EVENTS {
            let group = hooks[*event].as_array().expect("matcher groups");
            assert_eq!(group[0]["hooks"][0]["type"], "command");
            assert_eq!(
                group[0]["hooks"][0]["command"],
                "/SYNTHETIC/bin/cclogger hook"
            );
            // `matcher` is deliberately omitted: it is unsupported on `Stop` and
            // `UserPromptSubmit`, and omitting it means "every match" on the events
            // that do support one -- which is what this channel wants everywhere.
            assert!(group[0].get("matcher").is_none());
        }
    }
}
