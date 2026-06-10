//! GitHub-backed annotation store.
//!
//! Phase 1 is read-only: `new()` fetches the PR's review threads via the
//! `gh` CLI, maps them into `Annotation`s, and caches them in memory. All
//! mutators return `AnnotationError::ReadOnly`. Phase 2 will swap that for
//! real `gh api` mutations.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::vcs::github::{GhClient, GhError, PrInfo};

use super::super::types::DiffPanelFocus;
use super::{
    is_github_origin, Annotation, AnnotationError, AnnotationId, AnnotationStore,
    AnnotationTarget, Author, Capabilities, Comment, NewAnnotation, NewReply,
};

/// `AnnotationStore::id` value. Annotations also carry `origin = "github"`
/// (see `GITHUB_ORIGINS` in `mod.rs`), kept separate from the store id so a
/// future "github-enterprise" variant could share the origin tag.
pub const STORE_ID: &str = "github";

pub struct GithubStore {
    #[allow(dead_code)] // used by Phase 2 mutations
    pr: PrInfo,
    #[allow(dead_code)] // used by Phase 2 mutations
    gh: Box<dyn GhClient>,
    /// Materialized annotations, populated at construction. The trait's
    /// `list()` signature is `&self`, so we can't refetch on read — Phase 1
    /// rebuilds the store on every source-switch instead.
    cache: Vec<Annotation>,
}

impl GithubStore {
    /// Construct and eagerly populate the cache by querying review threads.
    /// Fails fast if the query errors so callers can show a useful toast
    /// without entering a half-broken state.
    pub fn new(pr: PrInfo, gh: Box<dyn GhClient>) -> Result<Self, GhError> {
        let cache = fetch_threads(&*gh, &pr)?;
        Ok(Self { pr, gh, cache })
    }

    #[allow(dead_code)] // used by Phase 2 / footer drift check
    pub fn pr(&self) -> &PrInfo {
        &self.pr
    }
}

impl AnnotationStore for GithubStore {
    fn id(&self) -> &str {
        STORE_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Flips to false once Phase 2 lands mutations + drift detection.
            read_only: true,
            supports_file_target: true,
            supports_line_range_target: true,
        }
    }

    fn list(&self) -> Result<Vec<Annotation>, AnnotationError> {
        Ok(self.cache.clone())
    }

    fn list_for_files(&self, filenames: &[String]) -> Result<Vec<Annotation>, AnnotationError> {
        let set: HashSet<&str> = filenames.iter().map(|s| s.as_str()).collect();
        Ok(self
            .cache
            .iter()
            .filter(|a| set.contains(a.filename.as_str()))
            .cloned()
            .collect())
    }

    fn create(&mut self, _draft: NewAnnotation) -> Result<Annotation, AnnotationError> {
        Err(AnnotationError::ReadOnly)
    }

    fn update(&mut self, id: &AnnotationId, _content: String) -> Result<(), AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        Err(AnnotationError::ReadOnly)
    }

    fn reply(
        &mut self,
        id: &AnnotationId,
        _reply: NewReply,
    ) -> Result<Annotation, AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        Err(AnnotationError::ReadOnly)
    }

    fn set_resolved(
        &mut self,
        id: &AnnotationId,
        _resolved: bool,
    ) -> Result<(), AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        Err(AnnotationError::ReadOnly)
    }

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        Err(AnnotationError::ReadOnly)
    }
}

// ---- GraphQL fetch + mapping ----

/// Build the `reviewThreads` GraphQL query for a PR. The shape is tuned to
/// match `ThreadJson` / `CommentJson` below; pagination caps at 100 of each
/// which is plenty for a single review.
fn build_query(pr: &PrInfo) -> String {
    format!(
        r#"query {{
            repository(owner: "{owner}", name: "{name}") {{
                pullRequest(number: {num}) {{
                    reviewThreads(first: 100) {{
                        nodes {{
                            id
                            isResolved
                            subjectType
                            path
                            line
                            startLine
                            originalLine
                            originalStartLine
                            diffSide
                            comments(first: 100) {{
                                nodes {{
                                    databaseId
                                    body
                                    createdAt
                                    author {{ login }}
                                }}
                            }}
                        }}
                    }}
                }}
            }}
        }}"#,
        owner = pr.repo_owner,
        name = pr.repo_name,
        num = pr.number,
    )
}

