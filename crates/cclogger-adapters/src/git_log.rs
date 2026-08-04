//! Git adapter: one collected commit record → one `commit.observed` draft.
//!
//! Every other adapter in this crate reads a file a *vendor* wrote. This one reads a
//! file cclog wrote: `cclogger-cli`'s git collector runs `git log` against a repository the
//! ledger already knows about and normalizes each commit into one JSONL record
//! (`adapters/git/fixtures/commit.fixture.json` is that shape). The split is the same
//! one every other source has -- collection decides *what to ask for*, this module is a
//! pure transform of what came back -- and it is what lets the same golden fixture serve
//! as both documentation and test.
//!
//! # What a commit observation may carry, and what it may not
//!
//! A commit is the first source cclog ingests that arrives carrying obvious PII: an
//! author name, an author email, and a message. None of the three reaches an
//! observation, and two of them never reach the collector's own snapshot either,
//! because the collector never asks `git log` for them (see `cclogger-cli`'s `git` module).
//! This module is the second line of that defence: it reads exactly six fields and
//! would ignore a message if one appeared beside them.
//!
//! - **The message is content**, in exactly the way a prompt is. `message_ref` is the
//!   schema's slot for a pointer into the encrypted vault, and in metadata-only mode --
//!   the default, and all this build has -- it is `null`.
//! - **The author is a person.** Only the person's own commits are collected at all
//!   (the collector filters by their configured git identities), so "whose" is settled
//!   before this module sees anything, and there is nothing for the row to say about
//!   it. An author field on the row would be a name or an email with no purpose.
//! - **The sha is an identifier**, and the one genuinely stable id this project
//!   ingests -- but it is also a lookup key into a public repository's contents, so it
//!   reaches the row only as the pseudonym [`crate::pseudonymize`] derives from it. The
//!   raw sha stays in the collector's snapshot, in the owner-only archive, which is
//!   where every source's raw record already lives.
//!
//! # Identity: (repository, sha), never sha alone
//!
//! `dedupe_seed` is `[repository_ref, "commit.observed", commit_ref]`. A sha is stable
//! across re-imports, rebases of *other* commits, and clones -- which is exactly why it
//! is the id -- but it is not unique to one repository: a fork, a submodule, or a
//! vendored copy can hold the same commit, and a cherry-pick keeps neither the sha nor
//! the parent. The same commit seen in two repositories is therefore two observations,
//! one per repository, which is the truth: it is evidence in both.
//!
//! # No workspace
//!
//! `workspace_ref` is `None`, always. A repository's refs are shared by every worktree
//! of it, so `git log` cannot say which worktree a commit was made from -- and the
//! commit is not *in* a worktree in any case, it is in the repository. Every other
//! source populates the field from a `cwd`; there is no equivalent here, and a guess
//! (the main checkout, say) would claim work happened somewhere the evidence does not
//! place it.

use crate::Keystore;
use cclogger_domain::{IntegrityState, ObservationDraft, PrivacyClass, SourceKind};
use serde_json::{Value, json};

pub const SOURCE_VERSION: &str = "git-log/1";
pub const ADAPTER_VERSION: &str = "git@0.0.0";

/// Record kinds this module has a case for.
///
/// One, and it is not read off the record: cclog writes this file itself, so every line
/// in it is a commit record by construction and there is no vendor discriminant to
/// dispatch on. The constant exists because the importer's gap accounting is written
/// against it -- see [`kind`].
pub const MAPPED_KINDS: &[&str] = &["commit"];

/// The kind the importer matches against [`MAPPED_KINDS`] and labels a gap with.
///
/// Always `"commit"`, for any record: this file has exactly one record kind, so a line
/// that does not parse into one is a *malformed commit record* (a missing-field gap
/// naming what it lacked), never an "unmapped kind" -- which would say cclog had met a
/// record kind it has not implemented, when what actually happened is that a record it
/// wrote itself came back wrong.
pub fn kind(_record: &Value) -> String {
    "commit".to_string()
}

/// The first field a commit record must carry and does not, or `None` when it carries
/// all of them.
///
/// Checked in a fixed order so the same malformed record always reports the same field.
/// `files_changed` is in the list on purpose: the collector omits it when `git log`
/// printed a diffstat it could not parse, and the alternative to a gap there is
/// emitting `0`, which is a *measurement* -- the one an empty commit legitimately
/// produces. A parser that quietly drifted out of step with git's output would
/// otherwise report every commit in the ledger as having changed nothing.
pub fn missing_field(record: &Value) -> Option<&'static str> {
    if record.get("repository").and_then(Value::as_str).is_none() {
        return Some("repository");
    }
    if record.get("commit").and_then(Value::as_str).is_none() {
        return Some("commit");
    }
    if record.get("author_time").and_then(Value::as_str).is_none() {
        return Some("author_time");
    }
    if record
        .get("files_changed")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Some("files_changed");
    }
    None
}

