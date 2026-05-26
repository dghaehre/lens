// ARCHIVED: GitHub / `gh` runtime integration.
//
// This file is intentionally NOT registered in `mod.rs` and is not compiled.
// It preserves the GitHub PR review code that was removed when lens moved
// to a "diff worktree against <ref>" model. Kept here so the integration
// can be revived later without digging through git history.
//
// To revive:
//   1. Add `pub mod _archived_github;` to `src/command/diff/mod.rs`
//      (rename the module on the way in).
//   2. Re-add the runtime deps to Cargo.toml:
//        spinoff = { version = "0.8.0", features = ["dots"] }
//        sha2 = "0.10"
//   3. Re-add `--pr` and `--origin` flags to `Cli::Diff` in `src/config/cli.rs`
//      and the matching fields on `DiffOptions` in `command/diff/mod.rs`.
//   4. Re-introduce the PR branches in `command/diff/app.rs`
//      (the old version is reachable via `git log` on this file).
//
// Runtime requirement: the `gh` CLI authenticated against GitHub.

use std::collections::HashSet;
use std::process::Command;
use std::thread;

#[derive(Clone)]
pub struct PrInfo {
    pub number: u64,
    pub node_id: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub base_ref: String,
    pub head_ref: String,
    pub base_repo_owner: String,
    pub head_repo_owner: Option<String>, // None if head repo was deleted (fork deleted)
}

pub fn parse_pr_input(input: &str) -> Option<(Option<String>, Option<String>, u64)> {
    // Try to parse as a URL first
    if input.starts_with("http://") || input.starts_with("https://") {
        // Extract PR number and repo info from URL
        // Format: https://github.com/owner/repo/pull/123
        let parts: Vec<&str> = input.trim_end_matches('/').split('/').collect();
        if parts.len() >= 2 {
            if let Some(pos) = parts.iter().position(|&p| p == "pull") {
                if pos + 1 < parts.len() {
                    if let Ok(num) = parts[pos + 1].parse::<u64>() {
                        // Extract owner and repo
                        if pos >= 2 {
                            let owner = parts[pos - 2].to_string();
                            let repo = parts[pos - 1].to_string();
                            return Some((Some(owner), Some(repo), num));
                        }
                        return Some((None, None, num));
                    }
                }
            }
        }
        None
    } else {
        // Try to parse as a PR number
        input.parse::<u64>().ok().map(|num| (None, None, num))
    }
}

pub fn resolve_origin_repo() -> Result<String, String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .map_err(|e| format!("Failed to run git: {}", e))?;
    if !output.status.success() {
        return Err(
            "Could not determine repository. Set origin remote or use --origin owner/repo"
                .to_string(),
        );
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let url = url.strip_suffix(".git").unwrap_or(&url);
    let path = url
        .split("github.com")
        .nth(1)
        .ok_or_else(|| format!("Origin URL is not a GitHub URL: {}", url))?;
    let path = path.trim_start_matches(':').trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 2 {
        Ok(format!("{}/{}", parts[0], parts[1]))
    } else {
        Err(format!(
            "Could not parse owner/repo from origin URL: {}",
            url
        ))
    }
}

