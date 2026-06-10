use std::str::FromStr;
use thiserror::Error;

/// The base of the diff. Either a git ref to diff the worktree against
/// (e.g. `HEAD`, `main`, `HEAD~3`, a SHA), or the literal `--cached` /
/// `--staged` to mean "staged-only" (`git diff --cached`). With no
/// reference at all, lens shows unstaged changes only (`git diff`).
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CommitReference(pub String);

impl CommitReference {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Error)]
pub enum ReferenceParseError {
    #[error("empty reference string")]
    Empty,
    #[error(
        "ranges like `a..b`, `a...b`, and `a..-` are no longer supported. \
         Pass a single ref to diff the working tree against (e.g. `main`)."
    )]
    RangeNotSupported,
}

impl FromStr for CommitReference {
    type Err = ReferenceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ReferenceParseError::Empty);
        }
        if s.contains("..") {
            return Err(ReferenceParseError::RangeNotSupported);
        }
        Ok(CommitReference(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct TestCli {
        reference: CommitReference,
    }

    #[test]
    fn test_single_ref() {
        assert_eq!(
            "HEAD".parse::<CommitReference>().unwrap(),
            CommitReference("HEAD".to_string())
        );
    }

    #[test]
    fn test_short_ref_name() {
        assert_eq!(
            "abc123".parse::<CommitReference>().unwrap(),
            CommitReference("abc123".to_string())
        );
    }

    #[test]
    fn test_branch_ref() {
        assert_eq!(
            "main".parse::<CommitReference>().unwrap(),
            CommitReference("main".to_string())
        );
    }

    #[test]
    fn test_two_dot_range_rejected() {
        assert!(matches!(
            "main..feature".parse::<CommitReference>(),
            Err(ReferenceParseError::RangeNotSupported)
        ));
    }

    #[test]
    fn test_three_dot_range_rejected() {
        assert!(matches!(
            "main...feature".parse::<CommitReference>(),
            Err(ReferenceParseError::RangeNotSupported)
        ));
    }

    #[test]
    fn test_working_tree_range_rejected() {
        assert!(matches!(
            "main..-".parse::<CommitReference>(),
            Err(ReferenceParseError::RangeNotSupported)
        ));
    }

    #[test]
    fn test_empty_reference() {
        assert!(matches!(
            "".parse::<CommitReference>(),
            Err(ReferenceParseError::Empty)
        ));
    }

    #[test]
    fn test_clap_integration() {
        let cli = TestCli::try_parse_from(["test", "main"]).unwrap();
        assert_eq!(cli.reference, CommitReference("main".to_string()));
    }
}
