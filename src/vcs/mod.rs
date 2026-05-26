//! VCS backend abstraction module.
//!
//! Provides Git repository detection and backend construction.

mod backend;
mod detection;
mod git;
#[cfg(test)]
pub mod test_utils;

pub use backend::{StackedCommitInfo, VcsBackend, VcsError};
pub use detection::{detect_vcs_type, VcsType};
pub use git::GitBackend;

use std::path::Path;

/// Get the appropriate VCS backend for the current directory.
pub fn get_backend(path: &Path) -> Result<Box<dyn VcsBackend>, VcsError> {
    match detect_vcs_type(path) {
        VcsType::Git => GitBackend::new(path).map(|b| Box::new(b) as Box<dyn VcsBackend>),
        VcsType::None => Err(VcsError::NotARepository),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_utils::RepoGuard;

    #[test]
    fn test_get_backend_in_git_repo() {
        let repo = RepoGuard::new();
        let backend = get_backend(&repo.dir).expect("should get backend");
        let commit = backend.get_commit("HEAD").expect("should get commit");
        assert!(!commit.commit_id.is_empty());
    }

    #[test]
    fn test_get_backend_in_non_repo_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let result = get_backend(temp.path());
        assert!(matches!(result, Err(VcsError::NotARepository)));
    }
}
