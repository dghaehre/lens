//! VCS repository support — git today, jj coming.

mod git;
pub mod github;
mod jj;
#[cfg(test)]
pub mod test_utils;
mod traits;
mod types;

pub use git::GitBackend;
pub use jj::JjBackend;
pub use traits::{VcsBackend, VcsKind};
pub use types::{StackedCommitInfo, VcsError};
