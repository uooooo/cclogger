//! Claude Code hook adapter: spool lines → observation drafts.
//!
//! The records this reads are **spool lines**, not raw hook payloads: `cclogger hook`
//! projects each payload onto a fixed metadata allowlist before anything is written to
//! disk, so prompt text, model output, tool arguments and tool output never reach this
//! module or the ledger. What survives is the vendor's own field names
//! (`hook_event_name`, `session_id`, `cwd`, `prompt_id`, `tool_use_id`, `tool_name`,
//! `agent_id`, `agent_type`, `reason`, `duration_ms`) plus the receiver's `received_at`.
//!
//! # Time
//!
//! **No hook event carries a timestamp** -- the reference is explicit that no global
//! `timestamp` field exists in the hook input schema -- so `time` here is the
//! receiver's arrival stamp and every draft says so with
//! `data.time_basis = "received_at"`, applied once in [`finish`] so no arm can forget
//! it. The error is the time Claude Code takes to spawn the hook process, which is
//! milliseconds; that is why `received_at` may be put on a clock where `acquired_at`
//! and `copied_at` may not.
//!
//! # Absence is absence
//!
//! This adapter and [`crate::claude_code_history`] write the same fields into the same
//! ledger from two different channels, so a value one of them fabricates is
//! indistinguishable from one the other measured. Both therefore say "not measured" the
//! way the schema provides for, rather than with a plausible-looking default:
//!
//! - a count bucket (`prompt_tokens`, `response_tokens`, `turn_count`) is **omitted**
//!   when the record carries no count -- `"0"` is a measurement, and every one of
//!   those fields is optional in the schema (there is no `null` member of the bucket
//!   enum to use instead). No hook payload documented today carries any of the three,
//!   so these arms are dormant on this channel: they exist because the field is the
//!   right one to fill if a payload ever does, not because one does now;
//! - `duration_ms` is **`null`** when the record carries no duration -- it is a
//!   required field whose schema type is explicitly `["integer", "null"]` for this,
//!   and `0` is a real measurement of a tool that returned within a millisecond;
//! - `session_kind` is **`"unknown"`**. No hook payload field says whether a session is
//!   interactive or headless, and this channel does not guess one. (The historical
//!   Claude Code adapter still hardcodes `"interactive"`; that is a known wart of that
//!   module, noted in [`crate::codex_history`], and is not copied here.)
//!
//! # What this channel cannot see
//!
//! `Stop` does not fire when a turn is interrupted; an API error fires `StopFailure`
//! instead, and a killed process fires nothing at all. So a `prompt.submitted` with no
//! matching `response.completed` is normal, and a count of completions is a floor
//! rather than a total. `cclogger import` says so in its own output rather than
//! leaving the arithmetic to imply otherwise.

use crate::Keystore;
use cclogger_domain::{IntegrityState, ObservationDraft, PrivacyClass, SourceKind};
use serde_json::{Value, json};

pub const SOURCE_VERSION: &str = "claude-code-hook/1";
pub const ADAPTER_VERSION: &str = "claude-code@0.0.0";

/// `hook_event_name` values this module has a case for. An importer should treat any
/// other value as an unmapped record kind (a gap) rather than dropping it.
///
/// `SubagentStop` is deliberately not here. Claude Code converts a subagent's `Stop`
/// into `SubagentStop`, so mapping both would put two `response.completed` observations
/// on one turn -- the subagent's and the main loop's -- and inflate every turn count
/// that reads them.
pub const MAPPED_HOOKS: &[&str] = &[
    "PostToolUse",
    "PostToolUseFailure",
    "PreToolUse",
    "SessionEnd",
    "SessionStart",
    "Stop",
    "StopFailure",
    "UserPromptSubmit",
];

/// The one `data.time_basis` value this adapter emits: `time` is when the receiver saw
/// the event, never a clock the event itself carried.
pub const TIME_BASIS: &str = "received_at";

