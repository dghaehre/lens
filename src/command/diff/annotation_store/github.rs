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
    is_github_origin, Annotation, AnnotationError, AnnotationId, AnnotationStore, AnnotationTarget,
    Author, Capabilities, Comment, NewAnnotation, NewReply,
};

/// `AnnotationStore::id` value. Annotations also carry `origin = "github"`
/// (see `GITHUB_ORIGINS` in `mod.rs`), kept separate from the store id so a
/// future "github-enterprise" variant could share the origin tag.
pub const STORE_ID: &str = "github";

pub struct GithubStore {
    pr: PrInfo,
    gh: Box<dyn GhClient>,
    /// Materialized annotations, populated at construction. The trait's
    /// `list()` signature is `&self`, so reads come from this cache —
    /// every mutation method refetches the affected thread (or the whole
    /// list) to keep it in sync with GitHub.
    cache: Vec<Annotation>,
    /// Whether the local checkout matches `pr.head_oid`. When `false`,
    /// `capabilities().read_only` flips to `true` so the UI disables
    /// mutations — see the drift comment in `state.rs`.
    writable: bool,
}

impl GithubStore {
    /// Construct and eagerly populate the cache by querying review threads.
    /// Fails fast if the query errors so callers can show a useful toast
    /// without entering a half-broken state. `writable=false` puts the
    /// store in read-only mode regardless of mutation request.
    pub fn new(pr: PrInfo, gh: Box<dyn GhClient>, writable: bool) -> Result<Self, GhError> {
        let cache = fetch_threads(&*gh, &pr)?;
        Ok(Self {
            pr,
            gh,
            cache,
            writable,
        })
    }

    #[allow(dead_code)] // exposed for future per-PR UI affordances
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
            read_only: !self.writable,
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

    fn create(&mut self, draft: NewAnnotation) -> Result<Annotation, AnnotationError> {
        if !self.writable {
            return Err(AnnotationError::ReadOnly);
        }
        let path = format!(
            "repos/{}/{}/pulls/{}/comments",
            self.pr.repo_owner, self.pr.repo_name, self.pr.number
        );
        let body = build_create_payload(&self.pr.head_oid, &draft)?;
        let resp = api_post_json(&*self.gh, &path, &body).map_err(gh_err)?;

        // The POST returns just the new comment, not its enclosing thread.
        // Refetch threads and find the one whose root comment matches the
        // returned databaseId — that's the only stable handle GitHub gives
        // us back across REST + GraphQL.
        let new_comment_id = resp
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| {
                AnnotationError::Backend("create: response missing comment `id`".to_string())
            })?;
        self.cache = fetch_threads(&*self.gh, &self.pr).map_err(gh_err)?;
        self.cache
            .iter()
            .find(|a| {
                a.comments.first().and_then(|c| c.native_id.as_deref())
                    == Some(new_comment_id.as_str())
            })
            .cloned()
            .ok_or_else(|| {
                AnnotationError::Backend(
                    "create: comment posted but not found in refreshed cache".to_string(),
                )
            })
    }

    fn update(&mut self, id: &AnnotationId, content: String) -> Result<(), AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        if !self.writable {
            return Err(AnnotationError::ReadOnly);
        }
        let root_id = self.root_comment_id(id)?;
        let path = format!(
            "repos/{}/{}/pulls/comments/{}",
            self.pr.repo_owner, self.pr.repo_name, root_id
        );
        let body = serde_json::json!({ "body": content.clone() });
        api_patch_json(&*self.gh, &path, &body).map_err(gh_err)?;
        if let Some(a) = self.cache.iter_mut().find(|a| a.id.native == id.native) {
            a.root_mut().content = content;
        }
        Ok(())
    }

    fn reply(&mut self, id: &AnnotationId, reply: NewReply) -> Result<Annotation, AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        if !self.writable {
            return Err(AnnotationError::ReadOnly);
        }
        let root_id = self.root_comment_id(id)?;
        let path = format!(
            "repos/{}/{}/pulls/{}/comments/{}/replies",
            self.pr.repo_owner, self.pr.repo_name, self.pr.number, root_id
        );
        let body = serde_json::json!({ "body": reply.content });
        api_post_json(&*self.gh, &path, &body).map_err(gh_err)?;
        // Refetch so the new reply shows up with a stable native_id.
        self.cache = fetch_threads(&*self.gh, &self.pr).map_err(gh_err)?;
        self.cache
            .iter()
            .find(|a| a.id.native == id.native)
            .cloned()
            .ok_or(AnnotationError::NotFound)
    }

    fn set_resolved(&mut self, id: &AnnotationId, resolved: bool) -> Result<(), AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        if !self.writable {
            return Err(AnnotationError::ReadOnly);
        }
        if !self.cache.iter().any(|a| a.id.native == id.native) {
            return Err(AnnotationError::NotFound);
        }
        // `resolveReviewThread` / `unresolveReviewThread` take the GraphQL
        // thread node id, which is exactly what we stored as
        // `AnnotationId.native` at fetch time.
        let mutation = if resolved {
            format!(
                r#"mutation {{ resolveReviewThread(input: {{ threadId: "{}" }}) {{ thread {{ id }} }} }}"#,
                id.native
            )
        } else {
            format!(
                r#"mutation {{ unresolveReviewThread(input: {{ threadId: "{}" }}) {{ thread {{ id }} }} }}"#,
                id.native
            )
        };
        graphql_mutation(&*self.gh, &mutation).map_err(gh_err)?;
        if let Some(a) = self.cache.iter_mut().find(|a| a.id.native == id.native) {
            a.resolved = resolved;
        }
        Ok(())
    }

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError> {
        if !is_github_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        if !self.writable {
            return Err(AnnotationError::ReadOnly);
        }
        let root_id = self.root_comment_id(id)?;
        let path = format!(
            "repos/{}/{}/pulls/comments/{}",
            self.pr.repo_owner, self.pr.repo_name, root_id
        );
        api_delete(&*self.gh, &path).map_err(gh_err)?;
        // Deleting the root cascades the thread on GitHub's side.
        self.cache.retain(|a| a.id.native != id.native);
        Ok(())
    }
}

