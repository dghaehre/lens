//! Thin wrapper around the `gh` CLI for PR discovery and the GraphQL/REST
//! calls the annotation `GithubStore` needs.
//!
//! Lives in `vcs/` because it is the GitHub-side counterpart to `git.rs`:
//! pure I/O against a remote VCS, with no Annotation knowledge. Tests stub
//! `GhClient` so they can exercise the JSON-parsing layer without shelling
//! out.

use std::io::Write;
use std::process::{Command, Stdio};

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
///
/// `run_with_stdin` is the same contract but pipes the supplied bytes into
/// `gh`'s stdin. Use it for `gh api -f body=@-` payloads where the body
/// may contain newlines or shell metacharacters that aren't safe to stuff
/// into argv. The default implementation just calls `run` and ignores
/// stdin — fine for test fakes that don't exercise body content.
pub trait GhClient: Send + Sync {
    fn run(&self, args: &[&str]) -> Result<String, GhError>;
    fn run_with_stdin(&self, args: &[&str], _stdin: &str) -> Result<String, GhError> {
        self.run(args)
    }
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
        classify(output.status.success(), &output.stdout, &output.stderr)
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Result<String, GhError> {
        let mut child = match Command::new("gh")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(GhError::NotInstalled)
            }
            Err(e) => return Err(GhError::GhFailure(e.to_string())),
        };
        // Closing stdin signals EOF to gh; do that immediately so it doesn't
        // wait forever for more input.
        if let Some(mut stdin_pipe) = child.stdin.take() {
            stdin_pipe
                .write_all(stdin.as_bytes())
                .map_err(|e| GhError::GhFailure(format!("write stdin: {}", e)))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| GhError::GhFailure(e.to_string()))?;
        classify(output.status.success(), &output.stdout, &output.stderr)
    }
}