/// The adapter-computed parts of an observation; the runtime-stamped fields (id, clocks,
/// source ref, device) are filled later by [`cclogger_domain::ObservationDraft::finalize`].
struct Parts {
    event_type: &'static str,
    subject: String,
    /// Most-specific ref used in the dedupe key (turn or tool); `None` for session events.
    scope_ref: Option<String>,
    workspace_ref: Option<String>,
    repository_ref: Option<String>,
    integrity: IntegrityState,
    correlation: Option<String>,
    traceparent: Option<String>,
    data: Value,
}

/// Transform one spool line into an observation draft, or `None` if the hook is not one
/// this adapter maps (or a required ref is unresolved).
pub fn transform(record: &Value, ctx: &Keystore) -> Option<ObservationDraft> {
    let hook = record.get("hook_event_name")?.as_str()?;
    let session_ref = ctx.resolve("session", record.get("session_id")?.as_str()?)?;
    let time = record.get("received_at")?.as_str()?.to_string();

    let cwd = record.get("cwd").and_then(Value::as_str);
    // A record that carries a `cwd` is identified by that cwd and nothing else; one
    // that carries none falls back to what the session as a whole resolved to. Same
    // rule as the historical adapter's `identity_ref`, so an observation of the same
    // work through either channel lands in the same repository.
    let workspace_ref = identity(ctx, cwd, record, "workspace", "session_workspace");
    let repository_ref = identity(ctx, cwd, record, "repository", "session_repository");
    // Common to every event, so a tool call can be placed in the turn it belongs to --
    // not just `UserPromptSubmit` and `Stop`.
    let turn_ref = record
        .get("prompt_id")
        .and_then(Value::as_str)
        .and_then(|t| ctx.resolve("turn", t));
    let tool_ref = record
        .get("tool_use_id")
        .and_then(Value::as_str)
        .and_then(|t| ctx.resolve("tool", t));

    let parts = match hook {
        "SessionStart" => Parts {
            event_type: "dev.cclog.session.started.v1",
            subject: format!("session/{session_ref}"),
            scope_ref: None,
            workspace_ref,
            repository_ref,
            integrity: IntegrityState::Ok,
            correlation: None,
            traceparent: None,
            // Not `"interactive"`: no hook payload field distinguishes an interactive
            // session from a headless one, and `"unknown"` is a real member of the
            // schema's enum for exactly this.
            data: json!({ "session_kind": "unknown" }),
        },
        "SessionEnd" => {
            let mut data = json!({
                "reason": normalize_reason(record.get("reason").and_then(Value::as_str)),
            });
            set_bucket(&mut data, "turn_count", record.get("turn_count"));
            Parts {
                event_type: "dev.cclog.session.ended.v1",
                subject: format!("session/{session_ref}"),
                scope_ref: None,
                workspace_ref,
                repository_ref,
                integrity: IntegrityState::Ok,
                correlation: None,
                traceparent: None,
                data,
            }
        }
        "UserPromptSubmit" => {
            let turn = turn_ref.clone()?;
            let mut data = json!({ "content_ref": null });
            set_bucket(&mut data, "prompt_tokens", record.get("prompt_tokens"));
            Parts {
                event_type: "dev.cclog.prompt.submitted.v1",
                subject: format!("session/{session_ref}/turn/{turn}"),
                scope_ref: turn_ref,
                workspace_ref,
                repository_ref,
                integrity: IntegrityState::Ok,
                correlation: None,
                traceparent: None,
                data,
            }
        }
        "Stop" | "StopFailure" => {
            let turn = turn_ref.clone()?;
            // The outcome comes from which event fired, because the payload carries no
            // outcome field at all: `Stop` is the documented end of a turn and
            // `StopFailure` is the documented end of one that hit an API error. A turn
            // the user interrupted fires neither, which is why this channel's turn
            // count is a floor -- see the module comment.
            let mut data = json!({
                "outcome": if hook == "Stop" { "succeeded" } else { "failed" },
                "content_ref": null,
            });
            set_bucket(&mut data, "response_tokens", record.get("response_tokens"));
            Parts {
                event_type: "dev.cclog.response.completed.v1",
                subject: format!("session/{session_ref}/turn/{turn}"),
                scope_ref: turn_ref,
                workspace_ref,
                repository_ref,
                integrity: IntegrityState::Ok,
                correlation: None,
                traceparent: None,
                data,
            }
        }
        "PreToolUse" => {
            let tool = tool_ref.clone()?;
            let mut data = json!({
                "tool_family": tool_family(record.get("tool_name").and_then(Value::as_str)),
                "workspace_ref": workspace_ref,
                "content_ref": null,
            });
            set_agent(&mut data, record, ctx);
            Parts {
                event_type: "dev.cclog.tool.started.v1",
                subject: tool_subject(&session_ref, turn_ref.as_deref(), &tool),
                scope_ref: tool_ref,
                workspace_ref,
                repository_ref,
                integrity: IntegrityState::Ok,
                correlation: None,
                traceparent: None,
                data,
            }
        }
        "PostToolUse" | "PostToolUseFailure" => {
            let tool = tool_ref.clone()?;
            // `None`, serialized as a literal `null`, when the record carries no
            // duration -- not `0`. A payload without `duration_ms` measured nothing,
            // while `0` is a real measurement of a tool that returned within a
            // millisecond, and the historical adapter emits genuine `null`s into this
            // same field (see `crate::claude_code_history`'s "Tool durations").
            //
            // This is the field the hook channel exists for: the reference documents it
            // as excluding time spent in permission prompts and PreToolUse hooks, which
            // is exactly what made the transcript path's longest "duration" 42.8 hours.
            let duration = record.get("duration_ms").and_then(Value::as_u64);
            let mut data = json!({
                "tool_family": tool_family(record.get("tool_name").and_then(Value::as_str)),
                "outcome": if hook == "PostToolUse" { "succeeded" } else { "failed" },
                "duration_ms": duration,
                "workspace_ref": workspace_ref,
                "content_ref": null,
            });
            set_agent(&mut data, record, ctx);
            Parts {
                event_type: "dev.cclog.tool.finished.v1",
                subject: tool_subject(&session_ref, turn_ref.as_deref(), &tool),
                scope_ref: tool_ref,
                workspace_ref,
                repository_ref,
                integrity: IntegrityState::Ok,
                correlation: None,
                traceparent: None,
                data,
            }
        }
        _ => return None,
    };

    Some(finish(&session_ref, time, parts))
}

