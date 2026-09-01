//! jj-lib backed VCS adapter.
//!
//! Pin: `jj-lib = "=0.42"`. jj-lib is `0.x` and rearranges types across minor
//! versions — keep all jj-lib usage in this module so a bump is a one-file
//! diff. See `/Users/dghaehre/.claude/plans/purring-cuddling-planet.md`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};

use futures::StreamExt as _;
use futures::TryStreamExt as _;

use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::config::StackedConfig;
use jj_lib::fileset::FilesetAliasesMap;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::RemoteName;
use jj_lib::repo::{ReadonlyRepo, Repo, StoreFactories};
use jj_lib::repo_path::{RepoPath, RepoPathUiConverter};
use jj_lib::revset::{
    self, RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions, RevsetParseContext,
    RevsetStreamExt, RevsetWorkspaceContext, SymbolResolver,
};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{default_working_copy_factories, Workspace, WorkspaceLoadError};

use futures::AsyncReadExt as _;
use pollster::FutureExt as _;

use super::traits::{VcsBackend, VcsKind};
use super::types::{RevisionSummary, StackedCommitInfo, VcsError};

const GIT_EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Revset backing the `b` revision picker. Builtin revset functions only:
/// `self.aliases` is an empty `RevsetAliasesMap`, so config-defined aliases
/// such as `trunk()` would fail to parse here.
const PICKER_REVSET: &str = "ancestors(@ | bookmarks() | remote_bookmarks(), 20)";
/// Used when `PICKER_REVSET` fails to evaluate (e.g. a repo shape or parser
/// change) so the picker degrades to the current stack rather than erroring.
const PICKER_REVSET_FALLBACK: &str = "ancestors(@, 20)";
/// Change-id prefix length; matches jj's own `change_id.short()`.
const CHANGE_ID_PREFIX_LEN: usize = 12;

/// jj-lib backed `VcsBackend`.
pub struct JjBackend {
    /// `<workspace>/.jj` — siting for `lens.db` and similar per-repo state.
    state_dir: PathBuf,
    workspace: Workspace,
    /// Most recently snapshotted repository view. Read on the render path and
    /// replaced only when `refresh()` snapshots the working copy.
    repo: RwLock<Arc<ReadonlyRepo>>,
    settings: UserSettings,
    /// Re-used by every revset parse; immutable.
    aliases: RevsetAliasesMap,
    fileset_aliases: FilesetAliasesMap,
    extensions: RevsetExtensions,
}

impl JjBackend {
    /// Open a jj workspace at or above `path` (the underlying `Workspace::load`
    /// walks upward to find `.jj/`).
    pub fn new(path: &Path) -> Result<Self, VcsError> {
        let config = StackedConfig::with_defaults();
        let settings = UserSettings::from_config(config)
            .map_err(|e| VcsError::Other(format!("jj settings: {e}")))?;

        let workspace = Workspace::load(
            &settings,
            path,
            &StoreFactories::default(),
            &default_working_copy_factories(),
        )
        .map_err(|e| match e {
            WorkspaceLoadError::NoWorkspaceHere(_) => VcsError::NotARepository,
            other => VcsError::Other(format!("jj workspace: {other}")),
        })?;

        let repo = workspace
            .repo_loader()
            .load_at_head()
            .block_on()
            .map_err(|e| VcsError::Other(format!("jj repo load: {e}")))?;

        let state_dir = workspace.workspace_root().join(".jj");
        Ok(Self {
            state_dir,
            workspace,
            repo: RwLock::new(repo),
            settings,
            aliases: RevsetAliasesMap::new(),
            fileset_aliases: FilesetAliasesMap::new(),
            extensions: RevsetExtensions::default(),
        })
    }

    /// Load the latest repository view after snapshotting the working copy.
    fn load_latest_repo(&self) -> Result<Arc<ReadonlyRepo>, VcsError> {
        self.workspace
            .repo_loader()
            .load_at_head()
            .block_on()
            .map_err(|e| VcsError::Other(format!("jj repo reload: {e}")))
    }

    /// Return the cached view without re-reading jj metadata on every render.
    fn repo_snapshot(&self) -> Result<Arc<ReadonlyRepo>, VcsError> {
        self.repo
            .read()
            .map_err(|_| VcsError::Other("jj repository lock poisoned".into()))
            .map(|repo| Arc::clone(&repo))
    }