impl GithubStore {
    /// Resolve the REST comment id of the root comment for a given
    /// annotation, used by every mutation that targets a specific comment
    /// (update / reply / delete).
    fn root_comment_id(&self, id: &AnnotationId) -> Result<String, AnnotationError> {
        let ann = self
            .cache
            .iter()
            .find(|a| a.id.native == id.native)
            .ok_or(AnnotationError::NotFound)?;
        ann.comments
            .first()
            .and_then(|c| c.native_id.clone())
            .ok_or_else(|| {
                AnnotationError::Backend(
                    "annotation has no root comment id (broken cache?)".to_string(),
                )
            })
    }
}

// ---- mutation helpers ----

fn gh_err(e: GhError) -> AnnotationError {
    AnnotationError::Backend(e.to_string())
}

/// Build the JSON body for `POST .../pulls/{n}/comments`. Routes the
/// `LineRange { panel: Old }` to `side=LEFT`, `New` to `side=RIGHT`, and
/// `File` targets to `subject_type=file` (no line/side).
fn build_create_payload(
    head_oid: &str,
    draft: &NewAnnotation,
) -> Result<serde_json::Value, AnnotationError> {
    let mut body = serde_json::json!({
        "commit_id": head_oid,
        "path": draft.filename,
        "body": draft.content,
    });
    match &draft.target {
        AnnotationTarget::File => {
            body["subject_type"] = "file".into();
        }
        AnnotationTarget::LineRange {
            panel,
            start_line,
            end_line,
        } => {
            let side = match panel {
                DiffPanelFocus::Old => "LEFT",
                _ => "RIGHT",
            };
            body["side"] = side.into();
            body["line"] = (*end_line as i64).into();
            if start_line != end_line {
                body["start_side"] = side.into();
                body["start_line"] = (*start_line as i64).into();
            }
        }
    }
    Ok(body)
}

