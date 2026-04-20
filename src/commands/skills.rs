use anyhow::{anyhow, Context, Result};
use owo_colors::OwoColorize;
use std::path::{Path, PathBuf};

use crate::cli::SkillsAction;

struct BundledSkill {
    /// Directory name under `.claude/skills/`.
    name: &'static str,
    /// Rendered SKILL.md body (with YAML frontmatter).
    body: &'static str,
}

const BUNDLED: &[BundledSkill] = &[BundledSkill {
    name: "libertai-image",
    body: include_str!("../skills_content/libertai-image.md"),
}];

/// Host — used by both `libertai skills` and launchers.
#[derive(Clone, Copy)]
pub enum Host {
    /// `~/.claude/skills/<name>/SKILL.md`
    Claude,
}

pub fn run(action: SkillsAction) -> Result<()> {
    match action {
        SkillsAction::List => list(),
        SkillsAction::Install { project } => install(Host::Claude, project, true),
        SkillsAction::Uninstall { project } => uninstall(Host::Claude, project),
    }
}

pub fn list() -> Result<()> {
    for s in BUNDLED {
        println!("{}", s.name);
    }
    Ok(())
}

/// Install bundled skills into the given host's skill directory. If `force`
/// is false, existing files are left alone — use this from launchers for a
/// non-destructive first-run install. If `force` is true, every skill file is
/// overwritten (explicit refresh).
pub fn install(host: Host, project: bool, force: bool) -> Result<()> {
    let base = skills_base(host, project)?;
    for s in BUNDLED {
        let dir = base.join(s.name);
        let skill_path = dir.join("SKILL.md");
        if skill_path.exists() && !force {
            continue;
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&skill_path, s.body)
            .with_context(|| format!("writing {}", skill_path.display()))?;
        eprintln!(
            "  {} {}",
            "skill:".dimmed(),
            skill_path.display()
        );
    }
    Ok(())
}

pub fn uninstall(host: Host, project: bool) -> Result<()> {
    let base = skills_base(host, project)?;
    for s in BUNDLED {
        let dir = base.join(s.name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing {}", dir.display()))?;
            eprintln!("removed skill: {}", dir.display());
        }
    }
    Ok(())
}

/// Non-destructive install used by launchers: writes skill files only if they
/// don't already exist. Returns the number of skills newly installed.
pub fn install_if_missing(host: Host) -> Result<usize> {
    let base = skills_base(host, false)?;
    let mut installed = 0usize;
    for s in BUNDLED {
        let dir = base.join(s.name);
        let skill_path = dir.join("SKILL.md");
        if skill_path.exists() {
            continue;
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&skill_path, s.body)
            .with_context(|| format!("writing {}", skill_path.display()))?;
        installed += 1;
    }
    Ok(installed)
}

fn skills_base(host: Host, project: bool) -> Result<PathBuf> {
    let root: PathBuf = if project {
        std::env::current_dir().context("determining current working directory")?
    } else {
        dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?
    };
    Ok(match host {
        Host::Claude => root.join(dot_claude_path()),
    })
}

fn dot_claude_path() -> &'static Path {
    Path::new(".claude/skills")
}