pub fn fetch_pr_info(pr_input: &str, repo_override: Option<&str>) -> Result<PrInfo, String> {
    let (owner, repo, number) = parse_pr_input(pr_input).ok_or_else(|| {
        format!(
            "Invalid PR reference: {}. Use a PR number or URL.",
            pr_input
        )
    })?;

    let repo_full = match (&owner, &repo, repo_override) {
        (Some(o), Some(r), _) => format!("{}/{}", o, r),
        (_, _, Some(r)) => r.to_string(),
        _ => resolve_origin_repo()?,
    };

    let (repo_owner, repo_name) = {
        let parts: Vec<&str> = repo_full.split('/').collect();
        if parts.len() != 2 {
            return Err(format!("Invalid repo format: {}", repo_full));
        }
        (
            owner.unwrap_or_else(|| parts[0].to_string()),
            repo.unwrap_or_else(|| parts[1].to_string()),
        )
    };

    // Use GraphQL to get the PR node ID, branch refs, and repo owners
    let query = format!(
        r#"query {{ repository(owner: "{}", name: "{}") {{ pullRequest(number: {}) {{ id url baseRefName headRefName baseRepository {{ owner {{ login }} }} headRepository {{ owner {{ login }} }} }} }} }}"#,
        repo_owner, repo_name, number
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", query)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh api graphql failed: {}", stderr.trim()));
    }

    let json_str = String::from_utf8_lossy(&output.stdout);

    // Parse the GraphQL response
    let node_id = extract_json_string(&json_str, "id")
        .ok_or_else(|| "Could not parse PR node ID from GraphQL response".to_string())?;
    let base_ref =
        extract_json_string(&json_str, "baseRefName").unwrap_or_else(|| "base".to_string());
    let head_ref =
        extract_json_string(&json_str, "headRefName").unwrap_or_else(|| "head".to_string());

    // Extract repo owners from nested structure
    let base_repo_owner =
        extract_nested_login(&json_str, "baseRepository").unwrap_or_else(|| repo_owner.clone());
    let head_repo_owner = extract_nested_login(&json_str, "headRepository");

    Ok(PrInfo {
        number,
        node_id,
        repo_owner,
        repo_name,
        base_ref,
        head_ref,
        base_repo_owner,
        head_repo_owner,
    })
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    if let Some(start) = json.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = json[value_start..].find('"') {
            return Some(json[value_start..value_start + end].to_string());
        }
    }
    None
}

fn extract_nested_login(json: &str, parent_key: &str) -> Option<String> {
    // Look for pattern like "baseRepository":{"owner":{"login":"username"}}
    // or handle null case like "headRepository":null
    let pattern = format!("\"{}\":", parent_key);
    if let Some(start) = json.find(&pattern) {
        let after_key = &json[start + pattern.len()..];
        // Check if it's null
        if after_key.trim_start().starts_with("null") {
            return None;
        }
        // Look for login within this section
        if let Some(login_start) = after_key.find("\"login\":\"") {
            let value_start = login_start + 9;
            let after_login = &after_key[value_start..];
            if let Some(end) = after_login.find('"') {
                return Some(after_login[..end].to_string());
            }
        }
    }
    None
}

/// Fetch the list of files that are marked as viewed on GitHub
pub fn fetch_viewed_files(pr_info: &PrInfo) -> Result<HashSet<String>, String> {
    let query = format!(
        r#"query {{ repository(owner: "{}", name: "{}") {{ pullRequest(number: {}) {{ files(first: 100) {{ nodes {{ path viewerViewedState }} }} }} }} }}"#,
        pr_info.repo_owner, pr_info.repo_name, pr_info.number
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", query)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh api graphql failed: {}", stderr.trim()));
    }

    let json_str = String::from_utf8_lossy(&output.stdout);

    // Parse the response to find viewed files
    // Look for patterns like: "path":"filename","viewerViewedState":"VIEWED"
    let mut viewed_files = HashSet::new();

    // Simple parsing: find all path/viewerViewedState pairs
    let mut remaining = json_str.as_ref();
    while let Some(path_start) = remaining.find("\"path\":\"") {
        let path_value_start = path_start + 8;
        let after_path = &remaining[path_value_start..];
        if let Some(path_end) = after_path.find('"') {
            let path = &after_path[..path_end];

            // Look for viewerViewedState after this path
            let after_path_str = &after_path[path_end..];
            if let Some(state_start) = after_path_str.find("\"viewerViewedState\":\"") {
                let state_value_start = state_start + 21;
                let after_state = &after_path_str[state_value_start..];
                if let Some(state_end) = after_state.find('"') {
                    let state = &after_state[..state_end];
                    if state == "VIEWED" {
                        viewed_files.insert(path.to_string());
                    }
                }
            }

            remaining = &remaining[path_value_start + path_end..];
        } else {
            break;
        }
    }

    Ok(viewed_files)
}

