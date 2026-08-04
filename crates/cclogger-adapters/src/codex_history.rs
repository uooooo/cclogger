//! Codex historical adapter: transcript JSONL records → observation drafts.
//!
//! The Codex counterpart to [`crate::claude_code_history`], and deliberately a separate
//! module for the same reason those two are: the record shapes have nothing in common
//! beyond both being JSONL. A Codex transcript record is always
//! `{"type": …, "timestamp": …, "payload": {…}}`, and its *kind* is `type` plus --
//! for the two envelope types that carry a discriminated union, `event_msg` and
//! `response_item` -- `payload.type`, e.g. `event_msg:user_message` (see [`kind`]).
//! `adapters/codex/shapes/*.shape.json` reproduces the real record shapes without any
//! real content, and is the contract this module is written against.
//!
//! # Contract
//!
//! Same purity contract as every other adapter: no clock, no RNG, no I/O, no
//! `std::env`. Anything this module cannot compute from the one record in front of it
//! arrives through the [`Keystore`] the importer's whole-file pre-scan builds. Ids,
//! device, and profile are stamped later by
//! [`cclogger_domain::ObservationDraft::finalize`]. Arity is 0..N, matching
//! [`crate::claude_code_history::transform`].
//!
//! # What makes Codex different from Claude Code
//!
//! Three things, all of which this module's identity strategy exists to handle. Each is
//! a measured fact about the real 328-file corpus, not a guess; the survey behind them
//! is recorded in `docs/superpowers/plans/2026-08-01-codex-importer.md`.
//!
//! 1. **The human-prompt record carries no id at all.** `event_msg:user_message`'s
//!    payload keys are `type`, `message`, `local_images`, `text_elements`, `images`,
//!    `client_id`, `audio`, `local_audio` -- there is no `uuid` to dedupe on the way
//!    every Claude Code transcript record can be. Identity has to be derived from
//!    content; see [`content_identity`].
//! 2. **`cwd` never appears on a prompt record.** It lives on `session_meta` (1,907
//!    records) and `turn_context` (3,350) and nowhere else, so a Codex observation's
//!    workspace cannot be resolved per-record by path the way
//!    [`crate::claude_code_history`] does it. Both `workspace` and `repository` are
//!    looked up by *session* instead. Unlike the Claude path there is no majority vote
//!    to take: `cwd` was measured not to vary within a single Codex file (0 of 328
//!    files show more than one), so the session's answer is the record's answer.
//! 3. **Most records name no session either.** `session_id` appears on `session_meta`
//!    (and only on newer CLI versions -- 4 of 328 files omit it entirely), so nearly
//!    every record has to inherit its session from the file it is in. That is a
//!    whole-file fact, so it reaches this module the way every whole-file fact does:
//!    the importer registers the file's session under [`FILE_SESSION`] in the
//!    [`Keystore`], and [`session_key`] falls back to it.
//!
//! # Record kinds mapped, and why
//!
//! [`MAPPED_KINDS`] is the list, and it means exactly what its Claude Code
//! counterpart means: a kind this module has a case for at all. The importer turns any
//! *other* kind into a `dev.cclog.source.gap.v1` marker rather than dropping it
//! silently.
//!
//! `response_item:agent_message` is deliberately **not** mapped: it duplicates
//! `event_msg:agent_message` for the same turn, and mapping both would double every
//! Codex response. It is left to become a diagnosed gap rather than a counted skip,
//! even though a skip would describe it better, because the importer distinguishes the
//! two by `MAPPED_KINDS` membership and this module's own
//! `every_mapped_kind_produces_drafts_and_unlisted_kinds_produce_none` test pins every
//! listed kind to produce at least one draft. Listing a kind that by construction never
//! produces one would break that contract in the other direction, which is the worse
//! trade: a gap is visible and countable, a mis-stated invariant is not.
//!
//! `response_item:message` (7,303 records) is also unmapped, and is a genuine open
//! question rather than a decision: `message-without-id.shape.json` exists because some
//! CLI versions may route human turns through it. It stays a gap until the gap counts
//! from a real import say how big it is.
//!
//! `event_msg:mcp_tool_call_end` (6,220 records) *is* mapped, unlike its siblings
//! `patch_apply_end` and `sub_agent_activity`: those two describe work the ledger
//! already holds under other kinds, so mapping them would double-count, while an MCP
//! call is counted nowhere else -- 51 distinct MCP tool names appear in
//! `mcp_tool_call_end` and zero among `function_call`'s 31, so these are not a second
//! view of something already imported. `token_count` and `agent_reasoning` are a
//! different tool's subject, not this one's, and stay unmapped too.
//!
//! # Tool durations
//!
//! Same mechanism as the Claude Code historical path, for the same reason: a
//! `*_call_output` record carries no duration, so the interval is closed against the
//! `tool_started_at` timestamp the importer's pre-scan registers for the matching call.
//! When it is unavailable the field is `null` -- never `0`, which is a real measurement
//! of a tool that returned within a millisecond.
//!
//! `event_msg:mcp_tool_call_end` is the one exception: it carries its own `duration` --
//! a serialized Rust `Duration`, `{secs, nanos}` -- because it is a single self-reporting
//! record rather than a call/output pair split across two lines the way
//! `custom_tool_call`/`function_call` are. [`transform_mcp_tool_call`] converts it
//! directly rather than closing an interval against a `tool_started_at` this record kind
//! never registers. Same rule for what an unmeasurable duration means: `null`, never
//! `0`.

use crate::{Keystore, pseudonymize};
use cclogger_domain::{IntegrityState, ObservationDraft, PrivacyClass, SourceKind};
use serde_json::{Value, json};

pub const SOURCE_VERSION: &str = "codex-transcript/1";
pub const ADAPTER_VERSION: &str = "codex-history/0.0.0";

