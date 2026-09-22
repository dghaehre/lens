//! VCS-agnostic backend trait. `GitBackend` and `JjBackend` both implement
//! this; consumers in `command/diff` and `command/annotation` take
//! `&dyn VcsBackend` so the rest of the app doesn't care which VCS is live.

use std::path::Path;

use super::types::{RevisionSummary, StackedCommitInfo, VcsError};

/// Which VCS backend is in use. The staging-area code paths
/// (`--cached` / `--staged`) are git-only and need to branch on this.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VcsKind {
    Git,
    Jj,
}

/// The read-only VCS operations lens needs.
///
/// Some methods (the index/staged ones) only make sense on git. The jj
/// backend either maps them to a sensible equivalent (jj has no index, so
/// "unstaged" becomes "`@-` vs `@`") or returns `VcsError::Unsupported`
/// (the `staged_*` calls — jj has no staging area at all).
pub trait VcsBackend {
    /// Synchronize any VCS state that is maintained outside the process.
    ///
    /// Git reads the working tree directly, so the default is a no-op. jj
    /// snapshots its working copy into `@` before it can be diffed, and its
    /// backend overrides this hook to perform that snapshot before each load.
    fn refresh(&self) -> Result<(), VcsError> {
        Ok(())
    }

    /// Which VCS this backend wraps.
    fn kind(&self) -> VcsKind;

    /// Path to the VCS state directory — `.git/` for git, `.jj/` for jj.
    /// Used to site per-repo state like `lens.db`. Per-worktree on git
    /// (it is `Repository::path()`) and per-workspace on jj, so two
    /// worktrees/workspaces of one repo get independent state.
    fn state_dir(&self) -> &Path;

    /// Currently checked-out branch name (git) or bookmark(s) at `@` (jj).
    /// Returns `Ok(None)` in detached-HEAD or no-bookmark cases.
    fn get_current_branch(&self) -> Result<Option<String>, VcsError>;

    /// Recent revisions suitable for a "pick a diff base" picker, newest
    /// first, capped at `limit`. Git has no implementation yet — there `b`
    /// keeps its typed prompt.
    fn list_revisions(&self, limit: usize) -> Result<Vec<RevisionSummary>, VcsError> {
        let _ = limit;
        Err(VcsError::Unsupported("revision listing is jj-only".into()))
    }

    /// Resolve a user-supplied reference (commit-ish, branch, tag, revset)
    /// to a stable identifier — a 40-char hex SHA for git, a hex commit-id
    /// for jj.
    fn resolve_ref(&self, reference: &str) -> Result<String, VcsError>;

    /// Reference identifying the parent of `reference`, or git's empty-tree
    /// SHA when `reference` has no parent. For git this is typically the
    /// string `"<ref>^"`. For jj it's the resolved commit id of `(<ref>)-`.
    fn get_parent_ref_or_empty(&self, reference: &str) -> Result<String, VcsError>;

    /// Files changed by a single commit (vs its parent), or by a `from..to`
    /// range when `reference` contains `..`.
    fn get_changed_files(&self, reference: &str) -> Result<Vec<String>, VcsError>;

    /// Files differing between two refs.
    fn get_range_changed_files(&self, from: &str, to: &str) -> Result<Vec<String>, VcsError>;

    /// Commits reachable from `to` but not `from`, oldest first.
    /// Empty / no-op commits are filtered out.
    fn get_commits_in_range(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<StackedCommitInfo>, VcsError>;

    /// Commits selected by an arbitrary revset, oldest first.
    ///
    /// This intentionally differs from [`Self::get_commits_in_range`]: a jj
    /// revset can select a non-linear or otherwise disjoint set of commits,
    /// which has no meaningful single "base" commit. Consumers review the
    /// returned commits individually instead of diffing them against a live
    /// working copy. Git does not have an equivalent revset language.
    fn get_commits_for_revset(&self, revset: &str) -> Result<Vec<StackedCommitInfo>, VcsError> {
        let _ = revset;
        Err(VcsError::Unsupported(
            "arbitrary revset review is jj-only".into(),
        ))
    }

    /// File contents at a given ref.
    fn get_file_content_at_ref(&self, reference: &str, path: &Path) -> Result<String, VcsError>;

    /// All files in the working tree that differ from HEAD (git status),
    /// including untracked.
    fn get_working_tree_changed_files(&self) -> Result<Vec<String>, VcsError>;

    /// Files differing between the index and the working tree (git diff).
    /// On jj — where there's no index — this returns the same set as
    /// the `@-` vs `@` diff: jj's natural "uncommitted changes" view.
    fn get_unstaged_changed_files(&self) -> Result<Vec<String>, VcsError>;

    /// Files differing between HEAD and the index (git diff --cached).
    /// On jj this returns `VcsError::Unsupported`.
    fn get_staged_changed_files(&self) -> Result<Vec<String>, VcsError>;

    /// File contents at the git index. On jj this returns the file's
    /// contents at `@-` — the "before" side of jj's default diff.
    fn get_file_content_at_index(&self, path: &Path) -> Result<String, VcsError>;
}
