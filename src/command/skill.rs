//! `lens skill install <provider>` — drop the bundled SKILL.md somewhere the
//! agent for the chosen provider will pick it up.
//!
//! Destinations:
//! - claude → `~/.claude/skills/lens-annotations/SKILL.md`
//! - codex  → `~/.codex/skills/lens-annotations/SKILL.md`

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
    let dir = home.join(".codex").join("skills").join("lens-annotations");
    let target = dir.join("SKILL.md");
    fs::create_dir_all(&dir)?;
    fs::write(&target, SKILL_MD)?;
    cleanup_legacy_codex_agents(home)?;
    println!("installed lens skill for codex → {}", target.display());
    Ok(())
}

/// Remove the old managed `lens-skill` block from Codex's AGENTS.md when users
/// reinstall with a newer Lens. User-owned content outside the block stays in
/// place; an AGENTS.md that only contained the managed block is removed.
fn cleanup_legacy_codex_agents(home: &Path) -> Result<(), LensError> {
    let target = home.join(".codex").join("AGENTS.md");
    let existing = match fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let Some(cleaned) = remove_legacy_codex_block(&existing) else {
        return Ok(());
    };

    if cleaned.trim().is_empty() {
        fs::remove_file(&target)?;
    } else {
        fs::write(&target, cleaned)?;
    }
    Ok(())
}

fn remove_legacy_codex_block(existing: &str) -> Option<String> {
    let start = existing.find(CODEX_BLOCK_START)?;
    let end_marker = existing[start..].find(CODEX_BLOCK_END)?;
    let end = start + end_marker + CODEX_BLOCK_END.len();

    let before = existing[..start].trim_end_matches('\n');
    let after = existing[end..].trim_start_matches('\n');
    let mut out = String::new();

    if !before.is_empty() {
        out.push_str(before);
        out.push('\n');
    }
    if !before.is_empty() && !after.is_empty() {
        out.push('\n');
    }
    out.push_str(after);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_legacy_codex_block_preserves_user_content() {
        let existing = format!(
            "# AGENTS\n\nintro text\n\n{start}\nold body\n{end}\n\ntrailing\n",
            start = CODEX_BLOCK_START,
            end = CODEX_BLOCK_END,
        );
        let out = remove_legacy_codex_block(&existing).expect("block present");
        assert!(!out.contains("old body"));
        assert!(out.contains("trailing"));
        assert!(out.contains("intro text"));
        assert_eq!(out, "# AGENTS\n\nintro text\n\ntrailing\n");
    }

    #[test]
    fn remove_legacy_codex_block_returns_none_when_missing() {
        assert!(remove_legacy_codex_block("# AGENTS\n\nno block here\n").is_none());
    }

    #[test]
    fn install_codex_writes_skill_file_without_agents() {
        let home = tempfile::tempdir().expect("tempdir");
        install_codex(home.path()).expect("install codex");

        let skill = home
            .path()
            .join(".codex")
            .join("skills")
            .join("lens-annotations")
            .join("SKILL.md");
        assert_eq!(fs::read_to_string(skill).expect("skill file"), SKILL_MD);
        assert!(!home.path().join(".codex").join("AGENTS.md").exists());
    }

    #[test]
    fn install_codex_removes_legacy_agents_file_when_only_managed_block() {
        let home = tempfile::tempdir().expect("tempdir");
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).expect("codex dir");
        fs::write(
            codex_dir.join("AGENTS.md"),
            format!(
                "{start}\nold body\n{end}\n",
                start = CODEX_BLOCK_START,
                end = CODEX_BLOCK_END,
            ),
        )
        .expect("legacy agents");

        install_codex(home.path()).expect("install codex");

        assert!(!codex_dir.join("AGENTS.md").exists());
    }
}
