//! Discovery of vendor transcript files under a home directory.
//!
//! Locators are relative to the home they were discovered under, not absolute
//! filesystem paths: a manifest locator should name a file relative to its home, the
//! same way a git path is relative to a repo root. This is not a privacy scrub. Claude
//! Code, in particular, encodes the absolute working directory into its own project
//! directory name (dashes standing in for slashes), so a `claude-code` locator still
//! contains the username and the full workspace path -- stripping the home prefix only
//! removes the home portion, not that vendor-supplied segment. What protects that data
//! is the archive's owner-only permissions (0700 root, 0600 files), not the locator's
//! shape.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub kind: &'static str,
    /// Home-relative path, used as the manifest locator.
    pub locator: String,
    pub path: PathBuf,
}

const ROOTS: &[(&str, &str)] = &[
    ("claude-code", ".claude/projects"),
    ("codex", ".codex/sessions"),
    ("codex", ".codex/archived_sessions"),
];

pub fn sources(home: &Path) -> Vec<SourceFile> {
    let mut out = Vec::new();
    for (kind, rel) in ROOTS {
        collect(home, &home.join(rel), kind, &mut out);
    }
    out.sort_by(|a, b| a.locator.cmp(&b.locator));
    out
}

fn collect(home: &Path, dir: &Path, kind: &'static str, out: &mut Vec<SourceFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // A vendor that was never installed simply has nothing to discover.
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(home, &path, kind, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let locator = path
                .strip_prefix(home)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push(SourceFile {
                kind,
                locator,
                path,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_home(name: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("cclog-home-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        // `-Users-x-repo` mirrors Claude Code's real project-directory naming: it
        // encodes the absolute working directory with dashes for slashes, so this
        // fixture's locator will still carry "x" (a stand-in username) after
        // `strip_prefix(home)` -- see the module doc comment above.
        fs::create_dir_all(home.join(".claude/projects/-Users-x-repo")).unwrap();
        fs::write(home.join(".claude/projects/-Users-x-repo/a.jsonl"), b"{}\n").unwrap();
        fs::create_dir_all(home.join(".codex/sessions")).unwrap();
        fs::write(home.join(".codex/sessions/b.jsonl"), b"{}\n").unwrap();
        fs::create_dir_all(home.join(".codex/archived_sessions")).unwrap();
        fs::write(home.join(".codex/archived_sessions/c.jsonl"), b"{}\n").unwrap();
        // Not a transcript: must not be picked up.
        fs::write(home.join(".claude/settings.json"), b"{}").unwrap();
        home
    }

    #[test]
    fn discovery_finds_transcripts_from_both_vendors() {
        let found = sources(&fake_home("both"));
        let kinds: Vec<_> = found.iter().map(|s| s.kind).collect();
        assert_eq!(found.len(), 3, "expected three transcripts, got {found:?}");
        assert!(kinds.contains(&"claude-code"));
        assert!(kinds.contains(&"codex"));
    }

    #[test]
    fn locators_are_relative_to_home_not_absolute_paths() {
        // This guards `strip_prefix`'s behaviour, not privacy: a locator can still
        // contain the username by way of vendor-encoded directory names (Claude Code
        // bakes the absolute cwd into its project directory, dashes for slashes -- see
        // the fixture below), and that is fine, because the manifest that stores these
        // locators is owner-only. What must hold is only that the *home* portion of the
        // path is gone and what remains is not itself absolute.
        let home = fake_home("locator");
        for s in sources(&home) {
            assert!(
                !s.locator.contains(home.to_str().unwrap()),
                "locator still contains the home path: {}",
                s.locator
            );
            assert!(
                !s.locator.starts_with('/'),
                "locator must be relative: {}",
                s.locator
            );
        }
    }

    #[test]
    fn a_missing_vendor_directory_is_not_an_error() {
        let home = std::env::temp_dir().join(format!("cclog-home-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(sources(&home).len(), 0);
    }
}