/// The [`Keystore`] vendor key under which the importer registers the identities of the
/// *file's own* session.
///
/// Nearly every Codex record names no session: measured over the real corpus,
/// `session_id` occurs only on `session_meta` and takes exactly one value per file
/// (absent entirely in 4 of 328 files), while the prompt record that matters most
/// carries no session field at all. A Codex transcript is one session, so the importer
/// -- which has the whole-file view this pure transform does not -- registers that
/// session's `session`, `workspace` and `repository` refs under this key in addition to
/// the raw vendor id, and [`session_key`] falls back to it for every record that names
/// no session of its own.
///
/// The value is not a possible vendor session id (those are UUIDs), so a record whose
/// own id happened to equal it cannot exist.
pub const FILE_SESSION: &str = "*file*";

/// Record kinds this module has a case for at all, in the [`kind`] encoding.
///
/// An importer should treat any *other* kind as an unmapped record kind (a gap, design
/// doc §8). A record whose kind *is* in this list producing zero drafts is not a gap:
/// it means the kind is understood and this particular instance is missing something it
/// needs (e.g. a `session_meta` whose session the pre-scan could not resolve).
pub const MAPPED_KINDS: &[&str] = &[
    "event_msg:user_message",
    "event_msg:agent_message",
    "session_meta",
    "response_item:custom_tool_call",
    "response_item:function_call",
    "response_item:custom_tool_call_output",
    "response_item:function_call_output",
    "event_msg:mcp_tool_call_end",
];

/// The kind of a Codex transcript record: its `type`, plus `payload.type` for the two
/// envelope types that carry a discriminated union under it.
///
/// Public because the importer needs the same string this module matches on -- to look
/// a record up in [`MAPPED_KINDS`], and to label a gap bucket -- and computing it in two
/// places is how the two would silently drift apart.
pub fn kind(record: &Value) -> String {
    let outer = record.get("type").and_then(Value::as_str).unwrap_or("");
    match outer {
        "event_msg" | "response_item" => {
            let inner = record
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("{outer}:{inner}")
        }
        other => other.to_string(),
    }
}

/// Transform one historical Codex transcript record into 0..N observation drafts.
/// Returns an empty `Vec` both when `record`'s kind is not in [`MAPPED_KINDS`] (the
/// importer's job to diagnose) and when the kind is mapped but this particular record
/// cannot be identified.
pub fn transform(record: &Value, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(payload) = record.get("payload") else {
        return Vec::new();
    };
    let Some(time) = timestamp(record) else {
        return Vec::new();
    };

    match kind(record).as_str() {
        "event_msg:user_message" => transform_user_message(payload, &time, ctx),
        "event_msg:agent_message" => transform_agent_message(payload, &time, ctx),
        "session_meta" => transform_session_meta(payload, &time, ctx),
        "response_item:custom_tool_call" | "response_item:function_call" => {
            transform_tool_call(payload, &time, ctx)
        }
        "response_item:custom_tool_call_output" | "response_item:function_call_output" => {
            transform_tool_output(payload, &time, ctx)
        }
        "event_msg:mcp_tool_call_end" => transform_mcp_tool_call(payload, &time, ctx),
        _ => Vec::new(),
    }
}

/// A stable identity for a record that carries none of its own.
///
/// Codex's `user_message` has no id field. Measured over the real corpus: the same
/// human action is re-announced within one file (4 groups, byte-identical payloads
/// at an identical timestamp on non-adjacent lines), so identity must collapse those
/// -- while `(file, timestamp)` alone collides 145 times, so it must not collapse
/// distinct text.
///
/// The hash covers the timestamp and the message only. `client_id` is excluded on
/// purpose: one real collision group differs in nothing else, and including it would
/// split one prompt into two.
fn content_identity(prefix: &str, timestamp: &str, body: &Value) -> String {
    let canonical = serde_json::to_string(body).unwrap_or_default();
    pseudonymize(prefix, &format!("{timestamp}|{canonical}"))
}

