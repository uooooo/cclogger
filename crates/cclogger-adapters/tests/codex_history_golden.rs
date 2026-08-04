//! Golden test: the Codex historical adapter must reproduce
//! `adapters/codex/fixtures/history-prompt.fixture.json`'s `expected` observations
//! exactly from its `records`.
//!
//! Mirrors `crates/cclogger-adapters/tests/claude_code_history_golden.rs`, which does the
//! same for the Claude Code historical adapter. The one structural difference is the
//! keystore: Codex's human-prompt records carry no session field at all (unlike Claude
//! Code's, which always carry `sessionId`), so every ref this fixture's records need is
//! registered under [`codex_history::FILE_SESSION`] as well as under the vendor session
//! id `session_meta` names -- exactly what a real whole-file pre-scan
//! (`cclogger-cli`'s `CodexPreScan`) would have produced, spelled out by hand here for
//! readability, the same way `claude_code_history_golden.rs`'s `keystore()` is.
//!
//! Before this test, no Codex historical observation was schema-validated by anything:
//! `adapters/codex/fixtures/*.fixture.json` only covered the live hook/OTLP path.

use cclogger_adapters::{Keystore, codex_history};
use cclogger_domain::{Observation, Profile, RuntimeStamp};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// The real Codex `session_meta.payload.session_id` this fixture's records share.
const SESSION_ID: &str = "9f2c1a00-72e1-4b8a-9c40-5e1d2f3a4b5c";

/// `subagent-prompt.fixture.json`'s two `session_id`s, standing in for two different
/// rollout files. Both are named explicitly on the record -- unlike a real Codex
/// `user_message`, which never carries `session_id` at all -- because that fixture's
/// flat records list is checked against one shared [`Keystore`], and
/// [`codex_history::FILE_SESSION`] can only stand for one file's fallback at a time;
/// see that fixture's own `description`.
const HUMAN_SESSION_ID: &str = "9f2c1a00-0000-4000-8000-000000000h01";
const SUBAGENT_SESSION_ID: &str = "9f2c1a00-0000-4000-8000-000000000s01";

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../adapters/codex/fixtures")
        .join(name);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("parse fixture json")
}

/// The keystore state at capture time. Every ref this fixture's records reference is
/// registered under **both** the vendor id and its fallback key, mirroring exactly how
/// `cclogger-cli`'s `CodexPreScan::finish` registers a Codex session's identities: the
/// `session_meta` record names its session explicitly, but the `user_message` and
/// `response_item` records in a real transcript never do, so they resolve only through
/// [`codex_history::FILE_SESSION`]. Registering just one key would silently leave half
/// of this fixture's records unattributed rather than exercising the fallback this
/// task exists to pin.
fn keystore() -> Keystore {
    Keystore::new()
        .map("session", SESSION_ID, "ses_CDX1")
        .map("session", codex_history::FILE_SESSION, "ses_CDX1")
        .map("workspace", SESSION_ID, "wsp_XYZ1")
        .map("workspace", codex_history::FILE_SESSION, "wsp_XYZ1")
        .map("repository", SESSION_ID, "rep_ACME")
        .map("repository", codex_history::FILE_SESSION, "rep_ACME")
        // The tool call carries both `id` and `call_id`; its output carries only
        // `call_id`. Both spellings are registered to the same opaque ref, the way
        // `CodexPreScan::observe_tool_call` registers a real one, so the call and its
        // output resolve to one shared scope.
        .map("tool", "ctc_synthetic0001", "tol_0001")
        .map("tool", "call_synthetic0001", "tol_0001")
        .map("tool_family", "ctc_synthetic0001", "shell")
        .map("tool_family", "call_synthetic0001", "shell")
        .map(
            "tool_started_at",
            "ctc_synthetic0001",
            "2026-07-20T04:02:10.000Z",
        )
        .map(
            "tool_started_at",
            "call_synthetic0001",
            "2026-07-20T04:02:10.000Z",
        )
        // `subagent-prompt.fixture.json`'s two sessions, standing in for two different
        // rollout files (see [`HUMAN_SESSION_ID`]'s doc comment). Only the subagent one
        // gets `codex_subagent_session` -- registered exactly the way
        // `CodexPreScan::finish` registers it in the real importer, under the same keys
        // `"session"` is, with a value that is never read. Presence is the whole fact.
        .map("session", HUMAN_SESSION_ID, "ses_HUM1")
        .map("workspace", HUMAN_SESSION_ID, "wsp_XYZ1")
        .map("repository", HUMAN_SESSION_ID, "rep_ACME")
        .map("session", SUBAGENT_SESSION_ID, "ses_SUB1")
        .map("workspace", SUBAGENT_SESSION_ID, "wsp_XYZ1")
        .map("repository", SUBAGENT_SESSION_ID, "rep_ACME")
        .map("codex_subagent_session", SUBAGENT_SESSION_ID, "subagent")
}