/// Resolve one identity kind for a record: by its own `cwd` when it has one, else by
/// what its session resolved to.
///
/// A `cwd` that resolves to nothing leaves the record unresolved rather than borrowing
/// the session's identity, which would merge it into a repository it was never in.
fn identity(
    ctx: &Keystore,
    cwd: Option<&str>,
    record: &Value,
    by_cwd: &str,
    by_session: &str,
) -> Option<String> {
    match cwd {
        Some(cwd) => ctx.resolve(by_cwd, cwd),
        None => record
            .get("session_id")
            .and_then(Value::as_str)
            .and_then(|s| ctx.resolve(by_session, s)),
    }
}

/// Where a tool event sits in the subject hierarchy.
///
/// `prompt_id` is a documented common field, so the turn is normally there. When it is
/// not -- an older build, or a payload shape that changed -- the event is still emitted,
/// anchored one level up, rather than dropped: real tool execution evidence is not worth
/// losing over a missing correlation id. That shorter shape is the one
/// [`crate::claude_code_history`] already produces for every historical tool event.
fn tool_subject(session: &str, turn: Option<&str>, tool: &str) -> String {
    match turn {
        Some(turn) => format!("session/{session}/turn/{turn}/tool/{tool}"),
        None => format!("session/{session}/tool/{tool}"),
    }
}

