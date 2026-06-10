//! Thin wrapper around the `gh` CLI for PR discovery and the GraphQL/REST
//! calls the annotation `GithubStore` needs.
//!
//! Lives in `vcs/` because it is the GitHub-side counterpart to `git.rs`:
//! pure I/O against a remote VCS, with no Annotation knowledge. Tests stub
//! `GhClient` so they can exercise the JSON-parsing layer without shelling
//! out.

use std::process::Command;

use serde::Deserialize;

/// Error returned by [`GhClient::run`] and the higher-level helpers in this
/// module. Variants encode the cases callers want to handle differently
/// (missing CLI vs. auth vs. no-PR), so the UI can show a useful toast
/// instead of a wrapped string.
#[derive(Debug, Clone)]
pub enum GhError {
    /// `gh` is not on `$PATH`.
    NotInstalled,
    /// `gh` reported "you are not logged in" (or similar). The original
    /// stderr is preserved for display.
    NotAuthenticated(String),
    /// The current branch has no associated PR.
    NoPrForBranch,
    /// Any other non-zero exit from `gh`. Holds the trimmed stderr.
    GhFailure(String),
    /// JSON returned by `gh` did not match the expected shape.
    ParseError(String),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(
                f,
                "`gh` CLI not found on PATH (install from https://cli.github.com)"
            ),
            Self::NotAuthenticated(msg) => write!(f, "gh not authenticated: {}", msg),
            Self::NoPrForBranch => write!(f, "no pull request found for the current branch"),
            Self::GhFailure(msg) => write!(f, "gh: {}", msg),
            Self::ParseError(msg) => write!(f, "could not parse gh response: {}", msg),
        }
    }
}

impl std::error::Error for GhError {}

/// Abstracts the `gh` CLI so unit tests can substitute a fake.
///
/// Implementors run `gh <args...>` and return stdout (UTF-8 lossy), or a
/// classified [`GhError`] on failure.
pub trait GhClient: Send + Sync {
    fn run(&self, args: &[&str]) -> Result<String, GhError>;
}

/// Default [`GhClient`] that shells out to a real `gh` binary.
#[derive(Default)]
pub struct RealGhClient;

impl GhClient for RealGhClient {
    fn run(&self, args: &[&str]) -> Result<String, GhError> {
        let output = match Command::new("gh").args(args).output() {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(GhError::NotInstalled)
            }
            Err(e) => return Err(GhError::GhFailure(e.to_string())),
        };
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).to_string());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // `gh` writes a distinctive "you are not logged into any GitHub hosts"
        // message on auth failure; route that to a dedicated variant so the
        // UI can suggest `gh auth login`.
        let lower = stderr.to_lowercase();
        if lower.contains("not logged into") || lower.contains("authentication required") {
            return Err(GhError::NotAuthenticated(stderr));
        }
        if lower.contains("no pull requests found") || lower.contains("no pull request") {
            return Err(GhError::NoPrForBranch);
        }
        Err(GhError::GhFailure(stderr))
    }
}

/// Everything the GitHub-backed annotation store needs to know about a PR.
/// `*_oid` are the commit SHAs at the time the PR was last looked up; we use
/// them both for accurate line anchoring (Phase 2 writes) and for the
/// "checkout drift" check that flips the store into read-only mode.
///
/// Several fields (`url`, `node_id`, `base_ref`/`head_ref`, `*_oid`) are
/// only consumed by Phase 2 mutations; the `dead_code` allow keeps them in
/// place without warnings.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    /// GraphQL node id (`PR_…`). Needed by Phase-2 mutations.
    pub node_id: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub base_ref: String,
    pub head_ref: String,
    pub base_oid: String,
    pub head_oid: String,
}

impl PrInfo {
    pub fn slug(&self) -> String {
        format!("{}/{}#{}", self.repo_owner, self.repo_name, self.number)
    }
}

#[derive(Deserialize)]
struct PrViewJson {
    id: String,
    number: u64,
    url: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
}