/// Shared exit-code → `Result` mapping used by both `run` and
/// `run_with_stdin`. Classifies `gh`'s stderr into the auth / no-PR /
/// generic-failure variants so callers can switch on them.
fn classify(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<String, GhError> {
    if success {
        return Ok(String::from_utf8_lossy(stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    let lower = stderr.to_lowercase();
    if lower.contains("not logged into") || lower.contains("authentication required") {
        return Err(GhError::NotAuthenticated(stderr));
    }
    if lower.contains("no pull requests found") || lower.contains("no pull request") {
        return Err(GhError::NoPrForBranch);
    }
    Err(GhError::GhFailure(stderr))
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
    /// PR title as displayed on GitHub.
    pub title: String,
    /// PR description body. May be empty.
    pub body: String,
    /// `OPEN` / `CLOSED` / `MERGED`. Surfaced raw so the renderer can map
    /// to its own badge colours.
    pub state: String,
    pub is_draft: bool,
    /// `MERGEABLE` / `CONFLICTING` / `UNKNOWN`.
    pub mergeable: String,
    /// `APPROVED` / `CHANGES_REQUESTED` / `REVIEW_REQUIRED`, or empty when
    /// no review has been posted.
    pub review_decision: String,
    /// Per-check rollup. Empty when the PR has no associated checks (or
    /// when `gh` returned nothing).
    pub checks: Vec<PrCheck>,
}

/// One row from `statusCheckRollup`. The shape varies by provider:
/// GitHub Actions emits `CheckRun` (uses `status` + `conclusion`); legacy
/// status APIs emit `StatusContext` (uses `state`). We capture all three
/// so [`PrCheckSummary::from_checks`] can normalise downstream.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PrCheck {
    pub name: String,
    /// `COMPLETED` / `IN_PROGRESS` / `QUEUED` / `PENDING` / `WAITING` (CheckRun).
    pub status: Option<String>,
    /// `SUCCESS` / `FAILURE` / `SKIPPED` / `CANCELLED` / `NEUTRAL` /
    /// `TIMED_OUT` / `ACTION_REQUIRED` / `STALE` / `STARTUP_FAILURE` (CheckRun).
    pub conclusion: Option<String>,
    /// `SUCCESS` / `FAILURE` / `PENDING` / `ERROR` (StatusContext).
    pub state: Option<String>,
}

/// Bucketed counts derived from a list of checks. The renderer uses this
/// to draw a one-line "✓ N · × M · … K" summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct PrCheckSummary {
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    pub skipped: usize,
    pub total: usize,
}

impl PrCheckSummary {
    pub fn from_checks(checks: &[PrCheck]) -> Self {
        let mut s = Self::default();
        for c in checks {
            s.total += 1;
            // Prefer `conclusion` (CheckRun) over `state` (StatusContext).
            let outcome = c
                .conclusion
                .as_deref()
                .or(c.state.as_deref())
                .unwrap_or("");
            let status = c.status.as_deref().unwrap_or("");
            match outcome.to_ascii_uppercase().as_str() {
                "SUCCESS" => s.passed += 1,
                "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED" | "ERROR" => {
                    s.failed += 1
                }
                "SKIPPED" | "NEUTRAL" | "CANCELLED" | "STALE" => s.skipped += 1,
                "PENDING" | "" => {
                    // No outcome yet — bucket by status if we have it.
                    if status.eq_ignore_ascii_case("COMPLETED") {
                        // Completed but no conclusion field → treat as
                        // skipped so it doesn't masquerade as pending.
                        s.skipped += 1;
                    } else {
                        s.pending += 1;
                    }
                }
                _ => s.skipped += 1,
            }
        }
        s
    }
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
    title: String,
    #[serde(default)]
    body: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(default)]
    state: String,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
    #[serde(default)]
    mergeable: String,
    #[serde(default, rename = "reviewDecision")]
    review_decision: String,
    #[serde(default, rename = "statusCheckRollup")]
    status_check_rollup: Vec<PrCheckJson>,
}

#[derive(Deserialize, Default)]
struct PrCheckJson {
    #[serde(default)]
    name: String,
    /// CheckRun checks expose `context` instead of `name`.
    #[serde(default)]
    context: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
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
        "id,number,url,title,body,baseRefName,headRefName,baseRefOid,headRefOid,\
         state,isDraft,mergeable,reviewDecision,statusCheckRollup",
    ])?;
    let parsed: PrViewJson = serde_json::from_str(&stdout)
        .map_err(|e| GhError::ParseError(format!("gh pr view: {}", e)))?;

    let (owner, name) = parse_repo_from_pr_url(&parsed.url)?;

    let checks: Vec<PrCheck> = parsed
        .status_check_rollup
        .into_iter()
        .map(|c| PrCheck {
            // `name` is empty for StatusContext rows; fall back to `context`.
            name: if c.name.is_empty() { c.context } else { c.name },
            status: c.status,
            conclusion: c.conclusion,
            state: c.state,
        })
        .collect();

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
        title: parsed.title,
        body: parsed.body,
        state: parsed.state,
        is_draft: parsed.is_draft,
        mergeable: parsed.mergeable,
        review_decision: parsed.review_decision,
        checks,
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
            "title": "Add the thing",
            "body": "Long description\nwith multiple lines.",
            "baseRefName": "main",
            "headRefName": "feature",
            "baseRefOid": "aaaaaaa",
            "headRefOid": "bbbbbbb",
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"name": "deploy", "status": "IN_PROGRESS"}
            ]
        }"#;
        let gh = FakeGh::with(vec![Ok(body.to_string())]);
        let pr = resolve_pr_for_current_branch(&gh).expect("should parse");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.repo_owner, "dghaehre");
        assert_eq!(pr.repo_name, "lens");
        assert_eq!(pr.head_oid, "bbbbbbb");
        assert_eq!(pr.title, "Add the thing");
        assert_eq!(pr.body, "Long description\nwith multiple lines.");
        assert_eq!(pr.state, "OPEN");
        assert!(!pr.is_draft);
        assert_eq!(pr.mergeable, "MERGEABLE");
        assert_eq!(pr.review_decision, "APPROVED");
        assert_eq!(pr.checks.len(), 3);
        let summary = PrCheckSummary::from_checks(&pr.checks);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.pending, 1);
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
