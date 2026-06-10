//! Git repository support.

mod git;
pub mod github;
#[cfg(test)]
pub mod test_utils;
mod types;

pub use git::GitBackend;
pub use types::{StackedCommitInfo, VcsError};
