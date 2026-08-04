//! Golden tests: the historical Claude Code adapter must reproduce each
//! `adapters/claude-code/shapes/*.shape.json` fixture's `expected` observations
//! exactly from its `records`.
//!
//! These shape fixtures were originally written (issue #9) to pin down record
//! *structure* only -- `records`, no `expected`. This issue (#13) is what turns them
//! into golden tests, by adding `expected` and asserting the historical adapter
//! reproduces it, the same way `crates/cclogger-adapters/tests/claude_code_golden.rs`
//! does for the hook adapter's `adapters/claude-code/fixtures/*.fixture.json`. The
//! only structural difference from that harness is where the input records live:
//! shape fixtures keep `records` at the top level (`doc["records"]`), not nested
//! under `source.records` like the hook fixtures.

use cclogger_adapters::{Keystore, claude_code_history};
use cclogger_domain::{Observation, RuntimeStamp};
use serde_json::Value;
use std::fs;
use std::path::Path;

fn shape(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../adapters/claude-code/shapes")
        .join(name);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("parse shape json")
}

/// The keystore state at capture time: every vendor id this fixture set's records
/// reference, mapped to a fixed opaque ref (matching how a real importer would have
/// pre-populated it via `cclogger_adapters::pseudonymize`, except spelled out by hand
/// here for readability).
fn keystore() -> Keystore {
    Keystore::new()
        .map(
            "session",
            "bbbbbbbb-2222-4222-8222-222222222222",
            "ses_ABCD",
        )
        .map(
            "workspace",
            "/Users/dev/ghq/github.com/acme/api",
            "wsp_XYZ1",
        )
        .map("tool", "toolu_synthetic1", "tol_0001")
        .map("tool_family", "toolu_synthetic1", "shell")
        // What the importer's pre-scan registers from the `tool_use` record's own
        // line, so the (later, separate) tool-result record can be given a real
        // `duration_ms` without the adapter reading a clock or the file.
        .map(
            "tool_started_at",
            "toolu_synthetic1",
            "2026-07-20T02:00:12.000Z",
        )
        .map(
            "session",
            "aaaaaaaa-1111-4111-8111-111111111111",
            "ses_SIDE",
        )
        .map(
            "workspace",
            "/Users/dev/ghq/github.com/acme/api",
            "wsp_XYZ1",
        )
        .map(
            "session",
            "cccccccc-3333-4333-8333-333333333333",
            "ses_DUPE",
        )
}

fn check(name: &str) {
    let fx = shape(name);
    let records = fx["records"].as_array().expect("records");
    let expected = fx["expected"].as_array().expect("expected");

    let ks = keystore();
    let drafts: Vec<_> = records
        .iter()
        .flat_map(|r| claude_code_history::transform(r, &ks))
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
fn user_prompt_and_tool() {
    check("user-prompt.shape.json");
}

#[test]
fn sidechain_subagent() {
    check("sidechain-subagent.shape.json");
}

#[test]
fn resume_duplicate() {
    check("resume-duplicate.shape.json");
}

/// The two duplicate records in `resume-duplicate.shape.json` must produce the same
/// `cclogdedupekey` -- the adapter's job (a deterministic dedupe key from a stable
/// record `uuid`), even though the adapter itself never deduplicates anything (that
/// is `cclogger_archive::Ledger::ingest`'s job at ingest time).
///
/// The keys are derived by running `transform` + `finalize` over the fixture's
/// *records*, not read back out of its `expected` block. Comparing the two `expected`
/// entries to each other only asserts that the fixture is self-consistent: an adapter
/// regression that changed how the key is built could not fail it standalone.
#[test]
fn resume_duplicate_records_share_one_dedupe_key() {
    let fx = shape("resume-duplicate.shape.json");
    let records = fx["records"].as_array().expect("records");
    assert_eq!(
        records.len(),
        2,
        "fixture must exercise exactly one duplicate pair"
    );

    let ks = keystore();
    let keys: Vec<String> = records
        .iter()
        .enumerate()
        .map(|(i, record)| {
            let mut drafts = claude_code_history::transform(record, &ks);
            assert_eq!(drafts.len(), 1, "record {i} must produce one draft");
            // Every runtime-supplied field varies between the two calls, exactly as
            // it would across the two real import runs this models -- so anything
            // that leaked from `RuntimeStamp` into the key would show up as a
            // mismatch here rather than being masked by identical inputs.
            drafts
                .remove(0)
                .finalize(RuntimeStamp {
                    id: format!("019bc4f1-0000-7000-8000-00000000000{i}"),
                    device: "dev_7N2".to_string(),
                    observed_at: format!("2026-07-2{i}T00:00:00.000Z"),
                    monotonic_ns: None,
                    boot_id: None,
                    source_record_ref: Some(format!("line:{i}")),
                    profile: cclogger_domain::Profile::Personal,
                })
                .cclogdedupekey
        })
        .collect();

    assert_eq!(
        keys[0], keys[1],
        "resuming a session and re-emitting the same record must yield the same dedupe key"
    );
}
