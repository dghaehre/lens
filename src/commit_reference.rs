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
        "unsupported range syntax. Use a bounded jj range such as `main..@`. \
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
        if s.contains("..") && split_endpoint_range(s).is_none() {
            return Err(ReferenceParseError::RangeNotSupported);
        }
        Ok(CommitReference(s.to_string()))
    }
}

/// A top-level, bounded two-dot range. Nested/quoted revsets retain their
/// normal revset meaning; each endpoint is validated by the backend.
pub fn split_endpoint_range(raw: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut split = None;
    let bytes = raw.as_bytes();
    for (i, c) in raw.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1)?,
            '.' if depth == 0 && bytes.get(i + 1) == Some(&b'.') => {
                if split.is_some() || bytes.get(i + 2) == Some(&b'.') {
                    return None;
                }
                split = Some(i);
            }
            _ => {}
        }
    }
    if depth != 0 || quote.is_some() {
        return None;
    }
    let i = split?;
    let (from, to) = (raw[..i].trim(), raw[i + 2..].trim());
    if from.is_empty() || to.is_empty() || to == "-" {
        None
    } else {
        Some((from, to))
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
    fn test_two_dot_range_accepted() {
        assert!("main..feature".parse::<CommitReference>().is_ok());
        assert_eq!(split_endpoint_range("trunk()..@"), Some(("trunk()", "@")));
        assert_eq!(split_endpoint_range("main..@..other"), None);
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
