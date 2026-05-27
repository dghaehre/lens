//! `lens skill install <provider>` — drop the bundled SKILL.md somewhere the
//! agent for the chosen provider will pick it up.
//!
//! Destinations:
//! - claude → `~/.claude/skills/lens-annotations/SKILL.md`
//! - codex  → `~/.codex/AGENTS.md` (idempotent block between markers, so
//!   re-running update in place without duplicating the content)

use std::fs;
use std::path::Path;

use dirs::home_dir;

use crate::config::cli::{SkillCommand, SkillInstallArgs, SkillProvider};
use crate::error::LensError;

/// SKILL.md is the single source of truth; bake it into the binary so
/// `lens skill install` works on machines without the repo checked out.
const SKILL_MD: &str = include_str!("../../SKILL.md");

const CODEX_BLOCK_START: &str = "<!-- lens-skill:start -->";
const CODEX_BLOCK_END: &str = "<!-- lens-skill:end -->";

pub fn run(command: SkillCommand) -> Result<(), LensError> {
    match command {
        SkillCommand::Install(args) => install(args),
    }
}

fn install(args: SkillInstallArgs) -> Result<(), LensError> {
    let home = home_dir().ok_or_else(|| {
        LensError::InvalidConfiguration("could not resolve home directory".to_string())
    })?;
    match args.provider {
        SkillProvider::Claude => install_claude(&home),
        SkillProvider::Codex => install_codex(&home),
    }
}

fn install_claude(home: &Path) -> Result<(), LensError> {
    let dir = home.join(".claude").join("skills").join("lens-annotations");
    let target = dir.join("SKILL.md");
    fs::create_dir_all(&dir)?;
    fs::write(&target, SKILL_MD)?;
    println!("installed lens skill for claude → {}", target.display());
    Ok(())
}

fn install_codex(home: &Path) -> Result<(), LensError> {
    let dir = home.join(".codex");
    let target = dir.join("AGENTS.md");
    fs::create_dir_all(&dir)?;

    let existing = match fs::read_to_string(&target) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };

    // Codex's AGENTS.md is a shared file the user may have other content in,
    // so we wrap our payload in HTML-comment markers and overwrite only the
    // text between them. This makes `lens skill install codex` idempotent.
    let block = format!(
        "{start}\n<!-- managed by `lens skill install codex` — re-run to update -->\n{body}\n{end}\n",
        start = CODEX_BLOCK_START,
        body = SKILL_MD.trim_end(),
        end = CODEX_BLOCK_END,
    );

    let new_contents = match existing {
        None => block,
        Some(prev) => match replace_block(&prev, &block) {
            Some(replaced) => replaced,
            None => {
                // No existing block. Preserve user content; append ours after a
                // blank line.
                let mut out = prev;
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&block);
                out
            }
        },
    };

    fs::write(&target, new_contents)?;
    println!("installed lens skill for codex → {}", target.display());
    Ok(())
}

/// Swap the managed `lens-skill` block inside an existing AGENTS.md for a
/// freshly-built one. The block is delimited by `CODEX_BLOCK_START` /
/// `CODEX_BLOCK_END` HTML comments so we can find and overwrite it without
/// touching anything else in the file. Returns `None` when no block is
/// present (caller falls back to appending).
fn replace_block(existing: &str, block: &str) -> Option<String> {
    let start = existing.find(CODEX_BLOCK_START)?;
    let end_marker = existing[start..].find(CODEX_BLOCK_END)?;
    let end = start + end_marker + CODEX_BLOCK_END.len();
    let mut out = String::with_capacity(existing.len() + block.len());
    out.push_str(&existing[..start]);
    out.push_str(block.trim_end());
    // Preserve a single trailing newline after the block.
    let tail = &existing[end..];
    let tail = tail.trim_start_matches('\n');
    out.push('\n');
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(tail);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_block_swaps_existing_block() {
        let existing = format!(
            "# AGENTS\n\nintro text\n\n{start}\nold body\n{end}\n\ntrailing\n",
            start = CODEX_BLOCK_START,
            end = CODEX_BLOCK_END,
        );
        let new_block = format!(
            "{start}\nnew body\n{end}\n",
            start = CODEX_BLOCK_START,
            end = CODEX_BLOCK_END,
        );
        let out = replace_block(&existing, &new_block).expect("block present");
        assert!(out.contains("new body"));
        assert!(!out.contains("old body"));
        assert!(out.contains("trailing"));
        assert!(out.contains("intro text"));
    }

    #[test]
    fn replace_block_returns_none_when_missing() {
        assert!(replace_block("# AGENTS\n\nno block here\n", "ignored").is_none());
    }
}
