//! Golden tests: the Claude Code adapter must reproduce each fixture's `expected`
//! observations exactly from its `source` records.
//!
//! The runtime-assigned fields (id, `observed_at`, `monotonic_ns`, source-record ref,
//! profile) are all taken from the expected rows and applied via a `RuntimeStamp` — they
//! are not adapter-computed. What the adapter genuinely produces (event type, subject,
//! data, dedupe key, privacy class, workspace pseudonym) is asserted in full via a
//! whole-observation comparison.

use cclogger_adapters::{Keystore, claude_code};
use cclogger_domain::{Observation, RuntimeStamp};
use serde_json::Value;
use std::fs;
use std::path::Path;

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../adapters/claude-code/fixtures")
        .join(name);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("parse fixture json")
}

/// The keystore state at import time (the vendor-id → opaque-ref map the spool
/// pre-scan builds).
fn keystore() -> Keystore {
    Keystore::new()
        .map("session", "cc-9d2f", "ses_9D2F")
        .map("turn", "cc-prompt-01", "trn_01")
        .map("turn", "cc-prompt-02", "trn_02")
        .map("turn", "cc-prompt-03", "trn_03")
        .map("tool", "cc-tool-4cj", "tol_4CJ")
        .map("tool", "cc-tool-7mx", "tol_7MX")
        .map("tool", "cc-tool-8ny", "tol_8NY")
        .map("agent", "cc-agent-1", "agt_5RD")
        .map("workspace", "/work/acme-web", "wsp_QN5")
        .map("repository", "/work/acme-web", "rep_A7K")
}

fn check(name: &str) {
    let fx = fixture(name);
    let records = fx["source"]["records"].as_array().expect("source.records");
    let expected = fx["expected"].as_array().expect("expected");

    let ks = keystore();
    let drafts: Vec<_> = records
        .iter()
        .filter_map(|r| claude_code::transform(r, &ks))
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
            boot_id: Some("boot_Qk1".to_string()),
            source_record_ref: want["cclogsourcerecordref"].as_str().map(str::to_string),
            // Taken from the fixture, like every other runtime-assigned field here —
            // not hardcoded. Routing, not this test, is what decides
            // profile at runtime; the fixtures happen to all be `personal`, but this
            // reads it back rather than assuming it, so a regression that stopped
            // `finalize` from honoring `stamp.profile` would still be caught if a
            // fixture's expected profile ever changed.
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
fn session_lifecycle() {
    check("session-lifecycle.fixture.json");
}

#[test]
fn prompt_and_response() {
    check("prompt-and-response.fixture.json");
}

#[test]
fn tool_command() {
    check("tool-command.fixture.json");
}

#[test]
fn subagent_and_failure() {
    check("subagent-and-failure.fixture.json");
}
