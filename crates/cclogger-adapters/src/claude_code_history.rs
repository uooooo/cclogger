//! Claude Code historical adapter: transcript JSONL records → observation drafts.
//!
//! This is a *different* source shape from [`crate::claude_code`], which handles hook
//! payloads (`hook_event_name`, `session_id`, `ts`, ...). Historical transcripts are the
//! JSONL files Claude Code writes to `~/.claude/projects/**/*.jsonl`: every record
//! carries a stable `uuid`, a vendor `type` discriminant, and (for the kinds this
//! module maps) a `timestamp` and `sessionId`. The two shapes are kept in separate
//! modules deliberately -- contorting one into the other would make both harder to
//! read and to test independently. See `docs/source-inventory.md` for the full
//! per-kind survey this module's coverage decisions are based on, and
//! `adapters/claude-code/shapes/*.shape.json` for fixtures that reproduce the real
//! record shapes without any real content.
//!
//! # Contract
//!
//! Same purity contract as [`crate::claude_code`]: never mint an id, read a clock, or
//! decide a profile -- those are runtime concerns applied later by
//! [`cclogger_domain::ObservationDraft::finalize`]. The one deliberate deviation from that
//! module's literal `Option<ObservationDraft>` return type is the *arity*: a single
//! historical record can produce more than one observation (an `assistant` record can
//! carry several `tool_use` blocks in one message), so [`transform`] returns
//! `Vec<ObservationDraft>` (0..N), matching the architecture design's own framing of an
//! adapter ("source record から 0..N 件の `ObservationDraft`... を返す純変換",
//! `docs/superpowers/specs/2026-07-29-history-import-slice-design.md` §6) rather than
//! the hook adapter's necessarily-1:1 case.
//!
//! # Record kinds mapped, and why
//!
//! Only three of the 22 real `type` values in `docs/source-inventory.md` are mapped
//! here: `user`, `assistant`, `attachment`. Every other kind (`last-prompt`, `mode`,
//! `permission-mode`, `ai-title`, `queue-operation`, `file-history-snapshot`,
//! `pr-link`, `agent-name`, `file-history-delta`, `started`, `result`,
//! `bridge-session`, `custom-title`, `relocated`, `worktree-state`, `frame-link`,
//! `fork-context-ref`, `agent-setting`, and -- deliberately, see below -- `system`) is
//! left for the importer to turn into a `dev.cclog.source.gap.v1` marker rather than
//! being silently dropped (design doc §8, and the acceptance criteria for issue #13).
//! [`MAPPED_KINDS`] is what the importer checks an unmapped record's `type` against to
//! tell "this kind has no case here at all" (a gap) apart from "this kind is
//! understood, but this particular record does not produce an event" (not a gap --
//! e.g. a sidechain-authored, non-tool-result `user` record).
//!
//! `system` is deliberately deferred, not because it is technically unmappable: real
//! archive samples show subtypes like `turn_duration` and `compact_boundary` that
//! could feed a later WorkBlock/duration projection, but that is out of this issue's
//! scope ("do not build activity slices"). Leaving it as a diagnosed gap keeps that
//! door open without doing the work now.
//!
//! ## Human prompt vs. tool result (the measured trap)
//!
//! `type == "user"` is the single most important discriminator this module gets
//! right. Filtering on `isSidechain != true && isMeta != true` alone counted 608
//! "human prompts" on the measured day where the true figure was 82 -- tool results
//! are delivered back to the model as `type: "user"` records whose `isSidechain` /
//! `isMeta` are false or simply absent (see
//! `docs/superpowers/specs/expected-output-2026-07-26.md` §2.1). [`tool_result_of`]
//! is checked *before* the sidechain/meta check for exactly this reason: a record
//! carrying a `tool_result` content block (equivalently, `toolUseResult != null`; the
//! two were 100% correlated across the 526 measured tool-result records) is always
//! tool-execution evidence, never a human prompt, regardless of what its
//! `isSidechain`/`isMeta` flags say.
//!
//! Sidechain-authored `user` records that are *not* tool results (a subagent's own
//! internal turn) are, for now, silently not mapped to any observation -- they are
//! neither human signal nor (yet) a distinct "subagent activity" event in schema v0.
//! This is a deliberate, scoped-down choice, not an oversight: the orchestrating side
//! of a subagent dispatch already produces a `tool.started`/`tool.finished` pair (the
//! `Task`-family tool call and its result), which is enough for M1. By contrast,
//! `assistant` records carrying `isSidechain: true` (a subagent's own responses and
//! tool calls) *are* mapped -- the design brief frames sidechain records as "parent
//! session の agent activity evidence", and unlike the human-prompt case there is no
//! measured contamination risk to guard against here.
//!
//! ## Tool durations
//!
//! `dev.cclog.tool.finished.v1`'s `duration_ms` is a real measurement here, closed
//! against the `tool_use` record that started the call: that record is a *different*
//! line of the transcript, so its timestamp reaches this pure transform through the
//! [`Keystore`] (`tool_started_at`, registered by the importer's pre-scan) rather than
//! by this module reading the file or a clock.
//!
//! When the start time is unavailable -- a tool result whose `tool_use` line is not
//! in the same snapshot, an unresolvable `tool_use_id`, or an interval that runs
//! backwards -- the field is `null`, never `0`. The two are not interchangeable: `0`
//! is a legitimate measurement of a tool that returned within a millisecond, and the
//! hook adapter ([`crate::claude_code`]) emits real durations into the same field, so
//! a fabricated `0` here would be indistinguishable from a measurement and would bias
//! every aggregate mixing the two channels toward zero.
//!
//! ## Token counts
//!
//! `prompt_tokens` and `response_tokens` follow the same rule as `duration_ms`, for the
//! same reason: the `"0"` bucket is a *measurement* (a turn that really did cost fewer
//! than one token's worth of anything), so a count that was never measured is left
//! absent instead. Both fields are optional in the schema, and absence is how a
//! metadata-only ledger says "not measured" for a count bucket -- there is no `null`
//! member of the bucket enum to say it with.
//!
//! A `user` transcript record carries no token count of any kind, so
//! `dev.cclog.prompt.submitted.v1` never carries `prompt_tokens` from this adapter. An
//! `assistant` record usually carries `message.usage.output_tokens`, so
//! `response_tokens` is present when that field is -- and absent, not `"0"`, when it is
//! not. The Codex adapter ([`crate::codex_history`]) omits both for the same reason,
//! and the hook adapter ([`crate::claude_code`]) omits them when its payload carries no
//! count: all three write into one ledger, so "0" has to mean the same thing in every
//! row of it.
//!
//! ## Session lifecycle
//!
//! Historical transcripts carry no dedicated "session started"/"session ended" record
//! kind analogous to the hook adapter's `SessionStart`/`SessionEnd` events. The one
//! reliable, purely-per-record signal available is `type == "attachment"` wrapping
//! `attachment.type == "hook_success"` with `attachment.hookEvent == "SessionStart"` --
//! present when the installation has a `SessionStart` hook configured (as this
//! project's own archive does, via the superpowers plugin). This is mapped to
//! `dev.cclog.session.started.v1`. It is honestly install-dependent, not universal
//! vanilla Claude Code behavior: an installation with no `SessionStart` hook
//! configured will simply never produce this signal for a given session, which is a
//! real coverage limitation, not a fabrication -- documented here rather than papered
//! over with a heuristic (e.g. "first record with `parentUuid == null`") that real
//! archive samples show is *not* reliably the first human message once any hook is
//! configured (the hook's own attachment record becomes the tree root instead).
//! `dev.cclog.session.ended.v1` is not mapped at all: no per-record signal for "this
//! is the terminal record of a session" was found in the real corpus (the `Stop`
//! hookEvent that exists is a *turn* stop, not a session end, and is vanishingly rare
//! in the sampled archive).

