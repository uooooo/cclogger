//! Workstream rules: mapping a repository identity to the bucket a person actually
//! thinks in -- client work, personal projects, one specific side project.
//!
//! The rule that matters most: **a repository no rule covers is `None` --
//! unassigned -- never folded into some other bucket.** [`Rules::workstream_for`]
//! returns `Option<&str>`, not a default label, so a new client whose config was
//! never written shows up as unassigned in the report rather than silently
//! attributing its time to whichever workstream happens to be listed first. This
//! project has already removed four fabricated values (see the design doc); a
//! guessed workstream would be the fifth.
//!
//! Two related refusals, both enforced by [`Rules::from_toml`] rather than left to
//! produce a rule that quietly does nothing:
//!
//! - An unknown key in `[[rule]]` or `[[override]]` is a parse error. A typo'd key
//!   (`matchh` for `match`) would otherwise silently define a rule that never
//!   matches anything, which is worse than refusing to load the config at all.
//! - A `*` anywhere but the final character of a `match` pattern is a parse error.
//!   Only a trailing wildcard is supported -- no general globbing -- so a pattern
//!   that looks like it does more (`github.com/*/api`) is rejected rather than
//!   reinterpreted as something narrower than it appears to say.
//!
//! `[[override]]` beats every `[[rule]]`: it names one repository exactly (via its
//! `workspace` key) and is checked before the ordered rule list, so a single
//! repository can be pulled out of a broader prefix rule without reordering
//! anything. Within `[[rule]]`, the first pattern that matches wins, so rule order
//! is meaningful -- a later, more specific rule after a broader earlier one is
//! simply unreachable, by design (TOML array order is preserved by both the `toml`
//! crate and this module).

use serde::Deserialize;

/// The `workstreams.toml` shape (acceptance document §3), deserialized strictly:
/// any table key this module does not know about is a parse error, not a silent
/// no-op. Both arrays default to empty so a config with only one kind of table --
/// or no tables at all -- is not an error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    rule: Vec<RawRule>,
    #[serde(default, rename = "override")]
    overrides: Vec<RawOverride>,
}

/// One `[[rule]]` table. `match` is a reserved word in Rust, hence the field
/// rename; `workstream` is a table key here, but it is *this rule's* workstream,
/// not a repository identity.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    #[serde(rename = "match")]
    pattern: String,
    workstream: String,
}

/// One `[[override]]` table. `workspace` names the one repository this override
/// applies to, exactly -- no pattern matching, unlike `[[rule]]`'s `match`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOverride {
    workspace: String,
    workstream: String,
}

/// A `[[rule]]`'s `match` pattern, validated at parse time so every `Pattern` that
/// exists is safe to evaluate without re-checking its shape.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    /// No `*`: matches only a repository identical to this string.
    Exact(String),
    /// A single trailing `*`, stored with the `*` already stripped: matches any
    /// repository that starts with this prefix.
    Prefix(String),
}

impl Pattern {
    /// Accepts zero stars (an exact match) or exactly one, as the pattern's final
    /// character (a prefix match). Anything else -- a star in the middle, or more
    /// than one -- is refused: §"a `*` anywhere but the end of a pattern is an
    /// error rather than reinterpreted" in this module's header.
    fn parse(raw: &str) -> Result<Self, RuleError> {
        match raw.matches('*').count() {
            0 => Ok(Pattern::Exact(raw.to_string())),
            1 if raw.ends_with('*') => Ok(Pattern::Prefix(raw[..raw.len() - 1].to_string())),
            _ => Err(RuleError::InvalidPattern(raw.to_string())),
        }
    }

    fn matches(&self, repository: &str) -> bool {
        match self {
            Pattern::Exact(exact) => repository == exact,
            Pattern::Prefix(prefix) => repository.starts_with(prefix.as_str()),
        }
    }
}

/// One validated `[[rule]]`: a pattern to test a repository against, and the
/// workstream it maps to when the pattern matches.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    pattern: Pattern,
    workstream: String,
}

/// One validated `[[override]]`: an exact repository identity and the workstream
/// it always maps to, regardless of what `[[rule]]` would otherwise say.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Override {
    workspace: String,
    workstream: String,
}

/// Everything that can keep [`Rules::from_toml`] from loading a config.
#[derive(Debug)]
pub enum RuleError {
    /// The text was not well-formed TOML for this shape, including an unknown
    /// table key (`#[serde(deny_unknown_fields)]`) or a missing required one.
    Toml(toml::de::Error),
    /// A `match` pattern had a `*` somewhere other than as its sole, final
    /// character.
    InvalidPattern(String),
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuleError::Toml(e) => write!(f, "invalid workstreams.toml: {e}"),
            RuleError::InvalidPattern(pattern) => write!(
                f,
                "invalid match pattern {pattern:?}: '*' is only supported as the final \
                 character, and at most once"
            ),
        }
    }
}