fn timestamp(record: &Value) -> Option<String> {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The [`Keystore`] key under which this record's session identities are registered:
/// the `session_id` the record names, or [`FILE_SESSION`] when it names none.
///
/// `session_id` is the *only* field read here. `session_meta` also carries an `id`, and
/// the importer's pre-scan does fall back to it for the older CLI versions that omit
/// `session_id` (4 of 328 files) -- but that fallback belongs there, not here, for two
/// reasons. It needs the whole-file view this transform does not have, because
/// `session_meta` is re-announced with a *different* `id` each time (2--3 distinct
/// values per file were measured) and only the *first* one may name the session; keying
/// on `id` here would split one session into three. And `id` on any other Codex record
/// means something else entirely -- on a tool call it is the `ctc_…` call id -- so a
/// blanket `id` fallback would leave every Codex tool event looking up a session that
/// does not exist. Deferring to [`FILE_SESSION`] gets the same answer with neither
/// hazard.
fn session_key(payload: &Value) -> &str {
    payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or(FILE_SESSION)
}

fn session_ref(payload: &Value, ctx: &Keystore) -> Option<String> {
    ctx.resolve("session", session_key(payload))
}

/// Workspace and repository for a record, both keyed by its *session*.
///
/// Not by the record's own `cwd` the way [`crate::claude_code_history`] does it: a
/// Codex record other than `session_meta`/`turn_context` has no `cwd` to key on, and
/// the session's answer is unambiguous anyway (`cwd` was measured never to vary within
/// one file), so there is no vote to lose by resolving it this way.
fn identity_refs(payload: &Value, ctx: &Keystore) -> (Option<String>, Option<String>) {
    let key = session_key(payload);
    (
        ctx.resolve("workspace", key),
        ctx.resolve("repository", key),
    )
}

fn transform_user_message(payload: &Value, time: &str, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(session) = session_ref(payload, ctx) else {
        return Vec::new();
    };
    let (workspace, repository) = identity_refs(payload, ctx);
    let identity = content_identity("umsg", time, payload.get("message").unwrap_or(&Value::Null));

    // Codex has no per-record subagent flag: a subagent writes its own rollout
    // file, so the fact lives in the *parent* file's sub_agent_activity and
    // reaches this file through the importer's cross-file pass. A prompt a
    // subagent typed to itself is not a human's attention, and 70 of them are on
    // the clock today.
    let origin = match ctx.resolve("codex_subagent_session", session_key(payload)) {
        Some(_) => "subagent",
        None => "human",
    };

    vec![ObservationDraft {
        event_type: "dev.cclog.prompt.submitted.v1".to_string(),
        subject: format!("session/{session}"),
        time: time.to_string(),
        traceparent: None,
        source_kind: SourceKind::Codex,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace,
        repository_ref: repository,
        correlation_cluster: None,
        dedupe_seed: vec![session, "prompt.submitted".to_string(), identity],
        // No `prompt_tokens`: a Codex transcript carries no token count for a human
        // turn, and the field is optional in the schema. Emitting the `"0"` bucket --
        // as the Claude Code historical path does -- would be a plausible-looking
        // default for something absent, and any bucket derived from the message
        // instead would leak the prompt's size out of a metadata-only ledger.
        data: json!({ "content_ref": null, "origin": origin }),
    }]
}

fn transform_agent_message(payload: &Value, time: &str, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(session) = session_ref(payload, ctx) else {
        return Vec::new();
    };
    let (workspace, repository) = identity_refs(payload, ctx);
    let identity = content_identity("amsg", time, payload.get("message").unwrap_or(&Value::Null));

    vec![ObservationDraft {
        event_type: "dev.cclog.response.completed.v1".to_string(),
        subject: format!("session/{session}"),
        time: time.to_string(),
        traceparent: None,
        source_kind: SourceKind::Codex,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace,
        repository_ref: repository,
        correlation_cluster: None,
        dedupe_seed: vec![session, "response.completed".to_string(), identity],
        // `unknown` -- a real member of the schema's `outcome` enum -- because nothing
        // this record carries establishes a verdict, and `succeeded` is a verdict.
        //
        // This field used to be hardcoded `succeeded`, justified by "a turn that
        // errored produces an `event_msg:error`/`stream_error` instead". The committed
        // corpus survey refutes that: `docs/source-inventory.md`'s Codex table
        // enumerates every record kind in 318 real files and contains **neither**
        // `event_msg:error` **nor** `event_msg:stream_error`. The kinds that do exist
        // for abnormal endings are `event_msg:turn_aborted` (28) and, for normal ones,
        // `event_msg:task_complete` (1,657).
        //
        // The same table also shows why an `agent_message` cannot carry a *turn's*
        // verdict even in principle: there are 16,197 of them against 1,725
        // `event_msg:user_message` records -- about nine per human turn. It is a record
        // of the model having said something, not of a turn having finished, and
        // whether the enclosing turn later aborted is not knowable from it.
        //
        // [`crate::claude_code_history`] emits `succeeded`/`failed` for this same field,
        // but it *derives* it: a Claude Code `assistant` record carries
        // `isApiErrorMessage` and `error`. The Codex payload has no surveyed
        // equivalent, so matching that value without matching its evidence would put a
        // fabricated verdict on 16,197 records per corpus -- and these rows are
        // aggregated across vendors, where a fabricated `succeeded` is indistinguishable
        // from a measured one. The same reasoning [`output_outcome`] already applies to
        // tool calls.
        //
        // What would upgrade this: a survey of `event_msg:agent_message`'s payload keys
        // for an error marker, or of whether `event_msg:turn_aborted` /
        // `event_msg:task_complete` can be joined to the turn a message belongs to.
        //
        // `response_tokens` is omitted for the same reason `prompt_tokens` is above: the
        // record carries no count, and absence is how this schema says NOT MEASURED.
        data: json!({ "outcome": "unknown", "content_ref": null }),
    }]
}

fn transform_session_meta(payload: &Value, time: &str, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(session) = session_ref(payload, ctx) else {
        // A `session.started` with no session identity says nothing, and its dedupe
        // seed would carry no discriminator at all -- two unresolved sessions would
        // collapse onto one row.
        return Vec::new();
    };
    let (workspace, repository) = identity_refs(payload, ctx);

    vec![ObservationDraft {
        event_type: "dev.cclog.session.started.v1".to_string(),
        subject: format!("session/{session}"),
        time: time.to_string(),
        traceparent: None,
        source_kind: SourceKind::Codex,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace,
        repository_ref: repository,
        correlation_cluster: None,
        // Deliberately carries no per-record discriminator, which is how "emit only for
        // the first `session_meta` per session" is expressed by a *pure* per-record
        // transform: `session_meta` is re-announced up to 30 times in one file, every
        // one of them produces this same seed, and the ledger collapses them onto the
        // first -- the earliest, which is the one whose timestamp a session start
        // should carry. The adapter cannot know which record is first without a
        // whole-file view it is not allowed to have.
        dedupe_seed: vec![session, "session.started".to_string()],
        data: json!({ "session_kind": session_kind(payload) }),
    }]
}

fn transform_tool_call(payload: &Value, time: &str, ctx: &Keystore) -> Vec<ObservationDraft> {
    let session = session_ref(payload, ctx);
    let (workspace, repository) = identity_refs(payload, ctx);
    let scope = tool_scope(payload, time, ctx, "tcal");

    vec![ObservationDraft {
        event_type: "dev.cclog.tool.started.v1".to_string(),
        subject: subject_for(session.as_deref(), &scope),
        time: time.to_string(),
        traceparent: None,
        source_kind: SourceKind::Codex,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace.clone(),
        repository_ref: repository,
        correlation_cluster: None,
        dedupe_seed: seed(session, "tool.started", &scope),
        data: json!({
            "tool_family": tool_family(payload.get("name").and_then(Value::as_str)),
            "workspace_ref": workspace,
            "content_ref": null,
        }),
    }]
}

fn transform_tool_output(payload: &Value, time: &str, ctx: &Keystore) -> Vec<ObservationDraft> {
    let session = session_ref(payload, ctx);
    let (workspace, repository) = identity_refs(payload, ctx);
    let scope = tool_scope(payload, time, ctx, "tout");

    // Closed against the *call* record's timestamp, which lives on an earlier, separate
    // line and therefore reaches this pure transform through the Keystore. `None` --
    // emitted as a literal `null` -- when the call is not in the same snapshot or the
    // interval runs backwards; never `0`, which is a real measurement.
    let duration_ms = call_key(payload)
        .and_then(|id| ctx.resolve("tool_started_at", id))
        .and_then(|started_at| crate::rfc3339::duration_ms(&started_at, time));
    let tool_family = call_key(payload)
        .and_then(|id| ctx.resolve("tool_family", id))
        .unwrap_or_else(|| "other".to_string());

    vec![ObservationDraft {
        event_type: "dev.cclog.tool.finished.v1".to_string(),
        subject: subject_for(session.as_deref(), &scope),
        time: time.to_string(),
        traceparent: None,
        source_kind: SourceKind::Codex,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace.clone(),
        repository_ref: repository,
        correlation_cluster: None,
        dedupe_seed: seed(session, "tool.finished", &scope),
        data: json!({
            "tool_family": tool_family,
            "outcome": output_outcome(payload),
            "duration_ms": duration_ms,
            "workspace_ref": workspace,
            "content_ref": null,
        }),
    }]
}

/// `mcp_tool_call_end` -> one `tool.finished`.
///
/// Self-contained, unlike [`transform_tool_call`]/[`transform_tool_output`]'s
/// call/output pair: this record carries the whole interaction -- including its own
/// `duration` -- on one line, so there is no `tool_started_at` to close against and no
/// separate `tool.started` half to emit. `tool_family` is always `"mcp"`: every record
/// this arm is reached for is one, by construction of the match in [`transform`].
///
/// `outcome` still goes through [`output_outcome`], which reads `success`/`status` --
/// neither of which this record shape carries, so it honestly reports `"unknown"`
/// rather than reading into `result` (tool output) to guess one.
fn transform_mcp_tool_call(payload: &Value, time: &str, ctx: &Keystore) -> Vec<ObservationDraft> {
    let session = session_ref(payload, ctx);
    let (workspace, repository) = identity_refs(payload, ctx);
    let scope = mcp_tool_scope(payload, time, ctx);
    let duration_ms = mcp_duration_ms(payload.get("duration"));

    vec![ObservationDraft {
        event_type: "dev.cclog.tool.finished.v1".to_string(),
        subject: subject_for(session.as_deref(), &scope),
        time: time.to_string(),
        traceparent: None,
        source_kind: SourceKind::Codex,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace.clone(),
        repository_ref: repository,
        correlation_cluster: None,
        dedupe_seed: seed(session, "tool.finished", &scope),
        data: json!({
            "tool_family": "mcp",
            "outcome": output_outcome(payload),
            "duration_ms": duration_ms,
            "workspace_ref": workspace,
            "content_ref": null,
        }),
    }]
}

/// The vendor id a tool record is keyed by: `id` when present, else `call_id`.
///
/// `tool-call-with-ids.shape.json`: most `custom_tool_call` records carry both, but a
/// minority carry only `call_id`, so `id` must never be assumed. A `*_output` record
/// carries only `call_id`, which is also what pairs it with its call.
fn call_key(payload: &Value) -> Option<&str> {
    payload
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("call_id").and_then(Value::as_str))
}