/// Record which subagent made a tool call, when the payload named one.
///
/// `agent_id` and `agent_type` are common fields present **only** when the hook fires
/// inside a subagent, which is what lets this channel tell subagent work apart at all --
/// something the transcript path cannot do reliably. Both are therefore absent for a
/// main-loop call, and absence is left as absence rather than filled with a
/// `"subagent": false` this adapter has no evidence for.
///
/// `agent_type` is vendor-controlled text (a user's own subagent name), so it is held to
/// the same discipline as a gap marker's `detail`: a plain identifier of at most 64
/// bytes, or nothing.
fn set_agent(data: &mut Value, record: &Value, ctx: &Keystore) {
    if let Some(agent_ref) = record
        .get("agent_id")
        .and_then(Value::as_str)
        .and_then(|id| ctx.resolve("agent", id))
    {
        data["agent_ref"] = Value::String(agent_ref);
    }
    if let Some(label) = record
        .get("agent_type")
        .and_then(Value::as_str)
        .and_then(agent_label)
    {
        data["agent_type"] = Value::String(label);
    }
}

/// Accept a vendor `agent_type` only if it is a plausible identifier. A value that is
/// not is dropped rather than truncated or escaped: the subagent is already identified
/// by its opaque `agent_ref`, so nothing is lost.
fn agent_label(raw: &str) -> Option<String> {
    let plausible = (1..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':');
    plausible.then(|| raw.to_string())
}

fn finish(session_ref: &str, time: String, p: Parts) -> ObservationDraft {
    let event_short = p
        .event_type
        .strip_prefix("dev.cclog.")
        .and_then(|s| s.strip_suffix(".v1"))
        .unwrap_or(p.event_type);

    let mut dedupe_seed = vec![session_ref.to_string()];
    if let Some(scope) = &p.scope_ref {
        dedupe_seed.push(scope.clone());
    }
    dedupe_seed.push(event_short.to_string());
    dedupe_seed.push(time.clone());

    let mut data = p.data;
    // Applied here rather than in each arm so no arm can omit it. `time` on every
    // observation from this channel is the receiver's arrival stamp, and a consumer
    // putting it on a clock has to be told which clock it came from.
    data["time_basis"] = Value::String(TIME_BASIS.to_string());

    ObservationDraft {
        event_type: p.event_type.to_string(),
        subject: p.subject,
        time,
        traceparent: p.traceparent,
        source_kind: SourceKind::ClaudeCode,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: p.integrity,
        workspace_ref: p.workspace_ref,
        repository_ref: p.repository_ref,
        correlation_cluster: p.correlation,
        dedupe_seed,
        data,
    }
}

/// Coarse magnitude bucket for a count that was measured; `None` when it was not.
/// Exact sensitive counts are never stored on a T1 row.
///
/// `None` in, `None` out -- deliberately *not* the `"0"` bucket, which is a real
/// measurement. [`set_bucket`] drops the field entirely for a `None`, which is how
/// "not measured" is said for a count: the bucket enum has no `null` member, and every
/// count field in the schema is optional.
fn bucket(n: Option<u64>) -> Option<&'static str> {
    Some(match n? {
        0 => "0",
        1..=9 => "1-9",
        10..=99 => "10-99",
        100..=999 => "100-999",
        1000..=9999 => "1000-9999",
        _ => "10000+",
    })
}

/// Write `field` into `data` as a count bucket, or leave it absent when `raw` carries
/// no usable count -- either because the record omitted it or because it was not a
/// non-negative integer.
fn set_bucket(data: &mut Value, field: &str, raw: Option<&Value>) {
    if let Some(b) = bucket(raw.and_then(Value::as_u64)) {
        data[field] = Value::String(b.to_string());
    }
}

/// Normalize a vendor tool name into a canonical tool family.
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

