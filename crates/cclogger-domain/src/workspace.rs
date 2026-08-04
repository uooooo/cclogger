//! Workspace identity: turning a historical `cwd` into stable repository and
//! workspace identities.
//!
//! Two identities, deliberately distinct (design doc §10). A **repository** is the
//! project — `github.com/acme/api`. A **workspace** is where work happened inside it:
//! the main checkout, or a git worktree, which gets its own workspace under the same
//! repository. On the corpus this was written against, 62 of 74 workspaces were
//! worktrees of a single repository, so collapsing the two would have split one
//! project into dozens of report rows.
//!
//! Resolution is purely syntactic — it never touches the filesystem. Historical cwds
//! routinely name directories that have since been deleted, and a resolver that
//! needed them to exist would silently lose exactly the oldest history this project
//! exists to keep.
//!
//! A cwd that does not sit under `<home>/ghq/<host>/<owner>/<repo>` resolves to
//! nothing. That is design §10 rule 4: leave it unresolved rather than guess-merging
//! it into some other repository. Absence is `None`, never a plausible-looking guess.
//!
//! # Known limitation: a worktree branch name containing `/`
//!
//! A worktree is labelled with the **first** path segment below `.worktrees`, so
//! `.worktrees/feature/x` labels the workspace `…@feature`, and `feature/x` and
//! `feature/y` collapse into one workspace named after neither. Git allows `/` in a
//! branch name, so this is a real shape.
//!
//! It is a deliberate choice, not an oversight, because the two readings are
//! syntactically indistinguishable: `.worktrees/<branch>/<subdir>` (a subdirectory
//! inside a worktree, which must fold into it — the common case, and the one
//! `a_subdirectory_inside_a_worktree_folds_into_that_worktree` pins) and
//! `.worktrees/<branch-with-a-slash>` produce identical paths. Distinguishing them
//! needs the filesystem or git, and this resolver deliberately has neither. Guessing
//! the longer reading would break every subdirectory of every ordinary worktree —
//! far more common than a slashed branch name — so the shorter one wins, and the
//! collapse is bounded: **repository identity is unaffected**, both worktrees stay
//! under the correct repository, and the result is deterministic. What is lost is
//! only the ability to tell two slash-sharing worktrees apart in a per-workspace
//! breakdown.
//!
//! `a_worktree_branch_containing_a_slash_is_labelled_by_its_first_segment` pins this
//! so it stays a known choice rather than drifting into an accident.

/// A cwd resolved to its repository and workspace, or `None` for either when the
/// path does not carry that identity.
///
/// Both fields are `None` together in practice: a path either sits in the ghq tree
/// deep enough to name a repository, or it names neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    /// The project — `host/owner/repo`, e.g. `github.com/acme/api`.
    pub repository: Option<String>,
    /// Where work happened inside it: the repository itself, or `repository@branch`
    /// for a git worktree under `<repo>/.worktrees/<branch>`, where `branch` is the
    /// first path segment below `.worktrees` (see the module docs' known limitation
    /// for a branch name containing `/`).
    pub workspace: Option<String>,
}

/// Resolve a `cwd` recorded on a machine whose home directory was `home`.
///
/// `home` is passed in rather than read from the environment so this stays a pure
/// function — the importer supplies it. A cwd archived on a *different* machine will
/// not resolve here, which is correct: this build cannot know that machine's layout.
pub fn resolve(cwd: &str, home: &str) -> WorkspaceIdentity {
    let Some(rest) = ghq_relative(cwd, home) else {
        return WorkspaceIdentity::default();
    };
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 {
        return WorkspaceIdentity::default();
    }
    let repository = segments[..3].join("/");
    let workspace = match &segments[3..] {
        [".worktrees", branch, ..] => format!("{repository}@{branch}"),
        _ => repository.clone(),
    };
    WorkspaceIdentity {
        repository: Some(repository),
        workspace: Some(workspace),
    }
}