use crate::Keystore;
use cclogger_domain::{IntegrityState, ObservationDraft, PrivacyClass, SourceKind};
use serde_json::{Value, json};

pub const SOURCE_VERSION: &str = "claude-code-transcript/1";
pub const ADAPTER_VERSION: &str = "claude-code-history/0.0.0";

/// `type` values this module has a case for at all. An importer should treat any
/// *other* `type` value as an unmapped record kind (a gap, design doc §8) -- but a
/// record whose kind *is* in this list producing zero drafts is not a gap: it means
/// the kind is understood and this particular instance legitimately carries no
/// reportable event (e.g. a sidechain, non-tool-result `user` record).
pub const MAPPED_KINDS: &[&str] = &["user", "assistant", "attachment"];

/// Transform one historical Claude Code transcript record into 0..N observation
/// drafts. Returns an empty `Vec` both when `record`'s kind is not in
/// [`MAPPED_KINDS`] (the importer's job to diagnose) and when the kind is mapped but
/// this particular record carries no event (not a gap).
pub fn transform(record: &Value, ctx: &Keystore) -> Vec<ObservationDraft> {
    match record.get("type").and_then(Value::as_str) {
        Some("user") => transform_user(record, ctx),
        Some("assistant") => transform_assistant(record, ctx),
        Some("attachment") => transform_attachment(record, ctx),
        _ => Vec::new(),
    }
}

