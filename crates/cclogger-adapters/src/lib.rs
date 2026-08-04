//! Source adapters: pure transforms from vendor / OS records into observation drafts.
//!
//! An adapter is a pure function of `(source_record, &Keystore)`. It never mints an id,
//! reads a clock, or knows the device — those are applied later by
//! [`cclogger_domain::ObservationDraft::finalize`]. The same adapter therefore serves both
//! historical import and live capture.

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Vendor-id → opaque-ref lookups available at transform time.
#[derive(Debug, Default, Clone)]
pub struct Keystore {
    pseudonyms: HashMap<(String, String), String>,
}

impl Keystore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a vendor-id → opaque-ref mapping.
    pub fn map(mut self, kind: &str, vendor: &str, opaque: &str) -> Self {
        self.pseudonyms
            .insert((kind.to_string(), vendor.to_string()), opaque.to_string());
        self
    }

    /// Resolve a vendor id to its opaque ref, or `None` if not registered.
    pub fn resolve(&self, kind: &str, vendor: &str) -> Option<String> {
        self.pseudonyms
            .get(&(kind.to_string(), vendor.to_string()))
            .cloned()
    }
}

/// Deterministically derive an opaque ref (`<prefix>_<hex>`) from a vendor id.
///
/// This is how a historical importer populates a [`Keystore`] at scale, without the
/// adapter itself ever minting anything: the same `(prefix, vendor_id)` pair always
/// produces the same opaque ref, on this run and on every re-run, which is what keeps
/// a draft's dedupe key stable across re-imports (see `cclogger-cli`'s import path).
/// This is a pseudonym, not encryption -- it is one-way and not intended to resist a
/// determined attacker with access to the local device, matching this slice's stated
/// scope (`docs/superpowers/specs/2026-07-29-history-import-slice-design.md` §7: an
/// encrypted, reversible lookup is later work).
///
/// The truncation width is 10 bytes (80 bits). It has to be chosen once and lived
/// with: these refs go into `cclogdedupekey`, which is write-once, so widening it
/// later would not re-key the rows already written. At 48 bits the birthday bound
/// starts to matter at ~16M refs within one collision scope, and a collision between
/// two tool ids in one session would wrongly collapse two distinct `tool.finished`
/// events into one -- a silent undercount, which is the failure mode this project
/// treats as its worst. 80 bits moves that bound to ~10^12 and costs 8 characters.
pub fn pseudonymize(prefix: &str, vendor_id: &str) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(vendor_id.as_bytes());
    // Appended into one String rather than collected from a per-byte `format!`, which
    // allocates ten times per call and a historical import makes a great many calls.
    // It is also what `clippy::format_collect` asks for -- default-on at the 1.85
    // floor CI pins, only pedantic on a newer stable, so the collect form passed
    // locally and failed there.
    let mut hex = String::with_capacity(20);
    for byte in digest.iter().take(10) {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("{prefix}_{hex}")
}

pub mod claude_code;
pub mod claude_code_history;
pub mod codex_history;
pub mod git_log;
pub mod rfc3339;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudonymize_is_deterministic_for_the_same_prefix_and_vendor_id() {
        assert_eq!(
            pseudonymize("ses", "vendor-session-1"),
            pseudonymize("ses", "vendor-session-1")
        );
    }

    #[test]
    fn pseudonymize_differs_across_vendor_ids() {
        assert_ne!(
            pseudonymize("ses", "vendor-session-1"),
            pseudonymize("ses", "vendor-session-2")
        );
    }

    #[test]
    fn pseudonymize_emits_the_full_chosen_width() {
        // The width is permanent once refs are in a ledger's dedupe keys (see the
        // function's doc comment), so pin it rather than leaving a `take(n)` that can
        // drift silently.
        let opaque = pseudonymize("ses", "vendor-session-1");
        assert_eq!(
            opaque.len(),
            "ses_".len() + 20,
            "10 bytes of digest, hex-encoded: {opaque}"
        );
    }

    #[test]
    fn pseudonymize_never_contains_the_vendor_id_verbatim() {
        let vendor_id = "/Users/dev/ghq/github.com/acme/api";
        let opaque = pseudonymize("wsp", vendor_id);
        assert!(!opaque.contains("Users"));
        assert!(!opaque.contains("acme"));
    }
}
