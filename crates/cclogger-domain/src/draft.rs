//! The adapter/runtime split.
//!
//! An adapter is a pure function of its source record: it can compute the event type,
//! subject, payload, and the source-derived part of the dedupe key, but it must not
//! mint an id, read a clock, know the device, or decide the profile. Those live in
//! [`RuntimeStamp`] and are applied by [`ObservationDraft::finalize`], so the same
//! adapter serves historical import and live capture.

use crate::{IntegrityState, Observation, PrivacyClass, Profile, SourceKind};
use serde_json::Value;

/// Everything about an observation that follows from the source record alone.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationDraft {
    pub event_type: String,
    pub subject: String,
    /// `occurred_at`: the source's own wall clock.
    pub time: String,
    pub traceparent: Option<String>,
    pub source_kind: SourceKind,
    pub source_version: String,
    pub adapter_version: String,
    pub privacy_class: PrivacyClass,
    pub integrity_state: IntegrityState,
    pub workspace_ref: Option<String>,
    pub repository_ref: Option<String>,
    pub correlation_cluster: Option<String>,
    /// Source-derived dedupe key parts, in order. `finalize` prepends the source-kind
    /// slug and the device id, which the adapter does not know.
    pub dedupe_seed: Vec<String>,
    pub data: Value,
}

/// The fields only the running collector can supply.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeStamp {
    pub id: String,
    pub device: String,
    /// `observed_at`: collector receipt clock, separate from the source's `time`.
    pub observed_at: String,
    pub monotonic_ns: Option<u64>,
    pub boot_id: Option<String>,
    pub source_record_ref: Option<String>,
    /// Local routing zone. Routing — not the adapter — decides
    /// this: it resolves `mapping_rules` against the observation's repo/path/branch/ticket
    /// and, failing a match, default-denies to [`Profile::Personal`]. An adapter sees only
    /// one vendor record and has no access to those routing rules, so it cannot make this
    /// call; it belongs on the stamp the runtime applies at finalize time.
    pub profile: Profile,
}

impl ObservationDraft {
    pub fn finalize(self, stamp: RuntimeStamp) -> Observation {
        let slug = self.source_kind.slug();
        let mut key = Vec::with_capacity(self.dedupe_seed.len() + 2);
        key.push(slug.to_string());
        key.push(stamp.device.clone());
        key.extend(self.dedupe_seed);

        Observation {
            specversion: "1.0".to_string(),
            id: stamp.id,
            source: format!("cclog://device/{}/adapter/{}", stamp.device, slug),
            event_type: self.event_type,
            subject: self.subject,
            time: self.time,
            datacontenttype: "application/json".to_string(),
            traceparent: self.traceparent,
            cclogschemaversion: 0,
            cclogsourcekind: self.source_kind,
            cclogsourceversion: self.source_version,
            cclogadapterversion: self.adapter_version,
            cclogsourcerecordref: stamp.source_record_ref,
            cclogobservedat: stamp.observed_at,
            cclogmonotonicns: stamp.monotonic_ns,
            cclogbootid: stamp.boot_id,
            cclogprivacyclass: self.privacy_class,
            cclogpurposehint: None,
            cclogdedupekey: key.join("|"),
            cclogintegritystate: self.integrity_state,
            cclogprofile: stamp.profile,
            cclogworkspaceref: self.workspace_ref,
            cclogrepositoryref: self.repository_ref,
            cclogcorrelationcluster: self.correlation_cluster,
            data: self.data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn draft() -> ObservationDraft {
        ObservationDraft {
            event_type: "dev.cclog.tool.started.v1".to_string(),
            subject: "session/ses_9D2F/turn/trn_01/tool/tol_4CJ".to_string(),
            time: "2026-07-20T03:02:10.000Z".to_string(),
            traceparent: None,
            source_kind: SourceKind::ClaudeCode,
            source_version: "claude-code-hook/1".to_string(),
            adapter_version: "claude-code@0.0.0".to_string(),
            privacy_class: PrivacyClass::T1Structured,
            integrity_state: IntegrityState::Ok,
            workspace_ref: Some("wsp_QN5".to_string()),
            repository_ref: None,
            correlation_cluster: None,
            dedupe_seed: vec![
                "ses_9D2F".to_string(),
                "tol_4CJ".to_string(),
                "tool.started".to_string(),
                "2026-07-20T03:02:10.000Z".to_string(),
            ],
            data: json!({ "tool_family": "shell" }),
        }
    }

    fn stamp() -> RuntimeStamp {
        RuntimeStamp {
            id: "obs_1".to_string(),
            device: "dev_7N2".to_string(),
            observed_at: "2026-07-20T03:02:10.100Z".to_string(),
            monotonic_ns: Some(42),
            boot_id: Some("boot_Qk1".to_string()),
            source_record_ref: None,
            profile: Profile::Personal,
        }
    }

    #[test]
    fn finalize_builds_the_dedupe_key_from_kind_device_and_seed() {
        let obs = draft().finalize(stamp());
        assert_eq!(
            obs.cclogdedupekey,
            "claude-code|dev_7N2|ses_9D2F|tol_4CJ|tool.started|2026-07-20T03:02:10.000Z"
        );
    }

    #[test]
    fn finalize_builds_the_opaque_source_uri_from_the_device() {
        let obs = draft().finalize(stamp());
        assert_eq!(obs.source, "cclog://device/dev_7N2/adapter/claude-code");
    }

    #[test]
    fn finalize_carries_runtime_fields_and_leaves_content_absent() {
        let obs = draft().finalize(stamp());
        assert_eq!(obs.id, "obs_1");
        assert_eq!(obs.cclogobservedat, "2026-07-20T03:02:10.100Z");
        assert_eq!(obs.cclogmonotonicns, Some(42));
        assert_eq!(obs.cclogbootid.as_deref(), Some("boot_Qk1"));
        assert_eq!(obs.cclogsourcerecordref, None);
        assert!(obs.is_metadata_only());
    }

    #[test]
    fn finalize_with_no_monotonic_clock_serializes_a_null_cclogmonotonicns() {
        // `RuntimeStamp::monotonic_ns` is an `Option` specifically so historical
        // import -- which has no monotonic clock to sample -- can supply `None`.
        // M1's first job is historical import, so this path must be exercised here.
        let mut stamp = stamp();
        stamp.monotonic_ns = None;

        let obs = draft().finalize(stamp);
        assert_eq!(obs.cclogmonotonicns, None);

        let wire = serde_json::to_value(&obs).unwrap();
        assert_eq!(wire["cclogmonotonicns"], serde_json::Value::Null);
    }

    #[test]
    fn finalize_honors_the_stamps_profile_rather_than_a_fixed_default() {
        // Every `Profile::` value elsewhere in this workspace's tests is `Personal`,
        // so a regression that hardcoded `Profile::Personal` inside `finalize`
        // instead of using `stamp.profile` would still pass every other test in the
        // suite, the golden fixtures, and the conformance harness. This is the exact
        // regression moving the profile decision into `RuntimeStamp` exists to
        // prevent, so it must be pinned with a non-`Personal` value here.
        let mut stamp = stamp();
        stamp.profile = Profile::Client;

        let obs = draft().finalize(stamp);
        assert_eq!(obs.cclogprofile, Profile::Client);
    }

    #[test]
    fn source_kind_slug_matches_the_wire_encoding() {
        assert_eq!(SourceKind::ClaudeCode.slug(), "claude-code");
        assert_eq!(SourceKind::Codex.slug(), "codex");
        assert_eq!(SourceKind::OsPresence.slug(), "os-presence");
    }
}