/// The opaque ref a tool event is scoped and deduped by.
///
/// Prefers the pseudonym the importer registered for this call's vendor id; falls back
/// to the same content-identity shape prompts use when the record carries no id at all
/// (or the pre-scan did not see it). The fallback still emits the event rather than
/// dropping it: real tool-execution evidence with a weaker identity is worth more than
/// silence, and the content tuple is not a weak identity -- it covers the timestamp and
/// the whole payload.
fn tool_scope(payload: &Value, time: &str, ctx: &Keystore, prefix: &str) -> String {
    call_key(payload)
        .and_then(|id| ctx.resolve("tool", id))
        .unwrap_or_else(|| content_identity(prefix, time, payload))
}

/// The opaque ref an `mcp_tool_call_end` is scoped and deduped by.
///
/// Prefers the same `"tool"` Keystore channel [`tool_scope`] resolves through, keyed on
/// `call_id` -- the field this record shares with every other tool kind -- so a future
/// pre-scan pass that learns to register MCP calls needs no change here.
///
/// Deliberately **not** [`tool_scope`] itself: that function's fallback is
/// [`content_identity`] over the *whole payload*, and this payload's `invocation`
/// carries `arguments` (the call's own input) alongside `result` (its output) --
/// content, in exactly the way a prompt is. Today's pre-scan does not observe
/// `mcp_tool_call_end` at all (see `CodexPreScan::observe`), so that fallback would not
/// be the rare miss it is for `custom_tool_call`/`function_call`; it would run on
/// effectively every one of these 6,220 records. So this hashes a value built field by
/// field instead: `call_id` plus `invocation`'s two identifiers, `server` and `tool`,
/// copied out by name. `arguments` and `result` never reach it, hashed or otherwise.
fn mcp_tool_scope(payload: &Value, time: &str, ctx: &Keystore) -> String {
    let call_id = payload.get("call_id").and_then(Value::as_str);
    if let Some(opaque) = call_id.and_then(|id| ctx.resolve("tool", id)) {
        return opaque;
    }
    let invocation = payload.get("invocation");
    let server = invocation
        .and_then(|i| i.get("server"))
        .and_then(Value::as_str);
    let tool = invocation
        .and_then(|i| i.get("tool"))
        .and_then(Value::as_str);
    content_identity(
        "mcpc",
        time,
        &json!({ "call_id": call_id, "server": server, "tool": tool }),
    )
}

