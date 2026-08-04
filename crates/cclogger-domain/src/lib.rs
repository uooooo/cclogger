//! Canonical observation domain types for cclog.
//!
//! Rust encoding of the canonical observation defined in
//! `schema/cclog.observation.v0.schema.json`. The wire form is CloudEvents 1.0
//! compatible: CloudEvents core attributes plus `cclog*` extension attributes.
//!
//! Content is never inlined. `data` is kept as an opaque [`serde_json::Value`] at this
//! layer so the type round-trips any valid observation losslessly; adapters build `data`
//! from typed payloads (see `cclogger-adapters`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which kind of source produced the raw record. Wire values are kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    ClaudeCode,
    Codex,
    Git,
    Otlp,
    OsPresence,
    Media,
}

impl SourceKind {
    /// The kebab-case wire encoding, used in opaque source URIs and dedupe keys.
    pub fn slug(&self) -> &'static str {
        match self {
            SourceKind::ClaudeCode => "claude-code",
            SourceKind::Codex => "codex",
            SourceKind::Git => "git",
            SourceKind::Otlp => "otlp",
            SourceKind::OsPresence => "os-presence",
            SourceKind::Media => "media",
        }
    }
}

/// Content-granularity tier. Orthogonal to identifiability/sensitivity (see data-model §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    T0Aggregate,
    T1Structured,
    T2Content,
    T3Media,
}

/// Integrity state of a single observation relative to its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityState {
    Ok,
    PossibleDuplicate,
    Gap,
    ClockSkew,
    Quarantined,
}

/// Local routing zone. Anything unclassified defaults to [`Profile::Personal`].
/// Local-only: never exported to a destination as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Personal,
    Client,
}

/// A single canonical observation as stored in the local ledger.
///
/// Field names match the wire form exactly (CloudEvents core + `cclog*` extensions).
/// Timestamps and ids are strings at this layer: the domain does not mint them — the
/// runtime stamps `id`, `cclogobservedat`, `cclogmonotonicns`, and `cclogprofile` at
/// [`ObservationDraft::finalize`] time (see [`RuntimeStamp`]). Opaque refs such as
/// `subject` are resolved by the adapter itself, from a read-only pseudonym lookup —
/// not by the runtime: the same source record and the same lookup always yield the
/// same draft, so resolving them there does not break adapter purity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// CloudEvents spec version, always `"1.0"`.
    pub specversion: String,
    /// `observation_id`, UUIDv7. Approximates emit order, not wall-time truth.
    pub id: String,
    /// Opaque `cclog://device/<opaque>/adapter/<kind>`. Never a username / path / machine id.
    pub source: String,
    /// `dev.cclog.<domain>.<event>.v<n>`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Opaque hierarchical ref path (e.g. `session/ses_2XQ/turn/trn_91M`).
    pub subject: String,
    /// `occurred_at`: source wall clock.
    pub time: String,
    /// Always `"application/json"`.
    pub datacontenttype: String,
    /// W3C trace context, local correlation evidence only. Absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,

    pub cclogschemaversion: u32,
    pub cclogsourcekind: SourceKind,
    pub cclogsourceversion: String,
    pub cclogadapterversion: String,
    /// Opaque local locator of the raw record (e.g. a `transcript_path`), never the bytes/path.
    #[serde(default)]
    pub cclogsourcerecordref: Option<String>,
    /// `observed_at`: collector receipt clock, separate from `time`.
    pub cclogobservedat: String,
    #[serde(default)]
    pub cclogmonotonicns: Option<u64>,
    #[serde(default)]
    pub cclogbootid: Option<String>,
    pub cclogprivacyclass: PrivacyClass,
    /// Nullable inference. Never a capture fact or authorization input.
    #[serde(default)]
    pub cclogpurposehint: Option<String>,
    pub cclogdedupekey: String,
    pub cclogintegritystate: IntegrityState,
    pub cclogprofile: Profile,
    #[serde(default)]
    pub cclogworkspaceref: Option<String>,
    #[serde(default)]
    pub cclogrepositoryref: Option<String>,
    /// Optional ingest-time cluster hint from explicit or same-batch evidence.
    /// Current semantic correlation, including late arrivals, is a versioned edge/projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cclogcorrelationcluster: Option<String>,

    /// Event payload, discriminated by `event_type`. Kept opaque at the domain layer.
    pub data: Value,
}

impl Observation {
    /// True at the metadata-only tiers, where a content pointer must never be present.
    pub fn is_metadata_only(&self) -> bool {
        matches!(
            self.cclogprivacyclass,
            PrivacyClass::T0Aggregate | PrivacyClass::T1Structured
        )
    }
}

mod draft;
pub use draft::{ObservationDraft, RuntimeStamp};
pub mod block;
pub mod clock;
pub mod workspace;
pub mod workstream;