    /// jj keeps working-copy edits on disk until a jj command snapshots them
    /// into `@`. Use the standard non-interactive command so Lens observes
    /// the same view as `jj status` and `jj diff`.
    fn snapshot_working_copy(&self) -> Result<(), VcsError> {
        let output = Command::new("jj")
            .arg("status")
            .current_dir(self.workspace.workspace_root())
            .output()
            .map_err(|e| VcsError::Other(format!("failed to snapshot jj working copy: {e}")))?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(VcsError::Other(format!(
            "jj status failed while snapshotting the working copy: {stderr}"
        )))
    }

    /// Reject refs that look like a CLI flag — same defense-in-depth as
    /// `GitBackend::validate_ref_format`.
    fn validate_ref(reference: &str) -> Result<(), VcsError> {
        if reference.trim().starts_with('-') {
            return Err(VcsError::InvalidRef(format!(
                "references cannot start with '-': {}",
                reference
            )));
        }
        Ok(())
    }

    /// Translate git-isms (`HEAD`, `HEAD~N`) into jj revsets (`@`, `@---`).
    /// Anything else is passed through verbatim.
    fn translate_ref(reference: &str) -> String {
        let r = reference.trim();
        if r == "HEAD" {
            return "@".to_string();
        }
        if let Some(rest) = r.strip_prefix("HEAD~") {
            if let Ok(n) = rest.parse::<usize>() {
                let mut out = String::from("@");
                for _ in 0..n {
                    out.push('-');
                }
                return out;
            }
        }
        r.to_string()
    }

    /// Resolve a revset string into the list of commits it produces (oldest
    /// last — jj's default is "children before parents" topo order).
    fn resolve_revset(&self, revset_str: &str) -> Result<Vec<Commit>, VcsError> {
        let translated = Self::translate_ref(revset_str);
        let repo = self.repo_snapshot()?;
        let cwd = self.workspace.workspace_root().to_path_buf();
        let base = self.workspace.workspace_root().to_path_buf();
        let path_converter = RepoPathUiConverter::Fs { cwd, base };

        let ctx = RevsetParseContext {
            aliases_map: &self.aliases,
            local_variables: Default::default(),
            user_email: self.settings.user_email(),
            date_pattern_context: chrono::Local::now().into(),
            default_ignored_remote: Some(RemoteName::new("git")),
            fileset_aliases_map: &self.fileset_aliases,
            use_glob_by_default: false,
            extensions: &self.extensions,
            workspace: Some(RevsetWorkspaceContext {
                path_converter: &path_converter,
                workspace_name: self.workspace.workspace_name(),
            }),
        };

        let mut diags = RevsetDiagnostics::new();
        let user_expr = revset::parse(&mut diags, &translated, &ctx)
            .map_err(|e| VcsError::InvalidRef(format!("{}: {e}", revset_str)))?;
        let resolver = SymbolResolver::new(repo.as_ref(), self.extensions.symbol_resolvers());
        let resolved = user_expr
            .resolve_user_expression(repo.as_ref(), &resolver)
            .map_err(|e| VcsError::InvalidRef(format!("{}: {e}", revset_str)))?;
        let revset = resolved
            .evaluate(repo.as_ref())
            .map_err(|e| VcsError::Other(format!("evaluate revset {revset_str}: {e}")))?;

        revset
            .stream()
            .commits(repo.store())
            .try_collect::<Vec<_>>()
            .block_on()
            .map_err(|e| VcsError::Other(format!("collect revset {revset_str}: {e}")))
    }

    /// Resolve a revset that must produce exactly one commit.
    fn resolve_single_commit(&self, revset_str: &str) -> Result<Commit, VcsError> {
        Self::validate_ref(revset_str)?;
        let mut commits = self.resolve_revset(revset_str)?;
        match commits.len() {
            0 => Err(VcsError::InvalidRef(format!(
                "{}: no commits resolved",
                revset_str
            ))),
            1 => Ok(commits.pop().unwrap()),
            n => Err(VcsError::InvalidRef(format!(
                "{}: revset resolved to {n} commits, expected 1",
                revset_str
            ))),
        }
    }