/// Mark a file as viewed on GitHub PR (non-blocking, spawns a thread)
pub fn mark_file_as_viewed_async(pr_info: &PrInfo, file_path: &str) {
    let node_id = pr_info.node_id.clone();
    let path = file_path.to_string();

    thread::spawn(move || {
        let _ = mark_file_as_viewed_sync(&node_id, &path);
    });
}

/// Unmark a file as viewed on GitHub PR (non-blocking, spawns a thread)
pub fn unmark_file_as_viewed_async(pr_info: &PrInfo, file_path: &str) {
    let node_id = pr_info.node_id.clone();
    let path = file_path.to_string();

    thread::spawn(move || {
        let _ = unmark_file_as_viewed_sync(&node_id, &path);
    });
}

/// Mark a file as viewed on GitHub PR (blocking)
fn mark_file_as_viewed_sync(node_id: &str, file_path: &str) -> Result<(), String> {
    let mutation = format!(
        r#"mutation {{ markFileAsViewed(input: {{ pullRequestId: "{}", path: "{}" }}) {{ clientMutationId }} }}"#,
        node_id, file_path
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", mutation)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }

    Ok(())
}

/// Unmark a file as viewed on GitHub PR (blocking)
fn unmark_file_as_viewed_sync(node_id: &str, file_path: &str) -> Result<(), String> {
    let mutation = format!(
        r#"mutation {{ unmarkFileAsViewed(input: {{ pullRequestId: "{}", path: "{}" }}) {{ clientMutationId }} }}"#,
        node_id, file_path
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", mutation)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }

    Ok(())
}

// ---- loader-side helpers (previously in command/diff/loader.rs) ----

/// Load file diffs for a PR by fetching base/head file content from GitHub.
/// Uses `gh pr diff` to discover changed files and `gh api` for raw content.
pub fn load_pr_file_diffs_archived(_pr_info: &PrInfo) -> Result<(), String> {
    // Pseudocode preserved for revival; the original implementation was:
    //
    //   let repo_arg = format!("{}/{}", pr_info.repo_owner, pr_info.repo_name);
    //   let output = Command::new("gh").args(["pr", "diff", &pr_info.number.to_string(),
    //       "--repo", &repo_arg]).output()?;
    //   let diff = String::from_utf8_lossy(&output.stdout);
    //   let changed_files = parse_changed_files_from_diff(&diff);
    //   for filename in changed_files {
    //       let old = fetch_file_content_from_github(&base_repo, &pr_info.base_ref, &filename);
    //       let new = fetch_file_content_from_github(&head_repo, &pr_info.head_ref, &filename);
    //       /* build FileDiff */
    //   }
    //
    // Helpers below.
    Ok(())
}

pub fn fetch_file_content_from_github(repo: &str, git_ref: &str, path: &str) -> String {
    let api_path = format!("repos/{}/contents/{}?ref={}", repo, path, git_ref);
    let output = Command::new("gh")
        .args([
            "api",
            &api_path,
            "-H",
            "Accept: application/vnd.github.raw+json",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    }
}

pub fn parse_changed_files_from_diff(diff: &str) -> Vec<String> {
    let mut files = Vec::new();

    for line in diff.lines() {
        if line.starts_with("diff --git") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let b_path = parts[3];
                if let Some(filename) = b_path.strip_prefix("b/") {
                    files.push(filename.to_string());
                } else {
                    files.push(b_path.to_string());
                }
            }
        }
    }

    files
}

// ---- app-side helpers (previously in command/diff/app.rs) ----
//
// Open a file URL using the system browser. Used for the `o` keybinding
// (open current file in the PR on github.com).
pub fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd").args(["/C", "start", url]).spawn()?;
    }
    Ok(())
}

/// Build the SHA-256 file anchor used by GitHub's PR file view (#diff-<hash>).
pub fn generate_file_anchor(filename: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(filename.as_bytes());
    format!("{:x}", hasher.finalize())
}