fn session_ref(record: &Value, ctx: &Keystore) -> Option<String> {
    let session_id = record
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| record.get("session_id").and_then(Value::as_str))?;
    ctx.resolve("session", session_id)
}

/// The raw vendor session id on a record, if it names one.
fn session_id(record: &Value) -> Option<&str> {
    record
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| record.get("session_id").and_then(Value::as_str))
}

/// Resolve one identity kind for a record.
///
/// A record that carries a `cwd` is identified by that cwd and nothing else -- if it
/// resolves to nothing (a path outside the ghq tree), the record stays unresolved.
/// Borrowing the session's identity there would merge it into a repository it was
/// never in, which design §10 rule 4 forbids.
///
/// The session fallback exists for records that carry no `cwd` at all; the importer
/// resolves those from a session-level majority vote. It reaches only the kinds this
/// adapter maps -- `user`, `assistant`, `attachment` -- since a record of any other
/// kind never gets here: the importer diagnoses it as a gap, which carries no identity
/// at all.
fn identity_ref(record: &Value, ctx: &Keystore, by_cwd: &str, by_session: &str) -> Option<String> {
    match record.get("cwd").and_then(Value::as_str) {
        Some(cwd) => ctx.resolve(by_cwd, cwd),
        None => session_id(record).and_then(|sid| ctx.resolve(by_session, sid)),
    }
}

/// The observation's `cclogworkspaceref`: the workspace a record belongs to -- the
/// main checkout, or (distinct from it) a git worktree.
fn workspace_ref(record: &Value, ctx: &Keystore) -> Option<String> {
    identity_ref(record, ctx, "workspace", "session_workspace")
}

/// The repository a record belongs to -- the project, shared by every worktree of it.
fn repository_ref(record: &Value, ctx: &Keystore) -> Option<String> {
    identity_ref(record, ctx, "repository", "session_repository")
}

fn timestamp(record: &Value) -> Option<String> {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn record_uuid(record: &Value) -> Option<&str> {
    record.get("uuid").and_then(Value::as_str)
}

/// Whether `record` (a `type == "user"` record) is a tool result, and the
/// `tool_use_id` it names if one could be extracted.
///
/// `Some(None)` -- tool result confirmed (via `toolUseResult != null`) but no
/// `tool_use_id` found in a `content` block -- should not occur in practice (the
/// design doc's measured corpus found the two signals 100% correlated), but is kept
/// distinct from `None` (not a tool result at all) rather than collapsed into it.
fn tool_result_of(record: &Value) -> Option<Option<String>> {
    let content_items = record
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array);

    let block_id = content_items.and_then(|items| {
        items.iter().find_map(|item| {
            if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                Some(
                    item.get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                )
            } else {
                None
            }
        })
    });
    if let Some(id) = block_id {
        return Some(id);
    }

    let has_tool_use_result = record
        .get("toolUseResult")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if has_tool_use_result {
        return Some(None);
    }
    None
}