/// Look up the PR associated with the current branch using `gh pr view`.
///
/// `gh pr view --json` does not expose `baseRepository` as a top-level
/// field (only `headRepository`), so we derive `repo_owner`/`repo_name` by
/// parsing the PR URL — which always points at the *base* repo where the
/// PR lives, regardless of whether the head is a fork. That keeps the
/// query schema-stable across `gh` versions too.
///
/// Errors map to a `GhError` variant the caller can switch on — typically:
/// `NotInstalled` → suggest installing `gh`; `NotAuthenticated` → suggest
/// `gh auth login`; `NoPrForBranch` → silently stay on the local source.
pub fn resolve_pr_for_current_branch(gh: &dyn GhClient) -> Result<PrInfo, GhError> {
    let stdout = gh.run(&[
        "pr",
        "view",
        "--json",
        "id,number,url,baseRefName,headRefName,baseRefOid,headRefOid",
    ])?;
    let parsed: PrViewJson = serde_json::from_str(&stdout)
        .map_err(|e| GhError::ParseError(format!("gh pr view: {}", e)))?;

    let (owner, name) = parse_repo_from_pr_url(&parsed.url)?;

    Ok(PrInfo {
        number: parsed.number,
        url: parsed.url,
        node_id: parsed.id,
        repo_owner: owner,
        repo_name: name,
        base_ref: parsed.base_ref_name,
        head_ref: parsed.head_ref_name,
        base_oid: parsed.base_ref_oid,
        head_oid: parsed.head_ref_oid,
    })
}

/// Extract `(owner, repo)` from a GitHub PR URL of the form
/// `https://github.com/<owner>/<repo>/pull/<n>`. Tolerates GHES hostnames
/// by matching on the literal path component after `github.com/` — for
/// non-`github.com` deployments we fall back to whatever follows the host.
fn parse_repo_from_pr_url(url: &str) -> Result<(String, String), GhError> {
    let after_host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    let path = after_host
        .split_once('/')
        .map(|(_, rest)| rest)
        .ok_or_else(|| GhError::ParseError(format!("PR url has no path: {}", url)))?;
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    if owner.is_empty() || name.is_empty() {
        return Err(GhError::ParseError(format!(
            "could not parse owner/repo from PR url: {}",
            url
        )));
    }
    Ok((owner.to_string(), name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Minimal fake `GhClient` for unit tests. Returns a queued response per
    /// call; panics if the queue is exhausted so missing fixtures surface
    /// loudly.
    pub struct FakeGh {
        responses: Mutex<Vec<Result<String, GhError>>>,
    }

    impl FakeGh {
        pub fn with(responses: Vec<Result<String, GhError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl GhClient for FakeGh {
        fn run(&self, _args: &[&str]) -> Result<String, GhError> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .expect("FakeGh: no more queued responses")
        }
    }

    #[test]
    fn parses_pr_view_response() {
        let body = r#"{
            "id": "PR_kwDOABCD1234",
            "number": 42,
            "url": "https://github.com/dghaehre/lens/pull/42",
            "baseRefName": "main",
            "headRefName": "feature",
            "baseRefOid": "aaaaaaa",
            "headRefOid": "bbbbbbb"
        }"#;
        let gh = FakeGh::with(vec![Ok(body.to_string())]);
        let pr = resolve_pr_for_current_branch(&gh).expect("should parse");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.repo_owner, "dghaehre");
        assert_eq!(pr.repo_name, "lens");
        assert_eq!(pr.head_oid, "bbbbbbb");
        assert_eq!(pr.slug(), "dghaehre/lens#42");
    }

    #[test]
    fn parses_repo_from_fork_pr_url() {
        // PR opened from a fork still has the URL pointing at the base
        // (upstream) repo where the PR lives.
        let (owner, name) =
            parse_repo_from_pr_url("https://github.com/dghaehre/lens/pull/99").unwrap();
        assert_eq!(owner, "dghaehre");
        assert_eq!(name, "lens");
    }

    #[test]
    fn rejects_url_without_owner_repo() {
        assert!(matches!(
            parse_repo_from_pr_url("https://github.com/"),
            Err(GhError::ParseError(_))
        ));
    }

    #[test]
    fn surfaces_no_pr_error() {
        let gh = FakeGh::with(vec![Err(GhError::NoPrForBranch)]);
        match resolve_pr_for_current_branch(&gh) {
            Err(GhError::NoPrForBranch) => {}
            other => panic!("expected NoPrForBranch, got {:?}", other),
        }
    }

    #[test]
    fn parse_error_on_bad_json() {
        let gh = FakeGh::with(vec![Ok("not json".to_string())]);
        match resolve_pr_for_current_branch(&gh) {
            Err(GhError::ParseError(_)) => {}
            other => panic!("expected ParseError, got {:?}", other),
        }
    }
}