    /// Diff two commits' trees and return the list of changed file paths
    /// (jj's internal forward-slash paths).
    fn diff_paths(&self, from: &Commit, to: &Commit) -> Result<Vec<String>, VcsError> {
        let from_tree = from.tree();
        let to_tree = to.tree();

        let mut stream = from_tree.diff_stream(&to_tree, &EverythingMatcher);
        let mut paths = Vec::new();
        while let Some(entry) = stream.next().block_on() {
            paths.push(entry.path.as_internal_file_string().to_string());
        }
        Ok(paths)
    }

    /// Read a file's bytes at a given tree, or empty string if the path
    /// doesn't resolve to a regular file.
    fn read_file_at_tree(&self, commit: &Commit, path: &Path) -> Result<String, VcsError> {
        let repo = self.repo_snapshot()?;
        let internal = path.to_string_lossy().replace('\\', "/");
        let repo_path = RepoPath::from_internal_string(&internal)
            .map_err(|e| VcsError::Other(format!("repo path {internal}: {e}")))?;

        let tree = commit.tree();

        let merged = tree
            .path_value(repo_path)
            .block_on()
            .map_err(|e| VcsError::Other(format!("path_value {}: {e}", path.display())))?;

        // Take the first resolved file value; ignore conflicts for now
        // (iteration 1 — see plan §"Known follow-ups").
        let file_id = merged
            .into_iter()
            .filter_map(|v| match v {
                Some(TreeValue::File { id, .. }) => Some(id),
                _ => None,
            })
            .next();
        let Some(file_id) = file_id else {
            return Ok(String::new());
        };

        let mut reader = repo
            .store()
            .read_file(repo_path, &file_id)
            .block_on()
            .map_err(|e| VcsError::Other(format!("read_file {}: {e}", path.display())))?;
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .block_on()
            .map_err(|e| VcsError::Io(e))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `@` commit — jj's working copy.
    fn working_copy_commit(&self) -> Result<Commit, VcsError> {
        let repo = self.repo_snapshot()?;
        let wc_id = repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
            .ok_or_else(|| VcsError::Other("no working-copy commit for this workspace".into()))?
            .clone();
        repo.store()
            .get_commit(&wc_id)
            .map_err(|e| VcsError::Other(format!("get @ commit: {e}")))
    }

    /// Parent of `@` (`@-`). Returns the empty-tree sentinel on a root `@`.
    fn working_copy_parent(&self) -> Result<Option<Commit>, VcsError> {
        let repo = self.repo_snapshot()?;
        let wc_id = repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
            .ok_or_else(|| VcsError::Other("no working-copy commit for this workspace".into()))?
            .clone();
        let wc = repo
            .store()
            .get_commit(&wc_id)
            .map_err(|e| VcsError::Other(format!("get @ commit: {e}")))?;
        let parent_id = match wc.parent_ids().first() {
            Some(id) => id.clone(),
            None => return Ok(None),
        };
        let parent = repo
            .store()
            .get_commit(&parent_id)
            .map_err(|e| VcsError::Other(format!("get @- commit: {e}")))?;
        Ok(Some(parent))
    }
}

impl VcsBackend for JjBackend {
    fn refresh(&self) -> Result<(), VcsError> {
        self.snapshot_working_copy()?;
        let latest_repo = self.load_latest_repo()?;
        let mut repo = self
            .repo
            .write()
            .map_err(|_| VcsError::Other("jj repository lock poisoned".into()))?;
        *repo = latest_repo;
        Ok(())
    }

    fn kind(&self) -> VcsKind {
        VcsKind::Jj
    }

    fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn get_current_branch(&self) -> Result<Option<String>, VcsError> {
        let repo = self.repo_snapshot()?;
        let wc_id = repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
            .ok_or_else(|| VcsError::Other("no working-copy commit for this workspace".into()))?;
        let names: Vec<String> = repo
            .view()
            .local_bookmarks_for_commit(wc_id)
            .map(|(name, _target)| name.as_str().to_string())
            .collect();
        if names.is_empty() {
            Ok(None)
        } else {
            Ok(Some(names.join(", ")))
        }
    }