/// `succeeded`/`failed`/`unknown` for a tool result record: prefers the `tool_result`
/// content block's own `is_error`, falling back to `toolUseResult.exit_code` (present
/// for shell-family tools) when the block omits it.
fn tool_result_outcome(record: &Value) -> &'static str {
    let from_block = record
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                    item.get("is_error").and_then(Value::as_bool)
                } else {
                    None
                }
            })
        });
    if let Some(is_error) = from_block {
        return if is_error { "failed" } else { "succeeded" };
    }
    match record
        .get("toolUseResult")
        .and_then(|r| r.get("exit_code"))
        .and_then(Value::as_i64)
    {
        Some(0) => "succeeded",
        Some(_) => "failed",
        None => "unknown",
    }
}

fn transform_user(record: &Value, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(session) = session_ref(record, ctx) else {
        return Vec::new();
    };
    let Some(time) = timestamp(record) else {
        return Vec::new();
    };
    let Some(uuid) = record_uuid(record) else {
        return Vec::new();
    };
    let workspace = workspace_ref(record, ctx);
    let repository = repository_ref(record, ctx);

    if let Some(tool_use_id) = tool_result_of(record) {
        let outcome = tool_result_outcome(record);
        let tool_family = tool_use_id
            .as_deref()
            .and_then(|id| ctx.resolve("tool_family", id))
            .unwrap_or_else(|| "other".to_string());
        let tool_ref = tool_use_id
            .as_deref()
            .and_then(|id| ctx.resolve("tool", id));
        // A historical tool result carries no duration of its own: the interval has
        // to be closed against the `tool_use` record that started it, which lives on
        // an *earlier, separate* line. The importer's pre-scan registers that record's
        // timestamp under `tool_started_at`, so the value arrives here the same way
        // every other cross-record fact does -- through the Keystore -- and this
        // function stays a pure transform of one record.
        //
        // `None` when the start time is unavailable or the interval does not make
        // sense, and it is emitted as a literal `null`: `duration_ms` is the field
        // this project exists to populate honestly, and a `0` there is
        // indistinguishable from a tool that really did return within a millisecond.
        let duration_ms = tool_use_id
            .as_deref()
            .and_then(|id| ctx.resolve("tool_started_at", id))
            .and_then(|started_at| crate::rfc3339::duration_ms(&started_at, &time));
        let (subject, scope) = match &tool_ref {
            Some(t) => (format!("session/{session}/tool/{t}"), t.clone()),
            // No resolvable tool_use id (defensive -- see `tool_result_of`'s doc
            // comment): still emit the event rather than silently dropping real tool
            // execution evidence, anchored on this record's own uuid instead.
            None => (format!("session/{session}"), uuid.to_string()),
        };
        return vec![ObservationDraft {
            event_type: "dev.cclog.tool.finished.v1".to_string(),
            subject,
            time,
            traceparent: None,
            source_kind: SourceKind::ClaudeCode,
            source_version: SOURCE_VERSION.to_string(),
            adapter_version: ADAPTER_VERSION.to_string(),
            privacy_class: PrivacyClass::T1Structured,
            integrity_state: IntegrityState::Ok,
            workspace_ref: workspace.clone(),
            repository_ref: repository,
            correlation_cluster: None,
            dedupe_seed: vec![session, "tool.finished".to_string(), scope],
            data: json!({
                "tool_family": tool_family,
                "outcome": outcome,
                "duration_ms": duration_ms,
                "workspace_ref": workspace,
                "content_ref": null,
            }),
        }];
    }

    let is_sidechain = record
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_meta = record
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_sidechain || is_meta {
        return Vec::new();
    }

    vec![ObservationDraft {
        event_type: "dev.cclog.prompt.submitted.v1".to_string(),
        subject: format!("session/{session}"),
        time,
        traceparent: None,
        source_kind: SourceKind::ClaudeCode,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace,
        repository_ref: repository,
        correlation_cluster: None,
        dedupe_seed: vec![session, "prompt.submitted".to_string(), uuid.to_string()],
        // No `prompt_tokens`: a `user` transcript record carries no token count at all,
        // so there is nothing to bucket. This used to emit the `"0"` bucket, which made
        // every one of the 2,845 prompts in the real corpus claim a measured zero --
        // indistinguishable, in the same ledger, from a hook payload that really did
        // report `prompt_tokens: 0`. The field is optional in the schema; counting the
        // prompt's own size here instead would leak it out of a metadata-only ledger.
        data: json!({
            "content_ref": null,
        }),
    }]
}