/// Transform one collected commit record into 0..1 observation drafts.
///
/// Empty when the record is missing a field it needs, or when an identity it names is
/// not in `ctx` -- both of which the importer diagnoses rather than dropping (see
/// [`missing_field`]). A commit is never emitted with a degraded identity: an
/// observation whose repository could not be resolved would be evidence that something
/// landed somewhere, which is not a statement worth storing.
pub fn transform(record: &Value, ctx: &Keystore) -> Vec<ObservationDraft> {
    let Some(repository) = record.get("repository").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(commit) = record.get("commit").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(author_time) = record.get("author_time").and_then(Value::as_str) else {
        return Vec::new();
    };
    // Not defaulted to 0: see `missing_field`. `0` is what an empty commit measures.
    let Some(changed_paths_count) = record.get("files_changed").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let Some(repository_ref) = ctx.resolve("repository", repository) else {
        return Vec::new();
    };
    let Some(commit_ref) = ctx.resolve("commit", commit) else {
        return Vec::new();
    };

    // `message_ref` is present and null rather than absent: the schema types it as a
    // content pointer, and null is how a metadata-only row says "there is content, and
    // it is not here". `changed_paths_count` is exact because the schema types it as an
    // integer -- it is a count of paths, not of anything the diff said.
    let mut data = json!({
        "repository_ref": repository_ref,
        "changed_paths_count": changed_paths_count,
        "message_ref": null,
    });
    // Absent when the collector measured nothing, never `"0"` -- `"0"` is the bucket a
    // commit that really did add no lines falls in, and git omits the clause for it, so
    // the two have to stay distinguishable in a ledger that mixes them.
    if let Some(bucket) = bucket(record.get("insertions").and_then(Value::as_u64)) {
        data["insertions_bucket"] = json!(bucket);
    }
    if let Some(bucket) = bucket(record.get("deletions").and_then(Value::as_u64)) {
        data["deletions_bucket"] = json!(bucket);
    }

    vec![ObservationDraft {
        event_type: "dev.cclog.commit.observed.v1".to_string(),
        subject: format!("artifact/commit/{commit_ref}"),
        // Author time, not commit time. They are the same until a rebase, an amend or a
        // cherry-pick, after which the author time is still when the work was done and
        // the commit time is when the history was last rewritten. This ledger measures
        // the first. The cost is stated rather than hidden: a commit rebased into the
        // window months later carries its original instant, so it lands on the day it
        // was written and not on the day it was replayed.
        time: author_time.to_string(),
        traceparent: None,
        source_kind: SourceKind::Git,
        source_version: SOURCE_VERSION.to_string(),
        adapter_version: ADAPTER_VERSION.to_string(),
        privacy_class: PrivacyClass::T1Structured,
        integrity_state: IntegrityState::Ok,
        // See the module header: a repository's refs are shared by every worktree of
        // it, so which one a commit was made from is not in `git log`.
        workspace_ref: None,
        repository_ref: Some(repository_ref.clone()),
        correlation_cluster: None,
        dedupe_seed: vec![repository_ref, "commit.observed".to_string(), commit_ref],
        data,
    }]
}