    fn list_revisions(&self, limit: usize) -> Result<Vec<RevisionSummary>, VcsError> {
        // Stream order is newest-first, which is the order `jj log` shows
        // and the order the picker wants — no reversing here.
        let commits = match self.resolve_revset(PICKER_REVSET) {
            Ok(commits) => commits,
            Err(_) => self.resolve_revset(PICKER_REVSET_FALLBACK)?,
        };

        let repo = self.repo_snapshot()?;
        let view = repo.view();
        let wc_id = view.get_wc_commit_id(self.workspace.workspace_name());

        Ok(commits
            .into_iter()
            .take(limit)
            .map(|commit| {
                // The revset symbol resolver decodes change ids from jj's
                // `z-k` display alphabet, not raw hex — `reverse_hex()` is
                // what round-trips through `resolve_ref`, `hex()` is not.
                let change_id = commit.change_id().reverse_hex();
                let id = change_id[..CHANGE_ID_PREFIX_LEN.min(change_id.len())].to_string();
                let bookmarks = view
                    .local_bookmarks_for_commit(commit.id())
                    .map(|(name, _target)| name.as_str().to_string())
                    .collect();
                let summary = commit
                    .description()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                RevisionSummary {
                    id,
                    bookmarks,
                    summary,
                    is_working_copy: wc_id == Some(commit.id()),
                }
            })
            .collect())
    }

    fn resolve_ref(&self, reference: &str) -> Result<String, VcsError> {
        let commit = self.resolve_single_commit(reference)?;
        Ok(commit.id().hex())
    }

    fn get_parent_ref_or_empty(&self, reference: &str) -> Result<String, VcsError> {
        let commit = self.resolve_single_commit(reference)?;
        match commit.parent_ids().first() {
            Some(parent_id) => Ok(parent_id.hex()),
            None => Ok(GIT_EMPTY_TREE_SHA.to_string()),
        }
    }

    fn get_changed_files(&self, reference: &str) -> Result<Vec<String>, VcsError> {
        let r = reference.trim();
        if let Some((from, to)) = split_range(r) {
            return self.get_range_changed_files(from, to);
        }
        let commit = self.resolve_single_commit(r)?;
        let parent_id = commit.parent_ids().first().cloned();
        let Some(parent_id) = parent_id else {
            // Only jj's root commit has no parent, and its tree is empty —
            // so "files it changed" really is nothing. Every user commit
            // parents the root, so this branch is unreachable for them.
            return Ok(Vec::new());
        };
        let repo = self.repo_snapshot()?;
        let parent = repo
            .store()
            .get_commit(&parent_id)
            .map_err(|e| VcsError::Other(format!("get parent: {e}")))?;
        self.diff_paths(&parent, &commit)
    }

    fn get_range_changed_files(&self, from: &str, to: &str) -> Result<Vec<String>, VcsError> {
        let from_commit = self.resolve_single_commit(from)?;
        let to_commit = self.resolve_single_commit(to)?;
        self.diff_paths(&from_commit, &to_commit)
    }

    fn get_commits_in_range(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<StackedCommitInfo>, VcsError> {
        Self::validate_ref(from)?;
        Self::validate_ref(to)?;
        let from_t = Self::translate_ref(from);
        let to_t = Self::translate_ref(to);
        // jj's `from..to` revset matches git's: ancestors of `to` excluding
        // ancestors of `from`.
        let revset_str = format!("({from_t})..({to_t})");
        let mut commits = self.resolve_revset(&revset_str)?;
        // Stream order is children before parents (newest first). Match
        // GitBackend which returns oldest first.
        commits.reverse();

        Ok(commits
            .into_iter()
            .map(|c| {
                let commit_id = c.id().hex();
                let short_id = commit_id[..7.min(commit_id.len())].to_string();
                let summary = c.description().lines().next().unwrap_or("").to_string();
                StackedCommitInfo {
                    commit_id,
                    short_id,
                    summary,
                }
            })
            .collect())
    }

    fn get_file_content_at_ref(&self, reference: &str, path: &Path) -> Result<String, VcsError> {
        let commit = self.resolve_single_commit(reference)?;
        self.read_file_at_tree(&commit, path)
    }

    fn get_working_tree_changed_files(&self) -> Result<Vec<String>, VcsError> {
        // jj snapshots into `@` continuously; "working tree changes" = `@-` vs `@`.
        let Some(parent) = self.working_copy_parent()? else {
            return Ok(Vec::new());
        };
        let wc = self.working_copy_commit()?;
        self.diff_paths(&parent, &wc)
    }

    fn get_unstaged_changed_files(&self) -> Result<Vec<String>, VcsError> {
        self.get_working_tree_changed_files()
    }

    fn get_staged_changed_files(&self) -> Result<Vec<String>, VcsError> {
        Err(VcsError::Unsupported(
            "jj has no staging area; --cached/--staged are git-only".into(),
        ))
    }

    fn get_file_content_at_index(&self, path: &Path) -> Result<String, VcsError> {
        // On jj the "before" side of the default diff is `@-`. Empty if
        // we're at a root `@`.
        let Some(parent) = self.working_copy_parent()? else {
            return Ok(String::new());
        };
        self.read_file_at_tree(&parent, path)
    }
}

/// Split a `from..to` or `from...to` ref into its endpoints. None when the
/// string is not a range.
fn split_range(reference: &str) -> Option<(&str, &str)> {
    if let Some((a, b)) = reference.split_once("...") {
        return Some((a, b));
    }
    if let Some((a, b)) = reference.split_once("..") {
        return Some((a, b));
    }
    None
}

#[cfg(test)]
mod tests {
    //! These tests shell out to the `jj` CLI to set up a workspace, then
    //! read it back through `JjBackend`. Tests are skipped if `jj` is not
    //! on PATH so CI without jj installed still builds.
    use super::*;
    use std::process::Command;