fn transform_assistant(record: &Value, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(session) = session_ref(record, ctx) else {
        return Vec::new();
    };
    let Some(time) = timestamp(record) else {
        return Vec::new();
    };
    let Some(uuid) = record_uuid(record) else {
        return Vec::new();
    };
    let workspace = workspace_ref(record, ctx);
    let repository = repository_ref(record, ctx);

    let is_error = record
        .get("isApiErrorMessage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || record.get("error").map(|v| !v.is_null()).unwrap_or(false);
    let outcome = if is_error { "failed" } else { "succeeded" };
    let response_tokens = bucket(
        record
            .get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64),
    );

    // Present only when the record carried a count. An `assistant` record without
    // `message.usage.output_tokens` (an API-error record, or a shape this adapter has
    // not seen) did not measure zero tokens -- it measured nothing.
    let mut data = json!({
        "outcome": outcome,
        "content_ref": null,
    });
    if let Some(response_tokens) = response_tokens {
        data["response_tokens"] = json!(response_tokens);
    }

    let mut out = vec![ObservationDraft {
        event_type: "dev.cclog.response.completed.v1".to_string(),
        subject: format!("session/{session}"),
        time: time.clone(),
        traceparent: None,
        source_kind: SourceKind::ClaudeCode,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace.clone(),
        repository_ref: repository.clone(),
        correlation_cluster: None,
        dedupe_seed: vec![
            session.clone(),
            "response.completed".to_string(),
            uuid.to_string(),
        ],
        data,
    }];

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
            let Some(tool_ref) = ctx.resolve("tool", tool_use_id) else {
                // Defensive only: the importer's own pre-scan registers every
                // tool_use id from this same record before calling `transform`, so
                // this should be unreachable in practice.
                continue;
            };
            let name = item.get("name").and_then(Value::as_str);
            out.push(ObservationDraft {
                event_type: "dev.cclog.tool.started.v1".to_string(),
                subject: format!("session/{session}/tool/{tool_ref}"),
                time: time.clone(),
                traceparent: None,
                source_kind: SourceKind::ClaudeCode,
                source_version: SOURCE_VERSION.to_string(),
                adapter_version: ADAPTER_VERSION.to_string(),
                privacy_class: PrivacyClass::T1Structured,
                integrity_state: IntegrityState::Ok,
                workspace_ref: workspace.clone(),
                repository_ref: repository.clone(),
                correlation_cluster: None,
                // Seeded with the opaque ref, not the raw `toolu_…` vendor id, so
                // this event's dedupe key is built the same way its `tool.finished`
                // pair's is. Both are equally stable, but `cclogdedupekey` is
                // write-once forever: embedding a raw vendor id in one half of a pair
                // and not the other is not something a later release can undo.
                dedupe_seed: vec![
                    session.clone(),
                    "tool.started".to_string(),
                    tool_ref.clone(),
                ],
                data: json!({
                    "tool_family": tool_family(name),
                    "workspace_ref": workspace,
                    "content_ref": null,
                }),
            });
        }
    }

    out
}

fn transform_attachment(record: &Value, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(session) = session_ref(record, ctx) else {
        return Vec::new();
    };
    let Some(time) = timestamp(record) else {
        return Vec::new();
    };
    let Some(uuid) = record_uuid(record) else {
        return Vec::new();
    };

    let attachment = record.get("attachment");
    let is_session_start = attachment
        .and_then(|a| a.get("type"))
        .and_then(Value::as_str)
        == Some("hook_success")
        && attachment
            .and_then(|a| a.get("hookEvent"))
            .and_then(Value::as_str)
            == Some("SessionStart");
    if !is_session_start {
        return Vec::new();
    }

    // Both identities resolve the ordinary way. This function used to hardcode
    // `workspace_ref: None`, which was harmless only while `repository_ref` was
    // hardcoded `None` too -- populating one and not the other would leave
    // `session.started` claiming a repository but no workspace inside it, a shape no
    // other event has and one a report would have to special-case. `attachment`
    // records do carry a `cwd` (all 5,161 of them in a 400-file sample of the real
    // corpus), so there was never a reason for the asymmetry.
    let workspace = workspace_ref(record, ctx);
    let repository = repository_ref(record, ctx);

    vec![ObservationDraft {
        event_type: "dev.cclog.session.started.v1".to_string(),
        subject: format!("session/{session}"),
        time,
        traceparent: None,
        source_kind: SourceKind::ClaudeCode,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        workspace_ref: workspace,
        repository_ref: repository,
        correlation_cluster: None,
        dedupe_seed: vec![session, "session.started".to_string(), uuid.to_string()],
        data: json!({ "session_kind": "interactive" }),
    }]
}