/// Map a `SessionEnd` reason onto the schema's vocabulary.
///
/// The documented values a real `SessionEnd` carries -- `clear`, `resume`, `logout`,
/// `prompt_input_exit`, `bypass_permissions_disabled`, `other` -- describe *how the
/// session was left*, and none of them is the "completed / aborted / timeout / crash"
/// distinction this field records. They therefore land on `"unknown"`, which is a real
/// member of the enum and the honest answer, rather than being bent into `"completed"`
/// because a session ending sounds like one completing. The four canonical spellings
/// are still accepted, so a payload that ever does carry one is read rather than
/// discarded.
fn normalize_reason(reason: Option<&str>) -> &'static str {
    match reason.unwrap_or("unknown") {
        "completed" => "completed",
        "aborted" => "aborted",
        "timeout" => "timeout",
        "crash" => "crash",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks() -> Keystore {
        Keystore::new()
            .map("session", "cc-1", "ses_TEST")
            .map("turn", "prm-1", "trn_TEST")
            .map("tool", "tool-1", "tol_TEST")
            .map("agent", "agt-1", "agt_TEST")
            .map("workspace", "/SYNTHETIC/work", "wsp_TEST")
            .map("repository", "/SYNTHETIC/work", "rep_TEST")
    }

    /// A spool line for `hook` carrying only the refs the arm requires -- no counts,
    /// no duration, no subagent.
    fn hook(hook: &str) -> Value {
        json!({
            "v": 1,
            "hook_event_name": hook,
            "session_id": "cc-1",
            "prompt_id": "prm-1",
            "tool_use_id": "tool-1",
            "tool_name": "Bash",
            "cwd": "/SYNTHETIC/work",
            "received_at": "2026-08-01T00:00:00.000Z",
        })
    }

    fn draft_of(record: &Value) -> ObservationDraft {
        transform(record, &ks()).expect("hook is mapped")
    }

    fn data_of(record: &Value) -> serde_json::Map<String, Value> {
        draft_of(record)
            .data
            .as_object()
            .expect("data is an object")
            .clone()
    }

    #[test]
    fn every_mapped_hook_produces_a_draft_and_nothing_else_does() {
        for name in MAPPED_HOOKS {
            assert!(
                transform(&hook(name), &ks()).is_some(),
                "{name} is listed as mapped but produced nothing"
            );
        }
        for unmapped in ["SubagentStop", "Notification", "PreCompact", "Setup"] {
            assert!(
                transform(&hook(unmapped), &ks()).is_none(),
                "{unmapped} must be left for the importer to diagnose as a gap"
            );
        }
    }

    #[test]
    fn every_draft_says_its_time_is_an_arrival_stamp_not_an_event_time() {
        // No hook event carries a timestamp, so `time` here is always the receiver's
        // clock. An arm that forgot to say so would put a receipt time on a clock as
        // though it were measured at the source.
        for name in MAPPED_HOOKS {
            let draft = draft_of(&hook(name));
            assert_eq!(
                draft.data["time_basis"], TIME_BASIS,
                "{name} must state which clock its time came from"
            );
            assert_eq!(draft.time, "2026-08-01T00:00:00.000Z");
        }
    }

    #[test]
    fn a_payload_without_a_count_omits_the_bucket_rather_than_reporting_zero() {
        // `"0"` is a measurement: the records below reported nothing, and the
        // historical adapter writes genuine counts into these same ledger fields.
        for (hook_name, field) in [
            ("UserPromptSubmit", "prompt_tokens"),
            ("Stop", "response_tokens"),
            ("SessionEnd", "turn_count"),
        ] {
            let data = data_of(&hook(hook_name));
            assert!(
                !data.contains_key(field),
                "{hook_name} carried no {field}, so the field must be absent rather \
                 than a fabricated bucket (got {:?})",
                data.get(field)
            );
        }
    }

    #[test]
    fn a_measured_zero_count_still_reports_the_zero_bucket() {
        // The other half of the pair above: omitting the field for a real zero would
        // lose a measurement just as surely as inventing one loses the distinction.
        for (hook_name, field) in [
            ("UserPromptSubmit", "prompt_tokens"),
            ("Stop", "response_tokens"),
            ("SessionEnd", "turn_count"),
        ] {
            let mut record = hook(hook_name);
            record[field] = json!(0);
            assert_eq!(
                data_of(&record).get(field),
                Some(&json!("0")),
                "{hook_name} measured {field} = 0, which is a measurement"
            );
        }
    }

    #[test]
    fn a_payload_without_a_duration_reports_null_rather_than_zero() {
        let data = data_of(&hook("PostToolUse"));
        assert_eq!(
            data.get("duration_ms"),
            Some(&Value::Null),
            "duration_ms is required, so it stays present -- but as an explicit null, \
             never a fabricated 0"
        );
    }

    #[test]
    fn a_measured_zero_duration_is_still_reported_as_zero() {
        let mut record = hook("PostToolUse");
        record["duration_ms"] = json!(0);
        assert_eq!(
            data_of(&record).get("duration_ms"),
            Some(&json!(0)),
            "a tool that returned within a millisecond really did measure 0"
        );
    }

    #[test]
    fn bucket_maps_an_absent_count_to_none_and_a_measured_zero_to_the_zero_bucket() {
        assert_eq!(
            bucket(None),
            None,
            "an absent count has no bucket; the caller omits the field"
        );
        assert_eq!(bucket(Some(0)), Some("0"));
        assert_eq!(bucket(Some(9)), Some("1-9"));
        assert_eq!(bucket(Some(10_000)), Some("10000+"));
    }

    #[test]
    fn a_turn_that_failed_is_recorded_as_failed_rather_than_as_one_that_succeeded() {
        // `Stop` does not fire on an API error -- `StopFailure` does. Mapping only
        // `Stop` would leave every failed turn out of the ledger entirely; mapping both
        // to `"succeeded"` would be worse still.
        assert_eq!(data_of(&hook("Stop"))["outcome"], "succeeded");
        assert_eq!(data_of(&hook("StopFailure"))["outcome"], "failed");
        assert_eq!(data_of(&hook("PostToolUse"))["outcome"], "succeeded");
        assert_eq!(data_of(&hook("PostToolUseFailure"))["outcome"], "failed");
    }

    #[test]
    fn a_session_start_does_not_claim_to_know_whether_the_session_was_interactive() {
        // No hook payload field carries this. `"unknown"` is a member of the schema's
        // enum precisely so it does not have to be guessed.
        assert_eq!(data_of(&hook("SessionStart"))["session_kind"], "unknown");
    }

    #[test]
    fn a_session_end_reason_outside_the_canonical_vocabulary_is_unknown_not_completed() {
        for documented in [
            "clear",
            "resume",
            "logout",
            "prompt_input_exit",
            "bypass_permissions_disabled",
            "other",
        ] {
            let mut record = hook("SessionEnd");
            record["reason"] = json!(documented);
            assert_eq!(
                data_of(&record)["reason"],
                "unknown",
                "{documented} says how the session was left, not whether it completed"
            );
        }
        let mut canonical = hook("SessionEnd");
        canonical["reason"] = json!("crash");
        assert_eq!(data_of(&canonical)["reason"], "crash");
    }

    #[test]
    fn a_tool_call_carries_the_repository_its_cwd_resolves_to() {
        // The previous version of this adapter hardcoded `repository_ref: None` on
        // every arm, which would have left every hook observation out of every
        // per-repository total in the report.
        for name in MAPPED_HOOKS {
            let draft = draft_of(&hook(name));
            assert_eq!(
                draft.repository_ref.as_deref(),
                Some("rep_TEST"),
                "{name} lost the repository its cwd resolves to"
            );
            assert_eq!(draft.workspace_ref.as_deref(), Some("wsp_TEST"));
        }
    }

    #[test]
    fn a_record_with_no_cwd_falls_back_to_what_its_session_resolved_to() {
        let ctx = Keystore::new()
            .map("session", "cc-1", "ses_TEST")
            .map("tool", "tool-1", "tol_TEST")
            .map("session_repository", "cc-1", "rep_SESSION")
            .map("session_workspace", "cc-1", "wsp_SESSION");
        let mut record = hook("PreToolUse");
        record.as_object_mut().unwrap().remove("cwd");
        let draft = transform(&record, &ctx).expect("mapped");
        assert_eq!(draft.repository_ref.as_deref(), Some("rep_SESSION"));
        assert_eq!(draft.workspace_ref.as_deref(), Some("wsp_SESSION"));
    }

    #[test]
    fn a_subagents_tool_call_says_which_agent_made_it_and_a_main_loop_call_says_nothing() {
        let plain = data_of(&hook("PreToolUse"));
        assert!(
            !plain.contains_key("agent_ref") && !plain.contains_key("agent_type"),
            "a main-loop call names no subagent, and absence is left as absence"
        );

        let mut sub = hook("PreToolUse");
        sub["agent_id"] = json!("agt-1");
        sub["agent_type"] = json!("Explore");
        let data = data_of(&sub);
        assert_eq!(data["agent_ref"], "agt_TEST");
        assert_eq!(data["agent_type"], "Explore");
    }

    #[test]
    fn an_implausible_agent_type_is_dropped_rather_than_carried_into_the_ledger() {
        let mut sub = hook("PreToolUse");
        sub["agent_id"] = json!("agt-1");
        sub["agent_type"] = json!("/Users/dev/a name with spaces");
        let data = data_of(&sub);
        assert!(
            !data.contains_key("agent_type"),
            "vendor text that is not a plain identifier must not reach a T1 row: {data:?}"
        );
        assert_eq!(
            data["agent_ref"], "agt_TEST",
            "the subagent is still identified by its opaque ref"
        );
        assert_eq!(
            agent_label(&"x".repeat(65)),
            None,
            "and 64 bytes is the bound"
        );
    }

    #[test]
    fn a_tool_event_with_no_resolvable_turn_is_still_emitted_one_level_up() {
        // `prompt_id` is a documented common field, but real evidence of tool execution
        // is not worth dropping if a build ever stops sending one.
        let ctx = Keystore::new()
            .map("session", "cc-1", "ses_TEST")
            .map("tool", "tool-1", "tol_TEST");
        let draft = transform(&hook("PreToolUse"), &ctx).expect("still emitted");
        assert_eq!(draft.subject, "session/ses_TEST/tool/tol_TEST");
        assert_eq!(
            draft.dedupe_seed,
            vec![
                "ses_TEST".to_string(),
                "tol_TEST".to_string(),
                "tool.started".to_string(),
                "2026-08-01T00:00:00.000Z".to_string(),
            ]
        );

        // A turn event, by contrast, has nothing to anchor on without one.
        assert!(transform(&hook("UserPromptSubmit"), &ctx).is_none());
    }

    #[test]
    fn a_tool_event_that_knows_its_turn_records_it_in_the_subject() {
        assert_eq!(
            draft_of(&hook("PreToolUse")).subject,
            "session/ses_TEST/turn/trn_TEST/tool/tol_TEST"
        );
        assert_eq!(
            draft_of(&hook("UserPromptSubmit")).subject,
            "session/ses_TEST/turn/trn_TEST"
        );
    }

    #[test]
    fn a_record_missing_the_fields_every_event_needs_produces_nothing() {
        for missing in ["hook_event_name", "session_id", "received_at"] {
            let mut record = hook("PreToolUse");
            record.as_object_mut().unwrap().remove(missing);
            assert!(
                transform(&record, &ks()).is_none(),
                "a record with no {missing} cannot be placed and must not be invented"
            );
        }
    }
}