/// Coarse magnitude bucket for a count that was measured; `None` when it was not.
///
/// Duplicated from the other adapters in this crate rather than shared, for the reason
/// their own copies state: they are tiny, and the adapters are deliberately independent.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ks() -> Keystore {
        Keystore::new()
            .map("repository", "github.com/acme/api", "rep_API")
            .map("repository", "github.com/acme/fork", "rep_FORK")
            .map("commit", "abc123", "cmt_ABC")
    }

    fn record() -> Value {
        json!({
            "repository": "github.com/acme/api",
            "commit": "abc123",
            "author_time": "2026-07-20T03:20:05.000Z",
            "files_changed": 2,
            "insertions": 30,
            "deletions": 4,
        })
    }

    #[test]
    fn a_commit_observation_carries_only_the_six_metadata_fields_the_schema_defines() {
        // The privacy gate, from the inside. A commit record is the first source that
        // arrives carrying a name, an email and a message; this pins that `data` holds
        // an exact, closed set of keys, so a later change that added `author`,
        // `message`, `subject` or a path list to the payload fails here rather than in
        // a leak scan that has to guess what prose looks like.
        let drafts = transform(&record(), &ks());
        assert_eq!(drafts.len(), 1);
        let data = drafts[0].data.as_object().expect("data is an object");
        let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "changed_paths_count",
                "deletions_bucket",
                "insertions_bucket",
                "message_ref",
                "repository_ref",
            ],
            "a commit observation's data is a closed set; got {data:?}"
        );
        assert_eq!(data["message_ref"], Value::Null);
    }

    #[test]
    fn a_message_beside_the_record_cannot_reach_the_observation() {
        // The collector never asks git for a message, so this record shape does not
        // occur -- which is exactly why the assertion is worth making: it is what stops
        // a later change on the collecting side from becoming a content leak silently.
        let mut record = record();
        record["message"] = json!("SYNTHETIC-CANARY fix the thing");
        record["author_email"] = json!("someone@example.test");
        record["author_name"] = json!("A Person");

        let drafts = transform(&record, &ks());
        assert_eq!(drafts.len(), 1);
        let wire = serde_json::to_string(&drafts[0].data).expect("serialize data");
        assert!(
            !wire.contains("SYNTHETIC-CANARY"),
            "a commit message reached the observation: {wire}"
        );
        assert!(
            !wire.contains("example.test") && !wire.contains("A Person"),
            "an author reached the observation: {wire}"
        );
        assert_eq!(drafts[0].subject, "artifact/commit/cmt_ABC");
        assert!(
            !drafts[0].subject.contains("abc123"),
            "the raw sha reached the subject: {}",
            drafts[0].subject
        );
    }

    #[test]
    fn the_same_sha_in_two_repositories_gets_two_distinct_identities() {
        // Forks, submodules and vendored copies really do hold the same sha, so the
        // dedupe key has to be scoped by repository. Seeded on the sha alone, the second
        // repository's evidence would collapse onto the first's and vanish.
        let mine = transform(&record(), &ks());
        let mut elsewhere = record();
        elsewhere["repository"] = json!("github.com/acme/fork");
        let forked = transform(&elsewhere, &ks());

        assert_eq!(mine.len(), 1);
        assert_eq!(forked.len(), 1);
        assert_ne!(
            mine[0].dedupe_seed, forked[0].dedupe_seed,
            "one sha in two repositories is two pieces of evidence, not one"
        );
        assert_eq!(
            mine[0].dedupe_seed,
            vec![
                "rep_API".to_string(),
                "commit.observed".to_string(),
                "cmt_ABC".to_string()
            ]
        );
    }

    #[test]
    fn a_commit_is_dated_by_its_author_time() {
        let drafts = transform(&record(), &ks());
        assert_eq!(drafts[0].time, "2026-07-20T03:20:05.000Z");
        // No `time_basis`: the record's own timestamp is when the work was done, and
        // absence is how every adapter says "nothing to qualify".
        assert!(
            !drafts[0]
                .data
                .as_object()
                .expect("data is an object")
                .contains_key("time_basis")
        );
    }

    #[test]
    fn a_commit_carries_no_workspace() {
        let drafts = transform(&record(), &ks());
        assert_eq!(
            drafts[0].workspace_ref, None,
            "`git log` cannot say which worktree a commit was made from"
        );
        assert_eq!(drafts[0].repository_ref.as_deref(), Some("rep_API"));
    }

    #[test]
    fn an_empty_commits_measured_zero_is_reported_rather_than_dropped() {
        let mut empty = record();
        empty["files_changed"] = json!(0);
        empty["insertions"] = json!(0);
        empty["deletions"] = json!(0);
        let drafts = transform(&empty, &ks());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].data["changed_paths_count"], json!(0));
        assert_eq!(
            drafts[0].data["insertions_bucket"],
            json!("0"),
            "a measured zero is a measurement"
        );
    }

    #[test]
    fn an_unmeasured_diffstat_produces_no_observation_rather_than_a_zero() {
        // The collector omits these three when git printed a diffstat it could not
        // parse. Emitting `0` would be indistinguishable from the empty commit above.
        let mut unmeasured = record();
        let object = unmeasured.as_object_mut().expect("record is an object");
        object.remove("files_changed");
        object.remove("insertions");
        object.remove("deletions");

        assert!(
            transform(&unmeasured, &ks()).is_empty(),
            "an unparseable diffstat must not become a commit that changed nothing"
        );
        assert_eq!(missing_field(&unmeasured), Some("files_changed"));
    }

    #[test]
    fn an_unresolvable_repository_produces_no_observation() {
        let mut elsewhere = record();
        elsewhere["repository"] = json!("github.com/acme/never-registered");
        assert!(
            transform(&elsewhere, &ks()).is_empty(),
            "a commit with no resolvable repository is not evidence of anything placeable"
        );
    }

    #[test]
    fn missing_field_names_the_first_absent_field_in_a_fixed_order() {
        assert_eq!(missing_field(&record()), None);
        assert_eq!(missing_field(&json!({})), Some("repository"));
        assert_eq!(
            missing_field(&json!({ "repository": "github.com/acme/api" })),
            Some("commit")
        );
        assert_eq!(
            missing_field(&json!({ "repository": "github.com/acme/api", "commit": "abc123" })),
            Some("author_time")
        );
    }

    #[test]
    fn every_record_is_the_one_mapped_kind() {
        // The importer's gap accounting reads "kind is in MAPPED_KINDS" as "this kind is
        // implemented", so a git line can only ever be a well-formed commit or a
        // malformed one -- never an unimplemented kind.
        assert_eq!(kind(&record()), "commit");
        assert_eq!(kind(&json!({ "nonsense": true })), "commit");
        assert!(MAPPED_KINDS.contains(&kind(&record()).as_str()));
    }

    #[test]
    fn bucket_maps_an_absent_count_to_none_and_a_measured_zero_to_the_zero_bucket() {
        assert_eq!(bucket(None), None);
        assert_eq!(bucket(Some(0)), Some("0"));
        assert_eq!(bucket(Some(137)), Some("100-999"));
        assert_eq!(bucket(Some(22)), Some("10-99"));
    }
}