/// Coarse magnitude bucket for a count that was measured; `None` when it was not.
/// Exact sensitive counts are never stored on a T1 row.
///
/// `None` in, `None` out -- deliberately *not* the `"0"` bucket. `"0"` is a real
/// measurement, and the caller drops the field entirely when this returns `None`, which
/// is how "not measured" is said for a count (the bucket enum has no `null` member, and
/// both count fields are optional in the schema).
///
/// Duplicated from `crate::claude_code` rather than shared: both are tiny, and the
/// two adapters are deliberately kept independent (see this module's doc comment).
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

/// Normalize a vendor tool name into a canonical tool family. Duplicated from
/// `crate::claude_code` for the same reason as [`bucket`].
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ks() -> Keystore {
        Keystore::new()
            .map("session", "sess-1", "ses_TEST")
            .map("workspace", "/work/repo", "wsp_TEST")
            .map("tool", "toolu_1", "tol_TEST")
            .map("tool_family", "toolu_1", "shell")
    }

    fn identity_ks() -> Keystore {
        Keystore::new()
            .map("session", "sess-1", "ses_TEST")
            .map("workspace", "/Users/dev/ghq/github.com/acme/api", "wsp_API")
            .map(
                "repository",
                "/Users/dev/ghq/github.com/acme/api",
                "rep_API",
            )
            .map("session_workspace", "sess-1", "wsp_API")
            .map("session_repository", "sess-1", "rep_API")
    }

    #[test]
    fn an_unrecognized_kind_produces_no_drafts() {
        let record = json!({ "type": "mode", "sessionId": "sess-1" });
        assert!(transform(&record, &ks()).is_empty());
    }

    #[test]
    fn a_sidechain_non_tool_result_user_record_produces_no_drafts() {
        let record = json!({
            "type": "user",
            "uuid": "u1",
            "sessionId": "sess-1",
            "timestamp": "2026-07-20T00:00:00.000Z",
            "isSidechain": true,
            "message": { "role": "user", "content": "subagent prompt" },
        });
        assert!(transform(&record, &ks()).is_empty());
    }

    #[test]
    fn a_tool_result_is_never_counted_as_a_human_prompt_even_with_false_flags() {
        // The exact trap `docs/superpowers/specs/expected-output-2026-07-26.md` §2.1
        // measured: isSidechain/isMeta false or absent, but a tool_result content
        // block (equivalently toolUseResult != null) must still exclude it.
        let record = json!({
            "type": "user",
            "uuid": "u2",
            "sessionId": "sess-1",
            "timestamp": "2026-07-20T00:00:05.000Z",
            "isSidechain": false,
            "toolUseResult": { "stdout": "x", "exit_code": 0 },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_1" }] },
        });
        let drafts = transform(&record, &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.tool.finished.v1");
    }

    /// A record of `kind` carrying every field that kind's mapping requires, so
    /// "produced no drafts" can only mean "this module has no case for the kind".
    fn fully_populated(kind: &str) -> Value {
        let mut record = json!({
            "type": kind,
            "uuid": "u-full",
            "sessionId": "sess-1",
            "timestamp": "2026-07-20T00:00:00.000Z",
            "cwd": "/work/repo",
        });
        match kind {
            "user" => record["message"] = json!({ "role": "user", "content": "SYNTHETIC prompt" }),
            "assistant" => {
                record["message"] = json!({
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "SYNTHETIC" }],
                    "usage": { "output_tokens": 1 },
                })
            }
            "attachment" => {
                record["attachment"] =
                    json!({ "type": "hook_success", "hookEvent": "SessionStart" })
            }
            _ => {}
        }
        record
    }

    #[test]
    fn every_mapped_kind_produces_drafts_and_unlisted_kinds_produce_none() {
        // Named for what it checks. It does NOT verify that `transform` has no match
        // arm outside `MAPPED_KINDS` -- that direction is not observable from here --
        // so it must not claim to. What it does pin is the direction the importer's
        // gap accounting depends on: a kind listed in `MAPPED_KINDS` really is
        // mapped, so "listed kind, zero drafts" genuinely means "this instance", not
        // "this kind was never implemented".
        for kind in MAPPED_KINDS {
            let drafts = transform(&fully_populated(kind), &ks());
            assert!(
                !drafts.is_empty(),
                "MAPPED_KINDS lists {kind:?}, but a fully-populated record of that kind \
                 produces no drafts -- the importer would report every one of them as a \
                 legitimate no-event skip"
            );
        }

        // A sample of the 19 real `type` values `docs/source-inventory.md` records
        // that this module deliberately does not map.
        for kind in [
            "mode",
            "system",
            "last-prompt",
            "result",
            "file-history-snapshot",
        ] {
            assert!(
                !MAPPED_KINDS.contains(&kind),
                "{kind:?} is listed in MAPPED_KINDS; this test's fixture list is stale"
            );
            assert!(
                transform(&fully_populated(kind), &ks()).is_empty(),
                "{kind:?} is not in MAPPED_KINDS but transform produced drafts for it -- \
                 the importer would gap a kind that is actually handled"
            );
        }
    }

    #[test]
    fn a_tool_results_duration_is_measured_against_the_tool_use_records_own_timestamp() {
        let record = json!({
            "type": "user",
            "uuid": "u3",
            "sessionId": "sess-1",
            "timestamp": "2026-07-20T00:00:07.250Z",
            "toolUseResult": { "stdout": "x", "exit_code": 0 },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_1" }] },
        });
        let ctx = ks().map("tool_started_at", "toolu_1", "2026-07-20T00:00:00.000Z");
        let drafts = transform(&record, &ctx);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].data["duration_ms"], json!(7250));
    }

    #[test]
    fn an_unmeasurable_tool_duration_is_null_rather_than_zero() {
        // `0` is a real measurement of a tool that returned within a millisecond, and
        // the hook adapter emits real durations into this same field. Emitting `0`
        // for "no start time available" would make the two indistinguishable.
        let record = json!({
            "type": "user",
            "uuid": "u4",
            "sessionId": "sess-1",
            "timestamp": "2026-07-20T00:00:07.000Z",
            "toolUseResult": { "stdout": "x", "exit_code": 0 },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_1" }] },
        });
        // `ks()` deliberately has no `tool_started_at` for `toolu_1`: the tool_use
        // line is not in this snapshot.
        let drafts = transform(&record, &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].data["duration_ms"],
            Value::Null,
            "an unmeasured duration must serialize as null, never as 0"
        );
    }

    /// An `assistant` record, with whatever `usage` object is passed (`None` for a
    /// record that carries none at all).
    fn assistant_with_usage(usage: Option<Value>) -> Value {
        let mut message = json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "SYNTHETIC" }],
        });
        if let Some(usage) = usage {
            message["usage"] = usage;
        }
        json!({
            "type": "assistant",
            "uuid": "u-assistant",
            "sessionId": "sess-1",
            "timestamp": "2026-07-20T00:00:00.000Z",
            "message": message,
        })
    }

    #[test]
    fn a_prompt_carries_no_token_count_because_a_user_record_measures_none() {
        // A Claude Code `user` transcript record has no token field anywhere in it.
        // The `"0"` bucket this used to hardcode is a *measurement* -- the one a hook
        // payload reporting a genuine zero produces into this same field -- so no
        // bucket at all may appear here.
        let record = json!({
            "type": "user",
            "uuid": "u-prompt",
            "sessionId": "sess-1",
            "timestamp": "2026-07-20T00:00:00.000Z",
            "message": { "role": "user", "content": "SYNTHETIC prompt" },
        });
        let drafts = transform(&record, &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.prompt.submitted.v1");

        let data = drafts[0].data.as_object().expect("data is an object");
        assert!(
            !data.contains_key("prompt_tokens"),
            "the source record carries no count, so prompt_tokens must be absent, \
             not a fabricated bucket (got {:?})",
            data.get("prompt_tokens")
        );
        // Stronger than "not `\"0\"`": *any* value there would be invented, since the
        // record holds no count to derive one from, and deriving one from the prompt's
        // own size would leak it out of a metadata-only ledger.
        let keys: Vec<&str> = data.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["content_ref"],
            "a transcript prompt.submitted carries content_ref and nothing else"
        );
    }

    #[test]
    fn an_unmeasured_response_token_count_is_absent_rather_than_the_zero_bucket() {
        // No `message.usage` at all: nothing was measured. Reporting `"0"` here would
        // be indistinguishable from the record below, which measured zero.
        let drafts = transform(&assistant_with_usage(None), &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.response.completed.v1");
        let data = drafts[0].data.as_object().expect("data is an object");
        assert!(
            !data.contains_key("response_tokens"),
            "a response whose record carries no usage must omit response_tokens \
             (got {:?})",
            data.get("response_tokens")
        );
    }

    #[test]
    fn a_measured_zero_response_token_count_still_reports_the_zero_bucket() {
        // The other half of the pair above: `"0"` must keep meaning "measured zero",
        // so dropping the field for a real zero would be just as wrong as inventing it
        // for an absent one.
        let drafts = transform(
            &assistant_with_usage(Some(json!({ "output_tokens": 0 }))),
            &ks(),
        );
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].data["response_tokens"],
            json!("0"),
            "a measured zero is a measurement and must still be reported"
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
        assert_eq!(bucket(Some(1)), Some("1-9"));
        assert_eq!(bucket(Some(10_000)), Some("10000+"));
    }

    #[test]
    fn a_record_with_a_cwd_carries_both_its_workspace_and_its_repository() {
        let record = json!({
            "type": "user",
            "uuid": "u-1",
            "sessionId": "sess-1",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "cwd": "/Users/dev/ghq/github.com/acme/api",
            "message": { "content": "hello" }
        });
        let drafts = transform(&record, &identity_ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].workspace_ref.as_deref(), Some("wsp_API"));
        assert_eq!(drafts[0].repository_ref.as_deref(), Some("rep_API"));
    }

    #[test]
    fn a_record_with_no_cwd_takes_its_identity_from_its_session() {
        let record = json!({
            "type": "user",
            "uuid": "u-2",
            "sessionId": "sess-1",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "message": { "content": "hello" }
        });
        let drafts = transform(&record, &identity_ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].repository_ref.as_deref(),
            Some("rep_API"),
            "a mapped record carrying no cwd is unattributable without this"
        );
    }

    #[test]
    fn a_session_start_carries_a_workspace_as_well_as_a_repository() {
        // Populating repository but not workspace would give `session.started` a
        // shape no other event has -- a repository with no workspace inside it.
        let record = json!({
            "type": "attachment",
            "uuid": "u-start",
            "sessionId": "sess-1",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "cwd": "/Users/dev/ghq/github.com/acme/api",
            "attachment": { "type": "hook_success", "hookEvent": "SessionStart" }
        });
        let drafts = transform(&record, &identity_ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].event_type, "dev.cclog.session.started.v1");
        assert_eq!(drafts[0].repository_ref.as_deref(), Some("rep_API"));
        assert_eq!(drafts[0].workspace_ref.as_deref(), Some("wsp_API"));
    }

    #[test]
    fn a_record_whose_own_cwd_is_unresolvable_stays_unresolved_rather_than_borrowing_its_session() {
        let record = json!({
            "type": "user",
            "uuid": "u-3",
            "sessionId": "sess-1",
            "timestamp": "2026-08-01T00:00:00.000Z",
            "cwd": "/Users/dev/Documents/notes",
            "message": { "content": "hello" }
        });
        let drafts = transform(&record, &identity_ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].repository_ref, None,
            "design §10 rule 4: the record's own cwd is better evidence than its \
             session's, so an unresolvable cwd must not be merged into another repository"
        );
        assert_eq!(drafts[0].workspace_ref, None);
    }
}