fn fetch_threads(gh: &dyn GhClient, pr: &PrInfo) -> Result<Vec<Annotation>, GhError> {
    let query = build_query(pr);
    let stdout = gh.run(&["api", "graphql", "-f", &format!("query={}", query)])?;
    let parsed: GraphQlResponse = serde_json::from_str(&stdout)
        .map_err(|e| GhError::ParseError(format!("reviewThreads: {}", e)))?;
    let threads = parsed
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .map(|p| p.review_threads.nodes)
        .unwrap_or_default();
    Ok(threads
        .into_iter()
        .filter_map(thread_to_annotation)
        .collect())
}

fn thread_to_annotation(t: ThreadJson) -> Option<Annotation> {
    // GitHub returns threads with no comments only in edge cases (e.g.,
    // permission-stripped views). Skip rather than violate our
    // "comments[0] is the root" invariant.
    if t.comments.nodes.is_empty() {
        return None;
    }

    let path = t.path.clone()?;
    let target = match t.subject_type.as_deref() {
        Some("FILE") => AnnotationTarget::File,
        // `line == null` happens for legacy file-level comments that
        // predate `subjectType` on the GraphQL API; treat them as file.
        _ if t.line.is_none() && t.original_line.is_none() => AnnotationTarget::File,
        _ => {
            // Prefer current-diff lines; fall back to `originalLine` for
            // outdated threads so they still anchor somewhere useful.
            let end_line = t.line.or(t.original_line)? as usize;
            let start_line = t
                .start_line
                .or(t.original_start_line)
                .map(|n| n as usize)
                .unwrap_or(end_line);
            let panel = match t.diff_side.as_deref() {
                Some("LEFT") => DiffPanelFocus::Old,
                _ => DiffPanelFocus::New,
            };
            AnnotationTarget::LineRange {
                panel,
                start_line,
                end_line,
            }
        }
    };

    let comments: Vec<Comment> = t
        .comments
        .nodes
        .into_iter()
        .map(comment_from_json)
        .collect();

    Some(Annotation {
        id: AnnotationId {
            origin: "github".to_string(),
            native: t.id,
        },
        filename: path,
        target,
        comments,
        resolved: t.is_resolved,
    })
}

fn comment_from_json(c: CommentJson) -> Comment {
    let login = c
        .author
        .as_ref()
        .map(|a| a.login.clone())
        .unwrap_or_else(|| "ghost".to_string());
    Comment {
        native_id: c.database_id.map(|id| id.to_string()),
        author: Author {
            login: login.clone(),
            display_name: None,
        },
        origin: "github".to_string(),
        content: c.body,
        created_at: parse_iso8601_utc(&c.created_at).unwrap_or(UNIX_EPOCH),
    }
}

// ---- JSON shapes returned by `gh api graphql` ----

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<GqlData>,
}

#[derive(Deserialize)]
struct GqlData {
    repository: Option<GqlRepository>,
}

#[derive(Deserialize)]
struct GqlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GqlPullRequest>,
}

#[derive(Deserialize)]
struct GqlPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: GqlThreads,
}

#[derive(Deserialize)]
struct GqlThreads {
    nodes: Vec<ThreadJson>,
}

#[derive(Deserialize)]
struct ThreadJson {
    id: String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "subjectType")]
    subject_type: Option<String>,
    path: Option<String>,
    line: Option<i64>,
    #[serde(rename = "startLine")]
    start_line: Option<i64>,
    #[serde(rename = "originalLine")]
    original_line: Option<i64>,
    #[serde(rename = "originalStartLine")]
    original_start_line: Option<i64>,
    #[serde(rename = "diffSide")]
    diff_side: Option<String>,
    comments: GqlComments,
}

#[derive(Deserialize)]
struct GqlComments {
    nodes: Vec<CommentJson>,
}