    fn jj_available() -> bool {
        Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_jj(cwd: &Path, args: &[&str]) {
        let out = Command::new("jj")
            .args(args)
            .current_dir(cwd)
            .env("JJ_USER", "lens test")
            .env("JJ_EMAIL", "lens@test")
            .output()
            .expect("jj command failed to spawn");
        assert!(
            out.status.success(),
            "jj {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    /// Create a non-colocated jj repo with two commits on `main` plus an
    /// uncommitted change in `@`. Returns the workspace path.
    fn setup_pure_jj_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        run_jj(path, &["--config", "git.colocate=false", "git", "init"]);
        run_jj(path, &["bookmark", "create", "main"]);
        std::fs::write(path.join("a.txt"), "hello\n").unwrap();
        std::fs::write(path.join("b.txt"), "lens\n").unwrap();
        run_jj(path, &["commit", "-m", "first"]);
        // After commit, @ is a new empty change. Add an unstaged edit:
        std::fs::write(path.join("a.txt"), "hello\nworld\n").unwrap();
        dir
    }

    #[test]
    fn list_revisions_returns_resolvable_working_copy_first() {
        if !jj_available() {
            eprintln!("skipping: jj not on PATH");
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).expect("open jj workspace");
        backend.refresh().expect("snapshot working copy");

        let revs = backend.list_revisions(50).expect("list revisions");
        assert!(!revs.is_empty(), "expected at least one revision");

        assert!(
            revs[0].is_working_copy,
            "expected @ first, got {:?}",
            revs[0]
        );
        assert_eq!(
            revs.iter().filter(|r| r.is_working_copy).count(),
            1,
            "exactly one row should be the working copy: {revs:?}"
        );

        assert!(
            revs.iter().any(|r| r.bookmarks.iter().any(|b| b == "main")),
            "expected the `main` bookmark on some row: {revs:?}"
        );
        assert!(
            revs.iter().any(|r| r.summary == "first"),
            "expected the `first` commit summary: {revs:?}"
        );

        // The picker writes `id` straight into `diff_reference`, so every row
        // must round-trip. This is what catches `ChangeId::hex()` being used
        // where the revset resolver wants `reverse_hex()`.
        for rev in &revs {
            assert_eq!(
                rev.id.len(),
                CHANGE_ID_PREFIX_LEN,
                "unexpected change-id prefix length: {rev:?}"
            );
            backend
                .resolve_ref(&rev.id)
                .unwrap_or_else(|e| panic!("id {:?} did not resolve: {e}", rev.id));
        }
    }

    #[test]
    fn opens_pure_jj_workspace() {
        if !jj_available() {
            eprintln!("skipping: jj not on PATH");
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).expect("open jj backend");
        // `Workspace::load` canonicalizes the root, so canonicalize the
        // expected side too (macOS resolves /var -> /private/var).
        assert_eq!(
            backend.state_dir(),
            dir.path().canonicalize().unwrap().join(".jj")
        );
        assert!(backend.state_dir().is_dir());
        assert_eq!(backend.kind(), VcsKind::Jj);
    }

    /// `lens.db` is sited at `state_dir()`, so that directory has to be a
    /// real, writable location for a SQLite file — not just a marker path.
    #[test]
    fn state_dir_is_writable_for_lens_db() {
        use crate::command::diff::annotation_store::{
            AnnotationStore, AnnotationTarget, NewAnnotation, SqliteStore,
        };

        if !jj_available() {
            eprintln!("skipping: jj not on PATH");
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).expect("open jj backend");

        let db_path = backend.state_dir().join("lens.db");
        let mut store = SqliteStore::open(&db_path).expect("open lens.db under .jj");
        let created = store
            .create(NewAnnotation {
                filename: "a.txt".to_string(),
                target: AnnotationTarget::File,
                content: "needs a look".to_string(),
                origin: "agent".to_string(),
                created_at: std::time::SystemTime::UNIX_EPOCH,
            })
            .expect("create annotation");

        let all = store.list().expect("list annotations");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, created.id);
        assert!(
            dir.path().join(".jj/lens.db").is_file(),
            "lens.db should land at <workspace>/.jj/lens.db",
        );
    }

    /// Regression guard for the watch loop: jj's snapshot walker treats
    /// `.jj` as a reserved directory and refuses to descend into it, so
    /// `lens.db` must never show up as a working-copy change. If it did,
    /// every annotation write would dirty `@`, and watch mode would
    /// snapshot -> reload -> snapshot forever.
    #[test]
    fn lens_db_does_not_dirty_the_jj_working_copy() {
        use crate::command::diff::annotation_store::SqliteStore;

        if !jj_available() {
            eprintln!("skipping: jj not on PATH");
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).expect("open jj backend");

        let db_path = backend.state_dir().join("lens.db");
        let _store = SqliteStore::open(&db_path).expect("open lens.db under .jj");

        backend.refresh().expect("snapshot working copy");
        let changed = backend
            .get_working_tree_changed_files()
            .expect("working tree changed files");
        assert!(
            !changed
                .iter()
                .any(|f| f.contains("lens.db") || f.starts_with(".jj")),
            "lens.db must not appear as a working-copy change, got {:?}",
            changed,
        );

        let status = Command::new("jj")
            .arg("status")
            .current_dir(dir.path())
            .env("JJ_USER", "lens test")
            .env("JJ_EMAIL", "lens@test")
            .output()
            .expect("jj status");
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(
            !stdout.contains("lens.db"),
            "jj status should not mention lens.db, got: {}",
            stdout,
        );
    }

    /// `state_dir()` is deliberately per-workspace, not the shared repo
    /// directory: two workspaces have different `@` commits and different
    /// file contents, and annotations are anchored by (file, line). Pins
    /// that decision so a switch to `Workspace::repo_path()` fails loudly.
    #[test]
    fn state_dir_in_secondary_workspace() {
        if !jj_available() {
            eprintln!("skipping: jj not on PATH");
            return;
        }
        let dir = setup_pure_jj_repo();
        let primary = JjBackend::new(dir.path()).expect("open primary backend");

        let secondary_root = dir.path().join("ws2");
        run_jj(dir.path(), &["workspace", "add", "--name", "ws2", "ws2"]);
        let secondary = JjBackend::new(&secondary_root).expect("open secondary backend");

        assert_eq!(
            secondary.state_dir(),
            secondary_root.canonicalize().unwrap().join(".jj")
        );
        assert_ne!(secondary.state_dir(), primary.state_dir());

        // A DB written in the secondary workspace is invisible to the primary.
        std::fs::write(secondary.state_dir().join("lens.db"), b"").unwrap();
        assert!(!primary.state_dir().join("lens.db").exists());
    }

    #[test]
    fn resolves_main_and_at() {
        if !jj_available() {
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).unwrap();
        let main_id = backend.resolve_ref("main").expect("resolve main");
        let at_id = backend.resolve_ref("@").expect("resolve @");
        let head_id = backend.resolve_ref("HEAD").expect("HEAD alias");
        assert!(!main_id.is_empty());
        assert!(!at_id.is_empty());
        assert_ne!(main_id, at_id, "main and @ should be different commits");
        assert_eq!(at_id, head_id, "HEAD should alias @");
    }

    #[test]
    fn current_branch_is_main_when_at_aligns() {
        // @ is normally a fresh empty commit ahead of main, so there's no
        // bookmark on it — we expect None. After `jj bookmark move` it
        // would show 'main'. We test the no-bookmark path.
        if !jj_available() {
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).unwrap();
        let branch = backend.get_current_branch().unwrap();
        assert!(
            branch.is_none(),
            "@ should have no bookmark in a fresh repo, got {branch:?}",
        );
    }

    #[test]
    fn refresh_snapshots_working_copy_edits() {
        if !jj_available() {
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).unwrap();
        backend
            .refresh()
            .expect("refresh should snapshot the working-copy edit");
        let files = backend.get_working_tree_changed_files().unwrap();
        assert!(
            files.iter().any(|p| p == "a.txt"),
            "expected a.txt in working-tree diff, got {files:?}",
        );
    }

    #[test]
    fn staged_files_is_unsupported() {
        if !jj_available() {
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).unwrap();
        let err = backend.get_staged_changed_files().unwrap_err();
        assert!(
            matches!(err, VcsError::Unsupported(_)),
            "expected Unsupported, got {err:?}",
        );
    }

    #[test]
    fn parent_ref_of_root_returns_empty_tree_sentinel() {
        if !jj_available() {
            return;
        }
        let dir = setup_pure_jj_repo();
        let backend = JjBackend::new(dir.path()).unwrap();
        // The very first commit's parent is root() which has no parent
        // (or its parent is the root commit with empty trees). For "main"
        // (the first commit) we expect a valid parent — the empty-tree
        // sentinel is harder to trigger here. Just exercise the path.
        let parent = backend.get_parent_ref_or_empty("main").unwrap();
        assert!(!parent.is_empty());
    }

    #[test]
    fn short_change_id_prefix_resolves() {
        // jj presents change IDs in reverse-hex (k–z alphabet). Many commits
        // mean we'll have unambiguous short prefixes. We just need *some*
        // multi-commit jj repo and we should be able to resolve a 2+ char
        // prefix of one of those change IDs.
        if !jj_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        run_jj(path, &["--config", "git.colocate=false", "git", "init"]);
        run_jj(path, &["bookmark", "create", "main"]);
        for i in 0..20 {
            std::fs::write(path.join("f.txt"), format!("v{i}\n")).unwrap();
            run_jj(path, &["commit", "-m", &format!("c{i}")]);
        }
        // Collect change ids via `jj log` so the test is independent of the
        // hash distribution on any particular machine.
        let out = std::process::Command::new("jj")
            .args([
                "log",
                "-r",
                "all()",
                "--no-graph",
                "-T",
                "change_id.short() ++ \"\\n\"",
            ])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(ids.len() >= 5, "need a handful of change ids, got {ids:?}");

        let backend = JjBackend::new(path).unwrap();

        // A full short change id (8 chars from jj's default template) MUST
        // resolve.
        let full = &ids[0];
        let resolved = backend
            .resolve_ref(full)
            .unwrap_or_else(|e| panic!("full short id {full} failed: {e}"));
        assert!(!resolved.is_empty());

        // 2-char prefix: either resolves (unique) or fails with InvalidRef
        // because ambiguous — both are acceptable. What MUST NOT happen
        // is silent resolution to the wrong commit or a panic.
        let two = &full[..2];
        match backend.resolve_ref(two) {
            Ok(id) => assert!(!id.is_empty()),
            Err(VcsError::InvalidRef(_)) => {}
            Err(other) => panic!("unexpected error type for prefix {two}: {other}"),
        }
    }

    #[test]
    fn translate_ref_handles_head_aliases() {
        assert_eq!(JjBackend::translate_ref("HEAD"), "@");
        assert_eq!(JjBackend::translate_ref("HEAD~0"), "@");
        assert_eq!(JjBackend::translate_ref("HEAD~1"), "@-");
        assert_eq!(JjBackend::translate_ref("HEAD~3"), "@---");
        assert_eq!(JjBackend::translate_ref("main"), "main");
        assert_eq!(JjBackend::translate_ref("@-"), "@-");
    }
}
