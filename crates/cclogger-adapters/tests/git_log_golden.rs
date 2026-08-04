//! Golden test: the git adapter must reproduce `adapters/git/fixtures/commit.fixture.json`'s
//! `expected` observation exactly from its `source` record.
//!
//! Shaped like `claude_code_golden.rs`: the runtime-assigned fields (id, `observed_at`,
//! `monotonic_ns`, boot id, source-record ref, profile) are read back off the expected
//! row and applied through a `RuntimeStamp`, so what is actually asserted is what the
//! adapter itself produces -- event type, subject, time, data, dedupe seed, privacy
//! class, and the two identity refs.

use cclogger_adapters::{Keystore, git_log};
use cclogger_domain::{Observation, RuntimeStamp};
use serde_json::Value;
use std::fs;
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../adapters/git/fixtures/commit.fixture.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("parse fixture json")
}

/// The keystore state at import time: what the git pre-scan builds from the same
/// records. The fixture's short, readable refs stand in for the `pseudonymize` output
/// a real run derives, exactly as the Claude Code goldens' refs do.
fn keystore() -> Keystore {
    Keystore::new()
        .map("repository", "github.com/acme/api", "rep_ACME")
        .map("commit", "9f3ac1e", "cmt_9F3A")
}

#[test]
fn commit_observed() {
    let fx = fixture();
    let records = fx["source"]["records"].as_array().expect("source.records");
    let expected = fx["expected"].as_array().expect("expected");

    let ks = keystore();
    let drafts: Vec<_> = records
        .iter()
        .flat_map(|r| git_log::transform(r, &ks))
        .collect();

    assert_eq!(
        drafts.len(),
        expected.len(),
        "produced {} observations, expected {}",
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
        assert_eq!(&got_val, want, "commit fixture[{i}]: observation mismatch");
    }
}