/// The part of `cwd` below `<home>/ghq/`, or `None` if it is not under it.
///
/// The `/` is stripped as its own step so a home directory that is merely a string
/// prefix of the cwd (`/Users/dev` against `/Users/developer/...`) does not match.
fn ghq_relative<'a>(cwd: &'a str, home: &str) -> Option<&'a str> {
    cwd.strip_prefix(home.trim_end_matches('/'))?
        .strip_prefix('/')?
        .strip_prefix("ghq/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/dev";

    #[test]
    fn a_repository_checkout_resolves_to_host_owner_repo() {
        let id = resolve("/Users/dev/ghq/github.com/acme/api", HOME);
        assert_eq!(id.repository.as_deref(), Some("github.com/acme/api"));
        assert_eq!(id.workspace.as_deref(), Some("github.com/acme/api"));
    }

    #[test]
    fn a_subdirectory_folds_into_its_repository() {
        let id = resolve("/Users/dev/ghq/github.com/acme/api/src/handlers", HOME);
        assert_eq!(id.repository.as_deref(), Some("github.com/acme/api"));
        assert_eq!(
            id.workspace.as_deref(),
            Some("github.com/acme/api"),
            "a subdirectory is the same workspace, not a new one"
        );
    }

    #[test]
    fn a_worktree_is_its_own_workspace_under_the_same_repository() {
        let id = resolve(
            "/Users/dev/ghq/github.com/acme/api/.worktrees/issue-62",
            HOME,
        );
        assert_eq!(id.repository.as_deref(), Some("github.com/acme/api"));
        assert_eq!(
            id.workspace.as_deref(),
            Some("github.com/acme/api@issue-62")
        );
    }

    #[test]
    fn a_subdirectory_inside_a_worktree_folds_into_that_worktree() {
        let id = resolve(
            "/Users/dev/ghq/github.com/acme/api/.worktrees/issue-62/tools/gateway",
            HOME,
        );
        assert_eq!(id.repository.as_deref(), Some("github.com/acme/api"));
        assert_eq!(
            id.workspace.as_deref(),
            Some("github.com/acme/api@issue-62")
        );
    }

    #[test]
    fn a_worktree_branch_containing_a_slash_is_labelled_by_its_first_segment() {
        // Pins the known limitation documented in this module's header. `feature/x`
        // and `feature/x/sub` are the same path shape, so the resolver cannot tell a
        // slashed branch name from a subdirectory inside a worktree without touching
        // the filesystem -- which it deliberately never does. The shorter reading
        // wins because a subdirectory of an ordinary worktree is far more common.
        let sliced = resolve(
            "/Users/dev/ghq/github.com/acme/api/.worktrees/feature/x",
            HOME,
        );
        assert_eq!(
            sliced.workspace.as_deref(),
            Some("github.com/acme/api@feature"),
            "the branch is labelled by its first segment below .worktrees"
        );

        // The bound on the damage, and the reason this is Minor rather than a bug:
        // two branches sharing a first segment collapse into one *workspace*, but
        // both stay under the correct repository, so no repository-level report --
        // the aggregation unit -- is affected.
        let sibling = resolve(
            "/Users/dev/ghq/github.com/acme/api/.worktrees/feature/y",
            HOME,
        );
        assert_eq!(
            sibling.workspace, sliced.workspace,
            "two slash-sharing branches are indistinguishable here"
        );
        assert_eq!(
            sliced.repository.as_deref(),
            Some("github.com/acme/api"),
            "but repository identity is unaffected"
        );
        assert_eq!(sibling.repository, sliced.repository);
    }

    #[test]
    fn a_worktrees_directory_with_no_branch_below_it_is_just_the_repository() {
        let id = resolve("/Users/dev/ghq/github.com/acme/api/.worktrees", HOME);
        assert_eq!(id.repository.as_deref(), Some("github.com/acme/api"));
        assert_eq!(id.workspace.as_deref(), Some("github.com/acme/api"));
    }

    #[test]
    fn a_path_outside_the_ghq_tree_is_left_unresolved() {
        let id = resolve("/Users/dev/Documents/notes", HOME);
        assert_eq!(id.repository, None);
        assert_eq!(
            id.workspace, None,
            "design §10 rule 4: leave it unresolved rather than guess-merging it"
        );
    }

    #[test]
    fn a_ghq_path_with_fewer_than_three_segments_is_left_unresolved() {
        // Real case: working in the parent directory that holds local-only repos.
        let id = resolve("/Users/dev/ghq/local/dev", HOME);
        assert_eq!(id.repository, None);
        assert_eq!(id.workspace, None);
    }

    #[test]
    fn the_ghq_root_itself_is_left_unresolved() {
        let id = resolve("/Users/dev/ghq", HOME);
        assert_eq!(id.repository, None);
        assert_eq!(id.workspace, None);
    }

    #[test]
    fn a_home_that_is_only_a_string_prefix_of_the_cwd_does_not_match() {
        let id = resolve("/Users/developer/ghq/github.com/acme/api", HOME);
        assert_eq!(
            id.repository, None,
            "/Users/developer must not be treated as living under /Users/dev"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_create_an_extra_segment() {
        let id = resolve("/Users/dev/ghq/github.com/acme/api/", HOME);
        assert_eq!(id.repository.as_deref(), Some("github.com/acme/api"));
        assert_eq!(id.workspace.as_deref(), Some("github.com/acme/api"));
    }

    #[test]
    fn a_non_github_host_resolves_the_same_way() {
        // ghq's tree is host/owner/repo whatever the host is, including the
        // fake `local` host used for repositories that have no remote.
        let id = resolve("/Users/dev/ghq/local/dev/scratch", HOME);
        assert_eq!(id.repository.as_deref(), Some("local/dev/scratch"));
        assert_eq!(id.workspace.as_deref(), Some("local/dev/scratch"));
    }

    #[test]
    fn resolution_does_not_depend_on_the_directory_existing() {
        // Historical cwds routinely name directories that have since been deleted.
        let id = resolve("/Users/dev/ghq/github.com/acme/deleted-repo", HOME);
        assert_eq!(
            id.repository.as_deref(),
            Some("github.com/acme/deleted-repo")
        );
    }
}