fn api_post_json(
    gh: &dyn GhClient,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, GhError> {
    let stdout = gh.run_with_stdin(
        &["api", "-X", "POST", path, "--input", "-"],
        &body.to_string(),
    )?;
    parse_json(&stdout, path)
}

fn api_patch_json(
    gh: &dyn GhClient,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, GhError> {
    let stdout = gh.run_with_stdin(
        &["api", "-X", "PATCH", path, "--input", "-"],
        &body.to_string(),
    )?;
    parse_json(&stdout, path)
}

fn api_delete(gh: &dyn GhClient, path: &str) -> Result<(), GhError> {
    // DELETE returns 204 with empty body — we don't need to parse anything.
    gh.run(&["api", "-X", "DELETE", path])?;
    Ok(())
}

/// Run a GraphQL mutation. `gh api graphql` returns HTTP 200 even when
/// the mutation hit a GraphQL-level error, so we have to peek at the
/// response shape ourselves and surface `errors[]` as a `GhFailure`.
fn graphql_mutation(gh: &dyn GhClient, mutation: &str) -> Result<serde_json::Value, GhError> {
    let stdout = gh.run(&["api", "graphql", "-f", &format!("query={}", mutation)])?;
    let value = parse_json(&stdout, "graphql")?;
    if let Some(errors) = value.get("errors").and_then(|v| v.as_array()) {
        if !errors.is_empty() {
            let summary = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::GhFailure(format!("graphql error: {}", summary)));
        }
    }
    Ok(value)
}

fn parse_json(stdout: &str, path: &str) -> Result<serde_json::Value, GhError> {
    serde_json::from_str(stdout).map_err(|e| GhError::ParseError(format!("{}: {}", path, e)))
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
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
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

    #[derive(Clone, Debug)]
    struct Call {
        args: Vec<String>,
        stdin: Option<String>,
    }

    struct FakeGh {
        responses: Mutex<Vec<Result<String, GhError>>>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeGh {
        fn with(mut responses: Vec<Result<String, GhError>>) -> Self {
            responses.reverse(); // serve in call order
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GhClient for FakeGh {
        fn run(&self, args: &[&str]) -> Result<String, GhError> {
            self.calls.lock().unwrap().push(Call {
                args: args.iter().map(|s| s.to_string()).collect(),
                stdin: None,
            });
            self.responses
                .lock()
                .unwrap()
                .pop()
                .expect("FakeGh: no more queued responses")
        }

        fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Result<String, GhError> {
            self.calls.lock().unwrap().push(Call {
                args: args.iter().map(|s| s.to_string()).collect(),
                stdin: Some(stdin.to_string()),
            });
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
            title: String::new(),
            body: String::new(),
            state: String::new(),
            is_draft: false,
            mergeable: String::new(),
            review_decision: String::new(),
            checks: Vec::new(),
            author: Default::default(),
            comments: Vec::new(),
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
        let store = GithubStore::new(pr(), Box::new(gh), true).unwrap();
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
        let store = GithubStore::new(pr(), Box::new(gh), true).unwrap();
        let all = store.list().unwrap();
        match &all[0].target {
            AnnotationTarget::LineRange {
                panel,
                start_line,
                end_line,
            } => {
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
        let store = GithubStore::new(pr(), Box::new(gh), true).unwrap();
        let only_readme = store.list_for_files(&["README.md".to_string()]).unwrap();
        assert_eq!(only_readme.len(), 1);
        assert_eq!(only_readme[0].filename, "README.md");
    }

    #[test]
    fn all_mutators_return_read_only_when_drifted() {
        // writable=false simulates "local HEAD != PR head" — the store
        // must refuse every mutation regardless of origin checks.
        let gh = FakeGh::with(vec![Ok(two_thread_payload().to_string())]);
        let mut store = GithubStore::new(pr(), Box::new(gh), false).unwrap();
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
        let mut store = GithubStore::new(pr(), Box::new(gh), true).unwrap();
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

    // ---- Phase 2 mutation tests ----

    /// Refetch payload returned after a `create` so the store can locate
    /// the new thread in the cache. Comment databaseId is parameterised so
    /// each test can stage its own freshly-created comment id.
    fn refetch_payload_with_new_comment(new_comment_id: i64) -> String {
        format!(
            r#"{{
              "data": {{ "repository": {{ "pullRequest": {{ "reviewThreads": {{ "nodes": [
                {{
                  "id": "PRRT_NEW",
                  "isResolved": false,
                  "subjectType": "LINE",
                  "path": "src/main.rs",
                  "line": 42,
                  "startLine": null,
                  "originalLine": 42,
                  "originalStartLine": null,
                  "diffSide": "RIGHT",
                  "comments": {{ "nodes": [
                    {{
                      "databaseId": {new_comment_id},
                      "body": "hello",
                      "createdAt": "2024-02-01T00:00:00Z",
                      "author": {{ "login": "me" }}
                    }}
                  ] }}
                }}
              ] }} }} }} }}
            }}"#
        )
    }

    fn line_range_draft() -> NewAnnotation {
        NewAnnotation {
            filename: "src/main.rs".to_string(),
            target: AnnotationTarget::LineRange {
                panel: DiffPanelFocus::New,
                start_line: 40,
                end_line: 42,
            },
            content: "hello".to_string(),
            origin: "ui".to_string(),
            created_at: UNIX_EPOCH,
        }
    }

    fn file_draft() -> NewAnnotation {
        NewAnnotation {
            filename: "README.md".to_string(),
            target: AnnotationTarget::File,
            content: "thoughts".to_string(),
            origin: "ui".to_string(),
            created_at: UNIX_EPOCH,
        }
    }

    #[test]
    fn create_line_range_posts_correct_body_and_locates_new_thread() {
        // Two responses: (1) POST returns the new comment with id=999,
        // (2) refetch returns a thread containing that comment.
        let post_response = r#"{ "id": 999, "body": "hello" }"#.to_string();
        let gh = FakeGh::with(vec![
            Ok(two_thread_payload().to_string()), // initial fetch in `new()`
            Ok(post_response.clone()),            // POST .../comments
            Ok(refetch_payload_with_new_comment(999)), // refetch after POST
        ]);
        // FakeGh is moved into the store; reach back into it via a raw ptr
        // would be unsafe — clone the call recorder by re-borrowing the
        // store's `gh` through a downcast we don't have. Simpler: hold an
        // Arc<FakeGh> and pass an Arc clone into the store.
        let gh_arc = std::sync::Arc::new(gh);
        let gh_for_store: Box<dyn GhClient> = Box::new(ArcGh(gh_arc.clone()));
        let mut store = GithubStore::new(pr(), gh_for_store, true).unwrap();

        let created = store
            .create(line_range_draft())
            .expect("create should succeed");
        assert_eq!(created.id.origin, "github");
        assert_eq!(created.id.native, "PRRT_NEW");
        assert_eq!(created.filename, "src/main.rs");

        // Inspect what we sent.
        let calls = gh_arc.calls();
        // 0 = initial GraphQL fetch, 1 = POST, 2 = refetch.
        assert_eq!(calls.len(), 3);
        let post_args = &calls[1].args;
        assert_eq!(post_args[0], "api");
        assert_eq!(post_args[1], "-X");
        assert_eq!(post_args[2], "POST");
        assert_eq!(post_args[3], "repos/o/r/pulls/7/comments");
        let body = serde_json::from_str::<serde_json::Value>(
            calls[1]
                .stdin
                .as_ref()
                .expect("POST should pipe body via stdin"),
        )
        .unwrap();
        assert_eq!(body["commit_id"], "b"); // pr().head_oid
        assert_eq!(body["path"], "src/main.rs");
        assert_eq!(body["body"], "hello");
        assert_eq!(body["side"], "RIGHT");
        assert_eq!(body["line"], 42);
        assert_eq!(body["start_side"], "RIGHT");
        assert_eq!(body["start_line"], 40);
        assert!(body.get("subject_type").is_none());
    }

    #[test]
    fn create_file_target_sends_subject_type_file() {
        let gh_arc = std::sync::Arc::new(FakeGh::with(vec![
            Ok(two_thread_payload().to_string()),
            Ok(r#"{ "id": 1234, "body": "thoughts" }"#.to_string()),
            Ok(refetch_payload_with_new_comment(1234)),
        ]));
        let mut store = GithubStore::new(pr(), Box::new(ArcGh(gh_arc.clone())), true).unwrap();

        store.create(file_draft()).unwrap();

        let body =
            serde_json::from_str::<serde_json::Value>(gh_arc.calls()[1].stdin.as_ref().unwrap())
                .unwrap();
        assert_eq!(body["subject_type"], "file");
        // File-target comments don't carry line/side fields.
        assert!(body.get("line").is_none());
        assert!(body.get("side").is_none());
        assert!(body.get("start_line").is_none());
    }

    #[test]
    fn update_patches_root_comment_body_and_updates_cache() {
        let gh_arc = std::sync::Arc::new(FakeGh::with(vec![
            Ok(two_thread_payload().to_string()),
            Ok(r#"{ "id": 1001, "body": "updated" }"#.to_string()),
        ]));
        let mut store = GithubStore::new(pr(), Box::new(ArcGh(gh_arc.clone())), true).unwrap();

        let id = AnnotationId {
            origin: "github".to_string(),
            native: "PRRT_1".to_string(),
        };
        store.update(&id, "updated".to_string()).unwrap();

        let calls = gh_arc.calls();
        let patch = &calls[1];
        assert_eq!(patch.args[0..3], ["api", "-X", "PATCH"]);
        assert_eq!(patch.args[3], "repos/o/r/pulls/comments/1001");
        let body =
            serde_json::from_str::<serde_json::Value>(patch.stdin.as_ref().unwrap()).unwrap();
        assert_eq!(body["body"], "updated");

        // Cache was patched in place — no refetch needed for update.
        let refreshed = store.list().unwrap();
        let ann = refreshed.iter().find(|a| a.id.native == "PRRT_1").unwrap();
        assert_eq!(ann.root().content, "updated");
    }

    #[test]
    fn reply_posts_to_replies_endpoint_and_refreshes_thread() {
        // After replying, refetch returns the same thread with a 3rd comment.
        let refetch_with_reply = r#"{
          "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [
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
              "comments": { "nodes": [
                { "databaseId": 1001, "body": "nit: rename", "createdAt": "2024-01-15T12:34:56Z", "author": { "login": "alice" } },
                { "databaseId": 1002, "body": "sure", "createdAt": "2024-01-15T12:40:00Z", "author": { "login": "bob" } },
                { "databaseId": 1003, "body": "ack", "createdAt": "2024-01-15T13:00:00Z", "author": { "login": "me" } }
              ] }
            }
          ] } } } }
        }"#;
        let gh_arc = std::sync::Arc::new(FakeGh::with(vec![
            Ok(two_thread_payload().to_string()),
            Ok(r#"{ "id": 1003 }"#.to_string()), // POST reply response
            Ok(refetch_with_reply.to_string()),
        ]));
        let mut store = GithubStore::new(pr(), Box::new(ArcGh(gh_arc.clone())), true).unwrap();

        let id = AnnotationId {
            origin: "github".to_string(),
            native: "PRRT_1".to_string(),
        };
        let updated = store
            .reply(
                &id,
                NewReply {
                    content: "ack".to_string(),
                    origin: "github".to_string(),
                    created_at: UNIX_EPOCH,
                },
            )
            .unwrap();

        let calls = gh_arc.calls();
        // POST args target the root comment's replies endpoint.
        assert_eq!(calls[1].args[3], "repos/o/r/pulls/7/comments/1001/replies");
        assert_eq!(updated.comments.len(), 3);
        assert_eq!(updated.comments[2].content, "ack");
    }

    #[test]
    fn set_resolved_true_calls_graphql_resolve_mutation() {
        let gh_arc = std::sync::Arc::new(FakeGh::with(vec![
            Ok(two_thread_payload().to_string()),
            Ok(
                r#"{ "data": { "resolveReviewThread": { "thread": { "id": "PRRT_1" } } } }"#
                    .to_string(),
            ),
        ]));
        let mut store = GithubStore::new(pr(), Box::new(ArcGh(gh_arc.clone())), true).unwrap();
        let id = AnnotationId {
            origin: "github".to_string(),
            native: "PRRT_1".to_string(),
        };
        store.set_resolved(&id, true).unwrap();

        let calls = gh_arc.calls();
        assert_eq!(calls[1].args[0..2], ["api", "graphql"]);
        // The query is embedded in the second arg as `query=mutation { ... }`.
        let query_field = &calls[1].args[3];
        assert!(query_field.contains("resolveReviewThread"));
        assert!(query_field.contains("PRRT_1"));

        let cached = store.list().unwrap();
        assert!(
            cached
                .iter()
                .find(|a| a.id.native == "PRRT_1")
                .unwrap()
                .resolved
        );
    }

    #[test]
    fn set_resolved_false_calls_unresolve_mutation() {
        let gh_arc = std::sync::Arc::new(FakeGh::with(vec![
            Ok(two_thread_payload().to_string()),
            Ok(
                r#"{ "data": { "unresolveReviewThread": { "thread": { "id": "PRRT_2" } } } }"#
                    .to_string(),
            ),
        ]));
        let mut store = GithubStore::new(pr(), Box::new(ArcGh(gh_arc.clone())), true).unwrap();
        let id = AnnotationId {
            origin: "github".to_string(),
            native: "PRRT_2".to_string(),
        };
        store.set_resolved(&id, false).unwrap();
        assert!(gh_arc.calls()[1].args[3].contains("unresolveReviewThread"));
        assert!(!store.list().unwrap()[1].resolved);
    }

    #[test]
    fn graphql_errors_surface_as_backend_error() {
        let gh_arc = std::sync::Arc::new(FakeGh::with(vec![
            Ok(two_thread_payload().to_string()),
            Ok(
                r#"{ "data": null, "errors": [ { "message": "Thread is outdated" } ] }"#
                    .to_string(),
            ),
        ]));
        let mut store = GithubStore::new(pr(), Box::new(ArcGh(gh_arc.clone())), true).unwrap();
        let id = AnnotationId {
            origin: "github".to_string(),
            native: "PRRT_1".to_string(),
        };
        match store.set_resolved(&id, true) {
            Err(AnnotationError::Backend(msg)) => assert!(msg.contains("Thread is outdated")),
            other => panic!("expected Backend(graphql ...), got {:?}", other),
        }
    }

    #[test]
    fn delete_calls_rest_delete_and_drops_from_cache() {
        let gh_arc = std::sync::Arc::new(FakeGh::with(vec![
            Ok(two_thread_payload().to_string()),
            Ok(String::new()), // DELETE returns 204 / empty body
        ]));
        let mut store = GithubStore::new(pr(), Box::new(ArcGh(gh_arc.clone())), true).unwrap();
        let id = AnnotationId {
            origin: "github".to_string(),
            native: "PRRT_1".to_string(),
        };
        store.delete(&id).unwrap();

        let calls = gh_arc.calls();
        assert_eq!(calls[1].args[0..3], ["api", "-X", "DELETE"]);
        assert_eq!(calls[1].args[3], "repos/o/r/pulls/comments/1001");
        assert!(calls[1].stdin.is_none());

        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id.native, "PRRT_2");
    }

    /// `Arc<FakeGh>` wrapper so tests can keep one handle to inspect calls
    /// while another handle (boxed) is moved into the `GithubStore`. Both
    /// share the same recorder.
    struct ArcGh(std::sync::Arc<FakeGh>);
    impl GhClient for ArcGh {
        fn run(&self, args: &[&str]) -> Result<String, GhError> {
            self.0.run(args)
        }
        fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Result<String, GhError> {
            self.0.run_with_stdin(args, stdin)
        }
    }
}