/// Whole milliseconds from `mcp_tool_call_end`'s own `duration` -- a serialized Rust
/// `Duration`, `{secs, nanos}` -- the vendor's own measurement, not an interval this
/// module closes itself the way [`transform_tool_output`]'s `duration_ms` is.
///
/// `None` -- emitted as a literal `null`, never `0` -- when either half is missing or is
/// not a whole non-negative number. A `0` here would be indistinguishable from an MCP
/// call that genuinely returned within a millisecond, which is the exact mistake this
/// project has already removed seven fabricated instances of.
fn mcp_duration_ms(duration: Option<&Value>) -> Option<u64> {
    let duration = duration?;
    let secs = duration.get("secs").and_then(Value::as_u64)?;
    let nanos = duration.get("nanos").and_then(Value::as_u64)?;
    Some(secs.saturating_mul(1_000).saturating_add(nanos / 1_000_000))
}

/// `session/<ses>/tool/<scope>`, or `tool/<scope>` when the session is unresolved.
///
/// Not `session//tool/…`: an empty path segment would claim a session ref that does not
/// exist, and the schema's `subject` is a hierarchical path a consumer splits on `/`.
fn subject_for(session: Option<&str>, scope: &str) -> String {
    match session {
        Some(s) => format!("session/{s}/tool/{scope}"),
        None => format!("tool/{scope}"),
    }
}

/// The dedupe seed for a tool event. The session is a *component* rather than a
/// requirement: dropping the event when the session is unresolved would lose real tool
/// evidence, and `scope` already discriminates on its own.
fn seed(session: Option<String>, event: &str, scope: &str) -> Vec<String> {
    let mut parts = Vec::with_capacity(3);
    if let Some(s) = session {
        parts.push(s);
    }
    parts.push(event.to_string());
    parts.push(scope.to_string());
    parts
}

/// `interactive` / `headless` / `unknown` from `session_meta.payload.source`.
///
/// `source` was measured present in every observed `session_meta` fingerprint, so this
/// is a real reading rather than the Claude Code historical path's hardcoded
/// `"interactive"`. An unrecognized value falls to `"unknown"` -- a real member of the
/// schema's enum -- rather than being assumed interactive.
fn session_kind(payload: &Value) -> &'static str {
    match payload.get("source").and_then(Value::as_str) {
        Some("interactive") => "interactive",
        Some("exec") | Some("headless") => "headless",
        _ => "unknown",
    }
}

/// `succeeded` / `failed` / `unknown` for a tool output record.
///
/// Codex's output records carry no uniform success signal: `tool-call-with-ids.shape.json`
/// shows `custom_tool_call_output` as `{type, call_id, output}` and nothing more, so
/// `"unknown"` is the honest answer for that shape -- and it is a real member of the
/// schema's `outcome` enum, not a placeholder. Defaulting to `"succeeded"` would put a
/// fabricated verdict on every Codex tool call this project cannot actually judge, and
/// `tool.finished` outcomes are aggregated across vendors.
fn output_outcome(payload: &Value) -> &'static str {
    if let Some(ok) = payload.get("success").and_then(Value::as_bool) {
        return if ok { "succeeded" } else { "failed" };
    }
    match payload.get("status").and_then(Value::as_str) {
        Some("completed") => "succeeded",
        Some("failed") | Some("error") | Some("incomplete") => "failed",
        _ => "unknown",
    }
}

