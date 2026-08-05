//! Conformance: every synthetic canonical observation in the fixtures must round-trip
//! losslessly through the domain [`Observation`] type. This proves the Rust model and the
//! JSON Schema agree — deserialization accepts the fixture and re-serialization reproduces
//! it byte-for-value (JSON object comparison is order-independent).

use cclogger_domain::Observation;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn find_fixtures(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            find_fixtures(&path, out);
        } else if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".fixture.json"))
        {
            out.push(path);
        }
    }
}

#[test]
fn all_fixture_observations_round_trip() {
    let adapters = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
    let mut files = Vec::new();
    find_fixtures(&adapters, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no fixtures under {}",
        adapters.display()
    );

    let mut count = 0usize;
    for file in &files {
        let raw = fs::read_to_string(file).expect("read fixture");
        let fixture: Value = serde_json::from_str(&raw).expect("parse fixture json");
        let expected = fixture
            .get("expected")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for (i, obs_json) in expected.iter().enumerate() {
            let obs: Observation = serde_json::from_value(obs_json.clone())
                .unwrap_or_else(|e| panic!("{}[{i}]: deserialize failed: {e}", file.display()));
            let reserialized = serde_json::to_value(&obs).expect("serialize");
            assert_eq!(
                &reserialized,
                obs_json,
                "{}[{i}]: round-trip mismatch",
                file.display()
            );
            count += 1;
        }
    }

    eprintln!(
        "round-tripped {count} observations across {} fixtures",
        files.len()
    );
    assert_eq!(count, 37, "expected 37 fixture observations");
}