#[derive(Deserialize)]
struct CommentJson {
    #[serde(rename = "databaseId")]
    database_id: Option<i64>,
    body: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    author: Option<AuthorJson>,
}

#[derive(Deserialize)]
struct AuthorJson {
    login: String,
}

// ---- timestamp parsing ----

/// Parse a strict-UTC ISO-8601 timestamp of the shape GitHub returns
/// (`YYYY-MM-DDTHH:MM:SSZ`, optionally with fractional seconds we discard).
/// Returns `None` for anything else rather than guessing.
fn parse_iso8601_utc(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    if s.len() < 20 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' || bytes[16] != b':'
    {
        return None;
    }
    let year: i32 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let min: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let sec: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    let days = days_since_epoch(year, month, day)?;
    let secs = days * 86400 + (hour as i64) * 3600 + (min as i64) * 60 + sec as i64;
    if secs < 0 {
        Some(UNIX_EPOCH)
    } else {
        Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
    }
}

fn days_since_epoch(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut days: i64 = 0;
    for year in 1970..y {
        days += if is_leap(year) { 366 } else { 365 };
    }
    let mdays = [
        31,
        if is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for mm in 1..m as usize {
        days += mdays[mm - 1];
    }
    days += d as i64 - 1;
    Some(days)
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeGh {
        responses: Mutex<Vec<Result<String, GhError>>>,
    }

    impl FakeGh {
        fn with(mut responses: Vec<Result<String, GhError>>) -> Self {
            responses.reverse(); // serve in call order
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

    fn pr() -> PrInfo {
        PrInfo {
            number: 7,
            url: "https://github.com/o/r/pull/7".into(),
            node_id: "PR_x".into(),
            repo_owner: "o".into(),
            repo_name: "r".into(),
            base_ref: "main".into(),
            head_ref: "feat".into(),
            base_oid: "a".into(),
            head_oid: "b".into(),
        }
    }

    /// Two threads: one line-range on the RIGHT side with a reply, one
    /// file-level resolved thread on a different file. Exercises the
    /// mapping for both targets, the resolved badge, and the reply chain
    /// (root + 1 reply).
    fn two_thread_payload() -> &'static str {
        r#"{
          "data": {
            "repository": {
              "pullRequest": {
                "reviewThreads": {
                  "nodes": [
                    {
                      "id": "PRRT_1",
                      "isResolved": false,
                      "subjectType": "LINE",
                      "path": "src/main.rs",
                      "line": 12,
                      "startLine": 10,
                      "originalLine": 12,
                      "originalStartLine": 10,
                      "diffSide": "RIGHT",
                      "comments": {
                        "nodes": [
                          {
                            "databaseId": 1001,
                            "body": "nit: rename",
                            "createdAt": "2024-01-15T12:34:56Z",
                            "author": { "login": "alice" }
                          },
                          {
                            "databaseId": 1002,
                            "body": "sure",
                            "createdAt": "2024-01-15T12:40:00Z",
                            "author": { "login": "bob" }
                          }
                        ]
                      }
                    },
                    {
                      "id": "PRRT_2",
                      "isResolved": true,
                      "subjectType": "FILE",
                      "path": "README.md",
                      "line": null,
                      "startLine": null,
                      "originalLine": null,
                      "originalStartLine": null,
                      "diffSide": "RIGHT",
                      "comments": {
                        "nodes": [
                          {
                            "databaseId": 2001,
                            "body": "file-level note",
                            "createdAt": "2024-01-16T08:00:00Z",
                            "author": { "login": "alice" }
                          }
                        ]
                      }
                    }
                  ]
                }
              }
            }
          }
        }"#
    }

    #[test]
    fn maps_line_range_and_file_threads() {
        let gh = FakeGh::with(vec![Ok(two_thread_payload().to_string())]);
        let store = GithubStore::new(pr(), Box::new(gh)).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);

        let line = &all[0];
        assert_eq!(line.filename, "src/main.rs");
        assert_eq!(line.id.origin, "github");
        assert_eq!(line.id.native, "PRRT_1");
        assert!(!line.resolved);
        match &line.target {
            AnnotationTarget::LineRange {
                panel,
                start_line,
                end_line,
            } => {
                assert!(matches!(panel, DiffPanelFocus::New));
                assert_eq!(*start_line, 10);
                assert_eq!(*end_line, 12);
            }
            other => panic!("expected LineRange, got {:?}", other),
        }
        assert_eq!(line.comments.len(), 2);
        assert_eq!(line.comments[0].content, "nit: rename");
        assert_eq!(line.comments[0].author.login, "alice");
        assert_eq!(line.comments[1].content, "sure");

        let file = &all[1];
        assert_eq!(file.filename, "README.md");
        assert!(file.resolved);
        assert!(matches!(file.target, AnnotationTarget::File));
    }

    #[test]
    fn left_side_maps_to_old_panel() {
        let payload = r#"{
          "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [
            {
              "id": "PRRT_L",
              "isResolved": false,
              "subjectType": "LINE",
              "path": "a.rs",
              "line": 5,
              "startLine": null,
              "originalLine": 5,
              "originalStartLine": null,
              "diffSide": "LEFT",
              "comments": { "nodes": [
                { "databaseId": 1, "body": "x", "createdAt": "2024-01-01T00:00:00Z", "author": { "login": "u" } }
              ] }
            }
          ] } } } }
        }"#;
        let gh = FakeGh::with(vec![Ok(payload.to_string())]);
        let store = GithubStore::new(pr(), Box::new(gh)).unwrap();
        let all = store.list().unwrap();
        match &all[0].target {
            AnnotationTarget::LineRange { panel, start_line, end_line } => {
                assert!(matches!(panel, DiffPanelFocus::Old));
                assert_eq!(*start_line, 5);
                assert_eq!(*end_line, 5);
            }
            _ => panic!("expected LineRange"),
        }
    }

    #[test]
    fn list_for_files_filters_by_filename() {
        let gh = FakeGh::with(vec![Ok(two_thread_payload().to_string())]);
        let store = GithubStore::new(pr(), Box::new(gh)).unwrap();
        let only_readme = store
            .list_for_files(&["README.md".to_string()])
            .unwrap();
        assert_eq!(only_readme.len(), 1);
        assert_eq!(only_readme[0].filename, "README.md");
    }

    #[test]
    fn all_mutators_return_read_only() {
        let gh = FakeGh::with(vec![Ok(two_thread_payload().to_string())]);
        let mut store = GithubStore::new(pr(), Box::new(gh)).unwrap();
        let id = AnnotationId {
            origin: "github".to_string(),
            native: "PRRT_1".to_string(),
        };
        assert!(matches!(
            store.update(&id, "x".to_string()),
            Err(AnnotationError::ReadOnly)
        ));
        assert!(matches!(store.delete(&id), Err(AnnotationError::ReadOnly)));
        assert!(matches!(
            store.set_resolved(&id, true),
            Err(AnnotationError::ReadOnly)
        ));
        assert!(matches!(
            store.reply(
                &id,
                NewReply {
                    content: "x".to_string(),
                    origin: "github".to_string(),
                    created_at: UNIX_EPOCH,
                }
            ),
            Err(AnnotationError::ReadOnly)
        ));
    }

    #[test]
    fn mutators_reject_wrong_origin_before_read_only_check() {
        let gh = FakeGh::with(vec![Ok(two_thread_payload().to_string())]);
        let mut store = GithubStore::new(pr(), Box::new(gh)).unwrap();
        let local_id = AnnotationId {
            origin: "ui".to_string(),
            native: "1".to_string(),
        };
        assert!(matches!(
            store.update(&local_id, "x".to_string()),
            Err(AnnotationError::WrongOrigin)
        ));
    }

    #[test]
    fn parses_iso8601_zulu() {
        // 2024-01-15T12:34:56Z = 1705322096
        let t = parse_iso8601_utc("2024-01-15T12:34:56Z").unwrap();
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_705_322_096
        );
    }

    #[test]
    fn malformed_iso8601_returns_none() {
        assert!(parse_iso8601_utc("nope").is_none());
        assert!(parse_iso8601_utc("2024/01/15 12:34:56").is_none());
    }
}