fn check(name: &str) {
    let fx = fixture(name);
    let records = fx["records"].as_array().expect("records");
    let expected = fx["expected"].as_array().expect("expected");

    let ks = keystore();
    let drafts: Vec<_> = records
        .iter()
        .flat_map(|r| codex_history::transform(r, &ks))
        .collect();

    assert_eq!(
        drafts.len(),
        expected.len(),
        "{name}: produced {} observations, expected {}",
        drafts.len(),
        expected.len()
    );

    for (i, (draft, want)) in drafts.into_iter().zip(expected).enumerate() {
        let stamp = RuntimeStamp {
            id: want["id"].as_str().unwrap().to_string(),
            device: "dev_7N2".to_string(),
            observed_at: want["cclogobservedat"].as_str().unwrap().to_string(),
            monotonic_ns: want["cclogmonotonicns"].as_u64(),
            boot_id: want["cclogbootid"].as_str().map(str::to_string),
            source_record_ref: want["cclogsourcerecordref"].as_str().map(str::to_string),
            profile: serde_json::from_value(want["cclogprofile"].clone())
                .expect("fixture cclogprofile must deserialize to a known Profile"),
        };
        let got: Observation = draft.finalize(stamp);
        let got_val = serde_json::to_value(&got).unwrap();
        assert_eq!(
            &got_val, want,
            "{name}[{i}]: canonical observation mismatch"
        );
    }
}

#[test]
fn history_prompt_reannounce_and_tool() {
    check("history-prompt.fixture.json");
}

/// Golden test for `adapters/codex/fixtures/subagent-prompt.fixture.json`: a subagent's
/// own prompt is stamped `origin: "subagent"`, a human's `origin: "human"`, from the
/// same [`codex_history::transform`] entry point every other prompt in this suite goes
/// through -- the difference is entirely in what the `Keystore` (here, `keystore()`)
/// has registered under `codex_subagent_session`, exactly as a real cross-file pre-scan
/// would.
#[test]
fn subagent_prompt_origin() {
    check("subagent-prompt.fixture.json");
}

/// `mcp_tool_call_end` is the only trace of an MCP tool call -- Task 4 of the Codex
/// importer plan: 6,220 real records, none of them a second view of anything
/// `function_call` already carries (zero of its 31 names are MCP names). Its `call_id`,
/// `call_synthetic_mcp01`, is deliberately **not** registered under `"tool"` in
/// [`keystore`], unlike the shell pair above: a real pre-scan does not yet observe
/// `mcp_tool_call_end` either, so this exercises the fallback identity every real MCP
/// import currently takes, and pins that it never reaches into `invocation.arguments`
/// or `result` -- both content -- to build it.
#[test]
fn mcp_tool_call() {
    check("mcp-tool-call.fixture.json");
}

/// The two `event_msg:user_message` records in `history-prompt.fixture.json` differ
/// only in `client_id` -- the real corpus's harder collision shape (measured: one of
/// four re-announcement groups differs in nothing else) -- and sit on non-adjacent
/// lines (the tool call pair falls between them). They must still collapse onto one
/// dedupe key.
///
/// As in `claude_code_history_golden.rs`'s analogous test, the keys are derived by
/// running `transform` + `finalize` over the fixture's *records* with independently
/// varying `RuntimeStamp`s, not read back out of `expected`: comparing the two
/// `expected` entries to each other would only prove the fixture is self-consistent,
/// not that the adapter itself collapses them.
#[test]
fn the_reannounced_prompt_shares_one_dedupe_key_with_the_first() {
    let fx = fixture("history-prompt.fixture.json");
    let records = fx["records"].as_array().expect("records");
    let prompts: Vec<&Value> = records
        .iter()
        .filter(|r| codex_history::kind(r) == "event_msg:user_message")
        .collect();
    assert_eq!(
        prompts.len(),
        2,
        "fixture must exercise exactly one re-announcement pair"
    );

    let ks = keystore();
    let keys: Vec<String> = prompts
        .into_iter()
        .enumerate()
        .map(|(i, record)| {
            let mut drafts = codex_history::transform(record, &ks);
            assert_eq!(drafts.len(), 1, "prompt {i} must produce one draft");
            // Every runtime-supplied field varies between the two calls, exactly as it
            // would across two real import runs -- so anything that leaked from
            // `RuntimeStamp` into the key would show up as a mismatch here rather than
            // being masked by identical inputs.
            drafts
                .remove(0)
                .finalize(RuntimeStamp {
                    id: format!("019bc50b-0000-7000-8000-00000000000{i}"),
                    device: "dev_7N2".to_string(),
                    observed_at: format!("2026-07-2{i}T00:00:00.000Z"),
                    monotonic_ns: None,
                    boot_id: None,
                    source_record_ref: Some(format!("line:{i}")),
                    profile: Profile::Personal,
                })
                .cclogdedupekey
        })
        .collect();

    assert_eq!(
        keys[0], keys[1],
        "re-announcing the same prompt with a different client_id must yield the same dedupe key"
    );
}