/// Normalize a Codex tool name into a canonical tool family.
///
/// Deliberately conservative: Codex's tool vocabulary does not overlap Claude Code's
/// (`shell` vs `Bash`), so this is a separate table rather than a shared one, and every
/// name not on it falls to `"other"` -- the schema's honest catch-all -- instead of
/// being guessed at from the name's shape. Task 4's per-kind gap report is what should
/// grow this table, not speculation.
///
/// Public for the same reason [`kind`] is: the importer's pre-scan has to record a
/// call's family for the *output* record on a later line to read back, and a second copy
/// of this table over there is how the started and finished halves of one tool call
/// would come to disagree about what tool it was.
pub fn tool_family(name: Option<&str>) -> &'static str {
    match name.unwrap_or("") {
        "shell" | "local_shell" | "exec_command" | "unified_exec" => "shell",
        "apply_patch" => "edit",
        "view_image" => "read",
        "web_search" => "web",
        n if n.starts_with("mcp__") => "mcp",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ks() -> Keystore {
        Keystore::new()
            .map("session", "sess-1", "ses_TEST")
            .map("workspace", "sess-1", "wsp_TEST")
            .map("repository", "sess-1", "rep_TEST")
            .map("tool", "call_1", "tol_TEST")
    }

    fn user_message(ts: &str, text: &str, client: &str) -> Value {
        json!({
            "type": "event_msg",
            "timestamp": ts,
            "payload": {
                "type": "user_message",
                "message": text,
                "client_id": client,
                "session_id": "sess-1"
            }
        })
    }

    /// A record of `kind` carrying every field that kind's arm reads, so "produced no
    /// drafts" can only mean "this module has no case for the kind" -- never "the
    /// fixture was missing a field the arm requires".
    fn fully_populated_record(kind: &str) -> Value {
        let (outer, inner) = match kind.split_once(':') {
            Some((outer, inner)) => (outer, Some(inner)),
            None => (kind, None),
        };
        let mut payload = match inner {
            Some(t) => json!({ "type": t }),
            None => json!({}),
        };
        payload["session_id"] = json!("sess-1");
        match inner.unwrap_or(outer) {
            "user_message" => {
                payload["message"] = json!("SYNTHETIC prompt");
                payload["client_id"] = json!("c1");
            }
            "agent_message" => payload["message"] = json!("SYNTHETIC response"),
            "session_meta" => {
                payload["id"] = json!("sess-1");
                payload["cwd"] = json!("/Users/dev/ghq/github.com/acme/api");
                payload["source"] = json!("interactive");
                payload["cli_version"] = json!("0.144.2");
            }
            "custom_tool_call" | "function_call" => {
                payload["id"] = json!("ctc_1");
                payload["call_id"] = json!("call_1");
                payload["name"] = json!("shell");
            }
            "custom_tool_call_output" | "function_call_output" => {
                payload["call_id"] = json!("call_1");
                payload["output"] = json!("SYNTHETIC");
            }
            // `message-without-id.shape.json`: `response_item:message` carries only
            // `content` / `role` / `type`. Populated so that if this kind were ever
            // mapped, the record would have everything such an arm could read -- an
            // under-populated fixture would make the unlisted-kind loop below pass for
            // the wrong reason.
            "message" => {
                payload["role"] = json!("user");
                payload["content"] = json!("SYNTHETIC prompt");
            }
            "token_count" => payload["info"] = json!({ "total_tokens": 1 }),
            "mcp_tool_call_end" => {
                payload["call_id"] = json!("mcpc_1");
                payload["connector_id"] = json!("conn_1");
                payload["plugin_id"] = json!("plugin_1");
                payload["link_id"] = json!("link_1");
                payload["app_name"] = json!("SYNTHETIC app");
                payload["action_name"] = json!("SYNTHETIC action");
                payload["invocation"] = json!({
                    "server": "synthetic-server",
                    "tool": "synthetic_tool",
                    "arguments": { "SYNTHETIC": "input" }
                });
                payload["duration"] = json!({ "secs": 1, "nanos": 0 });
                payload["result"] = json!({ "SYNTHETIC": "output" });
            }
            _ => {}
        }
        json!({
            "type": outer,
            "timestamp": "2026-08-01T00:00:00.000Z",
            "payload": payload,
        })
    }

    #[test]
    fn a_user_message_becomes_a_human_prompt_carrying_its_session_and_workspace() {
        let drafts = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &ks(),
        );
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.prompt.submitted.v1");
        assert_eq!(drafts[0].workspace_ref.as_deref(), Some("wsp_TEST"));
        assert_eq!(drafts[0].repository_ref.as_deref(), Some("rep_TEST"));
    }

    #[test]
    fn a_prompts_origin_is_subagent_only_when_the_keystore_names_its_session_one() {
        // `codex_subagent_session` is a cross-file fact the importer's pre-pass
        // registers -- never derived from anything on this one record -- under the
        // same keys `"session"` is, so it is looked up the same way: by
        // `session_key(payload)`, "sess-1" for every record `user_message` builds
        // here.
        let unregistered = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &ks(),
        );
        assert_eq!(unregistered[0].data["origin"], "human");

        let subagent_ks = ks().map("codex_subagent_session", "sess-1", "subagent");
        let subagent = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &subagent_ks,
        );
        assert_eq!(subagent[0].data["origin"], "subagent");

        // Presence is the whole fact -- the interface never reads the value. A match
        // on `Some("subagent")` instead of `Some(_)` would pass the assertion above
        // and still be wrong; this is what tells the two apart.
        let odd_value_ks = ks().map("codex_subagent_session", "sess-1", "anything-at-all");
        let odd = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &odd_value_ks,
        );
        assert_eq!(
            odd[0].data["origin"], "subagent",
            "presence in the keystore must be the whole fact, not a specific matched value"
        );
    }

    #[test]
    fn the_same_message_re_announced_in_one_file_collapses_to_one_dedupe_key() {
        // Measured on the real corpus: 4 groups of byte-identical user_message
        // payloads share a timestamp on non-adjacent lines. A human cannot type the
        // same text twice in the same millisecond, so these are one action recorded
        // several times -- exactly what dedupe is for.
        let a = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &ks(),
        );
        let b = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &ks(),
        );
        assert_eq!(a[0].dedupe_seed, b[0].dedupe_seed);

        // Those two records are byte-identical, so for a *pure* per-record transform
        // that assertion is a tautology -- it holds for any implementation at all,
        // including one that hashed the whole payload, and no mutation can make it
        // fail. What the test's name actually claims is that an announcement and a
        // re-announcement of one human action collapse, and the measured corpus says
        // those two can differ in payload fields outside `(timestamp, message)` (the
        // `client_id` case in the next test is the one real instance). So pin the
        // general form: identity is the timestamp and the message, and nothing else
        // on the payload moves it.
        let mut embellished = user_message("2026-08-01T00:00:00.000Z", "hello", "c1");
        embellished["payload"]["text_elements"] = json!([{ "type": "text" }]);
        embellished["payload"]["local_images"] = json!([]);
        let c = transform(&embellished, &ks());
        assert_eq!(
            a[0].dedupe_seed, c[0].dedupe_seed,
            "hashing anything beyond (timestamp, message) would keep both copies of \
             one human action"
        );
    }

    #[test]
    fn a_differing_client_id_does_not_split_one_action_into_two() {
        // One of the four real collision groups differs only in `client_id`. Including
        // it in the hash would keep both, double-counting a single prompt.
        let a = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &ks(),
        );
        let b = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c2"),
            &ks(),
        );
        assert_eq!(a[0].dedupe_seed, b[0].dedupe_seed);
    }

    #[test]
    fn two_different_prompts_at_the_same_timestamp_stay_two() {
        // `(file, timestamp)` collides 145 times in the real corpus, so the timestamp
        // alone cannot be the identity -- distinct text must stay distinct.
        let a = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &ks(),
        );
        let b = transform(
            &user_message("2026-08-01T00:00:00.000Z", "goodbye", "c1"),
            &ks(),
        );
        assert_ne!(a[0].dedupe_seed, b[0].dedupe_seed);
    }

    #[test]
    fn the_same_text_sent_at_a_different_time_stays_two() {
        let a = transform(
            &user_message("2026-08-01T00:00:00.000Z", "hello", "c1"),
            &ks(),
        );
        let b = transform(
            &user_message("2026-08-01T00:05:00.000Z", "hello", "c1"),
            &ks(),
        );
        assert_ne!(a[0].dedupe_seed, b[0].dedupe_seed);
    }

    #[test]
    fn no_draft_carries_the_prompt_text_or_its_length() {
        // The ledger stays metadata-only. A hash may leave the adapter; the text and
        // anything that leaks its size may not.
        let drafts = transform(
            &user_message(
                "2026-08-01T00:00:00.000Z",
                "a very distinctive secret phrase",
                "c1",
            ),
            &ks(),
        );
        let rendered = serde_json::to_string(&drafts[0].data).unwrap()
            + &drafts[0].dedupe_seed.join("|")
            + &drafts[0].subject;
        assert!(!rendered.contains("distinctive"));
        assert!(!rendered.contains("secret"));

        // ...and "or its length": the plan's own assertions above only pin the text,
        // so without this the name over-claims (a `prompt_tokens` bucket derived from
        // the prompt's size would pass every assertion above). Everything except the
        // deliberate one-way hash must be invariant to how long the prompt was.
        let longer = transform(
            &user_message(
                "2026-08-01T00:00:00.000Z",
                "a much longer prompt, several times the length of the one above, so \
                 that any size-derived field would have to take a different value",
                "c1",
            ),
            &ks(),
        );
        assert_eq!(
            drafts[0].data, longer[0].data,
            "a size-derived field (e.g. a prompt_tokens bucket) would differ here"
        );
        assert_eq!(drafts[0].subject, longer[0].subject);
    }

    #[test]
    fn a_tool_call_with_only_a_call_id_still_gets_a_stable_identity() {
        // tool-call-with-ids.shape.json: most records carry both `id` and `call_id`,
        // a minority only `call_id`, so `id` must not be assumed.
        let record = json!({
            "type": "response_item",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "payload": { "type": "custom_tool_call", "call_id": "call_1", "name": "shell" }
        });
        let drafts = transform(&record, &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.tool.started.v1");
        assert!(!drafts[0].dedupe_seed.is_empty());

        // ...and "stable" means something. Non-emptiness alone is trivially true of
        // every possible implementation, including one that ignored `call_id` and
        // hashed the record's content -- which is precisely the thing that would not be
        // stable. The identity must be the ref the importer registered for this call
        // id, and must therefore survive everything about the record that is not the
        // call id.
        assert_eq!(
            drafts[0].dedupe_seed,
            vec!["tool.started".to_string(), "tol_TEST".to_string()],
            "the identity must come from the registered ref for `call_id`"
        );
        let later = json!({
            "type": "response_item",
            "timestamp": "2026-08-01T09:99:99.000Z",
            "payload": { "type": "function_call", "call_id": "call_1", "name": "apply_patch" }
        });
        assert_eq!(
            transform(&later, &ks())[0].dedupe_seed,
            drafts[0].dedupe_seed,
            "one call keeps one identity across a different timestamp, tool name and \
             record kind"
        );
    }

    #[test]
    fn an_unmapped_kind_produces_no_draft_so_the_importer_can_gap_it() {
        let record = json!({
            "type": "event_msg",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "payload": { "type": "token_count", "input": 1 }
        });
        assert!(transform(&record, &ks()).is_empty());
    }

    #[test]
    fn every_mapped_kind_produces_drafts_and_unlisted_kinds_produce_none() {
        // Pins MAPPED_KINDS against the match arms in both directions that are
        // observable from outside: everything listed yields at least one draft, and
        // known-unlisted kinds yield none.
        for kind in MAPPED_KINDS {
            let record = fully_populated_record(kind);
            assert!(
                !transform(&record, &ks()).is_empty(),
                "{kind} is listed as mapped but produced no draft"
            );
        }
        for kind in [
            "event_msg:token_count",
            "world_state",
            "compacted",
            // The two whose *exclusion* is load-bearing, and which the plan's original
            // sample omitted. `response_item:agent_message` duplicates
            // `event_msg:agent_message` for the same turn (1,379 records against
            // 16,197 in `docs/source-inventory.md`), so mapping it double-counts every
            // Codex response -- with distinct dedupe keys, since the duplicate sits on
            // its own line at its own timestamp, so nothing collapses them. Adding a
            // match arm for it while resolving the documented `response_item:message`
            // question is a plausible accident; this is what makes it fail.
            "response_item:agent_message",
            "response_item:message",
        ] {
            let record = fully_populated_record(kind);
            assert!(
                transform(&record, &ks()).is_empty(),
                "{kind} is not in MAPPED_KINDS but produced a draft"
            );
        }
    }

    #[test]
    fn a_codex_response_reports_an_unknown_outcome_rather_than_assuming_success() {
        // A Codex `agent_message` carries no verdict, and `succeeded` is a verdict.
        // `docs/source-inventory.md` surveys 318 real files and lists neither
        // `event_msg:error` nor `event_msg:stream_error` -- the kinds the old hardcoded
        // `succeeded` was justified by -- and shows 16,197 `agent_message` records
        // against 1,725 human turns, so one of them is not a turn outcome to begin
        // with. `unknown` is a real member of the schema's enum, and these rows are
        // aggregated with Claude Code's, which *derives* this field from
        // `isApiErrorMessage`/`error`.
        let record = json!({
            "type": "event_msg",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "payload": {
                "type": "agent_message",
                "message": "SYNTHETIC response",
                "session_id": "sess-1",
            },
        });
        let drafts = transform(&record, &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.response.completed.v1");
        assert_eq!(
            drafts[0].data["outcome"], "unknown",
            "a verdict this adapter cannot derive must be reported as unknown, not \
             assumed to be success"
        );
    }

    #[test]
    fn an_object_valued_message_hashes_its_keys_in_sorted_order() {
        // `content_identity` hashes `serde_json::to_string(message)`, and a dedupe key
        // is write-once: if that serialization ever changed, every historical Codex
        // prompt would take a new identity and re-import as a duplicate row rather than
        // collapsing onto the one already there. serde_json's `Value` is BTreeMap-backed
        // by default, so key order is normalized to sorted -- but that is a *default
        // feature*, and cargo features are additive across a whole dependency graph: one
        // future dependency enabling `preserve_order` would flip it silently.
        //
        // So pin the bytes that are hashed, not merely that two equal `Value`s agree
        // (which they do by construction today, and would prove nothing). Parsed from
        // text with the keys out of order, exactly as a transcript could write them.
        let ts = "2026-08-01T00:00:00.000Z";
        let record: Value = serde_json::from_str(&format!(
            r#"{{"type":"event_msg","timestamp":"{ts}","payload":{{"type":"user_message",
                "message":{{"b":1,"a":2}},"session_id":"sess-1"}}}}"#
        ))
        .expect("parse");
        let drafts = transform(&record, &ks());
        assert_eq!(
            drafts[0].dedupe_seed[2],
            pseudonymize("umsg", &format!(r#"{ts}|{{"a":2,"b":1}}"#)),
            "the identity must hash the message with its keys sorted, whatever order \
             the transcript wrote them in -- and over exactly `timestamp|message`"
        );
    }

    /// A synthetic `mcp_tool_call_end` record carrying every field the real shape
    /// does. `duration` is a parameter so callers can exercise both a well-formed
    /// `{secs, nanos}` object and the malformed shapes [`mcp_duration_ms`] must turn
    /// into `null`.
    fn mcp_tool_call_end(call_id: &str, duration: Value) -> Value {
        json!({
            "type": "event_msg",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "payload": {
                "type": "mcp_tool_call_end",
                "call_id": call_id,
                "connector_id": "conn_1",
                "plugin_id": "plugin_1",
                "link_id": "link_1",
                "app_name": "SYNTHETIC app",
                "action_name": "SYNTHETIC action",
                "invocation": {
                    "server": "synthetic-server",
                    "tool": "synthetic_tool",
                    "arguments": { "SYNTHETIC": "input" }
                },
                "duration": duration,
                "result": { "SYNTHETIC": "output" }
            }
        })
    }

    #[test]
    fn an_mcp_tool_call_end_becomes_one_finished_mcp_tool_event() {
        let record = mcp_tool_call_end("call_9", json!({ "secs": 2, "nanos": 500_000_000 }));
        let drafts = transform(&record, &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.tool.finished.v1");
        assert_eq!(drafts[0].data["tool_family"], "mcp");
        assert_eq!(
            drafts[0].data["duration_ms"], 2500,
            "duration.secs * 1000 + duration.nanos / 1_000_000"
        );
    }

    #[test]
    fn an_mcp_call_with_a_registered_id_resolves_through_the_existing_tool_channel() {
        // `ks()` registers "call_1" under the same `"tool"` Keystore key
        // `custom_tool_call`/`function_call` resolve through via `call_key`/`tool_scope`
        // -- proving an MCP call reuses that channel rather than a second one, exactly
        // as `a_tool_call_with_only_a_call_id_still_gets_a_stable_identity` proves it
        // for `custom_tool_call`.
        let record = mcp_tool_call_end("call_1", json!({ "secs": 1, "nanos": 0 }));
        let drafts = transform(&record, &ks());
        assert_eq!(
            drafts[0].dedupe_seed,
            vec!["tool.finished".to_string(), "tol_TEST".to_string()],
            "a registered call id must resolve to the same ref other tool kinds use"
        );
    }

    #[test]
    fn mcp_duration_is_null_never_zero_when_unmeasurable() {
        for bad in [
            json!({ "nanos": 0 }),              // secs missing
            json!({ "secs": 1 }),               // nanos missing
            json!({ "secs": "1", "nanos": 0 }), // wrong type
            json!("2s"),                        // not an object at all
            Value::Null,                        // absent
        ] {
            let record = mcp_tool_call_end("call_9", bad.clone());
            let drafts = transform(&record, &ks());
            assert_eq!(
                drafts[0].data["duration_ms"],
                Value::Null,
                "duration {bad:?} is unmeasurable and must yield null, never 0"
            );
        }
    }

    #[test]
    fn a_genuinely_zero_mcp_duration_is_reported_as_zero_not_folded_into_unmeasurable() {
        // `{secs: 0, nanos: 0}` is present and parseable -- a real measurement of a
        // call that returned within a millisecond -- and must be told apart from the
        // `null` the test above pins for a duration that could not be read at all.
        let record = mcp_tool_call_end("call_9", json!({ "secs": 0, "nanos": 0 }));
        let drafts = transform(&record, &ks());
        assert_eq!(drafts[0].data["duration_ms"], 0);
    }

    #[test]
    fn changing_arguments_or_result_never_changes_an_unregistered_calls_identity_or_data() {
        // "call_9" is deliberately not registered in `ks()`, so this exercises
        // `mcp_tool_scope`'s fallback -- the path a real import takes for every MCP
        // call today, since `CodexPreScan::observe` does not (yet) observe
        // `mcp_tool_call_end`. If that fallback ever changed to hash the whole payload
        // the way `tool_scope`'s does (or `arguments`/`result` leaked into `data`),
        // this is what would catch it: the two records below differ only in
        // `invocation.arguments` and `result`, and nothing about the draft may move.
        let quiet = mcp_tool_call_end("call_9", json!({ "secs": 1, "nanos": 0 }));
        let mut loud = quiet.clone();
        loud["payload"]["invocation"]["arguments"] =
            json!("a much longer and totally different secret input payload");
        loud["payload"]["result"] =
            json!("a much longer and totally different secret result payload");

        let a = transform(&quiet, &ks());
        let b = transform(&loud, &ks());
        assert_eq!(
            a[0].dedupe_seed, b[0].dedupe_seed,
            "arguments/result must never affect an MCP call's identity"
        );
        assert_eq!(
            a[0].data, b[0].data,
            "arguments/result must never affect an MCP call's data"
        );

        let rendered = serde_json::to_string(&a[0].data).unwrap()
            + &a[0].dedupe_seed.join("|")
            + &a[0].subject;
        assert!(!rendered.contains("secret"));
    }
}