impl std::error::Error for RuleError {}

/// Ordered `[[rule]]` patterns plus exact-match `[[override]]`s, parsed from
/// `workstreams.toml`. See this module's header for the precedence and refusal
/// rules this type enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    rules: Vec<Rule>,
    overrides: Vec<Override>,
}

impl Rules {
    /// Parse the `workstreams.toml` shape. Unknown keys are an error, not ignored --
    /// a typo'd rule that silently does nothing is worse than a refusal.
    pub fn from_toml(text: &str) -> Result<Self, RuleError> {
        let raw: RawConfig = toml::from_str(text).map_err(RuleError::Toml)?;
        let rules = raw
            .rule
            .into_iter()
            .map(|r| {
                Ok(Rule {
                    pattern: Pattern::parse(&r.pattern)?,
                    workstream: r.workstream,
                })
            })
            .collect::<Result<Vec<_>, RuleError>>()?;
        let overrides = raw
            .overrides
            .into_iter()
            .map(|o| Override {
                workspace: o.workspace,
                workstream: o.workstream,
            })
            .collect();
        Ok(Rules { rules, overrides })
    }

    /// The workstream for a repository identity, or `None` when no rule matches.
    /// `None` means unassigned, which the report shows -- never a guessed bucket.
    ///
    /// Checks `[[override]]`s first (exact match, so at most one can apply), then
    /// `[[rule]]`s in file order, returning the first pattern that matches.
    pub fn workstream_for(&self, repository: &str) -> Option<&str> {
        if let Some(o) = self.overrides.iter().find(|o| o.workspace == repository) {
            return Some(o.workstream.as_str());
        }
        self.rules
            .iter()
            .find(|r| r.pattern.matches(repository))
            .map(|r| r.workstream.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[rule]]
match = "github.com/acme/*"
workstream = "acme"

[[rule]]
match = "github.com/dev/*"
workstream = "personal"

[[override]]
workspace = "github.com/dev/tooling"
workstream = "personal/tooling"
"#;

    #[test]
    fn a_prefix_rule_matches_every_repository_under_it() {
        let r = Rules::from_toml(SAMPLE).unwrap();
        assert_eq!(r.workstream_for("github.com/acme/api"), Some("acme"));
        assert_eq!(r.workstream_for("github.com/acme/web"), Some("acme"));
    }

    #[test]
    fn an_override_beats_the_rule_that_would_otherwise_match() {
        let r = Rules::from_toml(SAMPLE).unwrap();
        assert_eq!(
            r.workstream_for("github.com/dev/tooling"),
            Some("personal/tooling")
        );
        assert_eq!(r.workstream_for("github.com/dev/other"), Some("personal"));
    }

    #[test]
    fn a_repository_no_rule_covers_is_unassigned_rather_than_guessed() {
        let r = Rules::from_toml(SAMPLE).unwrap();
        assert_eq!(
            r.workstream_for("github.com/other/thing"),
            None,
            "an unmatched repository must not be folded into someone else's total"
        );
    }

    #[test]
    fn a_star_matches_only_at_the_end_of_the_pattern() {
        let r = Rules::from_toml("[[rule]]\nmatch = \"github.com/*/api\"\nworkstream = \"x\"\n");
        assert!(
            r.is_err(),
            "a mid-pattern star is refused, not silently reinterpreted"
        );
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let r = Rules::from_toml("[[rule]]\nmatchh = \"a/*\"\nworkstream = \"x\"\n");
        assert!(
            r.is_err(),
            "a typo'd rule that silently does nothing is worse than a refusal"
        );
    }

    #[test]
    fn the_first_matching_rule_wins_so_order_is_meaningful() {
        let r = Rules::from_toml(
            "[[rule]]\nmatch = \"github.com/acme/*\"\nworkstream = \"first\"\n\
             [[rule]]\nmatch = \"github.com/acme/*\"\nworkstream = \"second\"\n",
        )
        .unwrap();
        assert_eq!(r.workstream_for("github.com/acme/api"), Some("first"));
    }

    #[test]
    fn an_empty_config_assigns_nothing_and_is_not_an_error() {
        let r = Rules::from_toml("").unwrap();
        assert_eq!(r.workstream_for("github.com/acme/api"), None);
    }
}
