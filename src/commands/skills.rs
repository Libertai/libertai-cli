use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

use crate::cli::SkillsAction;
use crate::commands::output::Styler;

struct BundledSkill {
    /// Directory name under the host's skills dir.
    name: &'static str,
    /// Rendered SKILL.md body (with YAML frontmatter).
    body: &'static str,
}

const BUNDLED: &[BundledSkill] = &[
    BundledSkill {
        name: "libertai-image",
        body: include_str!("../skills_content/libertai-image.md"),
    },
    BundledSkill {
        name: "libertai-search",
        body: include_str!("../skills_content/libertai-search.md"),
    },
];

/// Host — used by both `libertai skills` and launchers.
#[derive(Clone, Copy)]
pub enum Host {
    /// `~/.claude/skills/<name>/SKILL.md`
    Claude,
    /// `$XDG_CONFIG_HOME/opencode/skills/<name>/SKILL.md` (else
    /// `~/.config/opencode/skills/`), or `.opencode/skills/` in project mode.
    OpenCode,
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
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&skill_path, s.body)
            .with_context(|| format!("writing {}", skill_path.display()))?;
        eprintln!(
            "  {} {}",
            Styler::stderr().dimmed("skill:"),
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
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
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
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&skill_path, s.body)
            .with_context(|| format!("writing {}", skill_path.display()))?;
        installed += 1;
    }
    Ok(installed)
}

fn skills_base(host: Host, project: bool) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("determining current working directory")?;
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Ok(skills_dir(
        host,
        project,
        &cwd,
        &home,
        xdg_config.as_deref(),
    ))
}

fn skills_dir(
    host: Host,
    project: bool,
    cwd: &Path,
    home: &Path,
    xdg_config: Option<&Path>,
) -> PathBuf {
    match (host, project) {
        (Host::Claude, true) => cwd.join(".claude/skills"),
        (Host::Claude, false) => home.join(".claude/skills"),
        (Host::OpenCode, true) => cwd.join(".opencode/skills"),
        (Host::OpenCode, false) => {
            // opencode resolves its config dir from $XDG_CONFIG_HOME (else
            // ~/.config) on every platform, including macOS.
            let base = xdg_config
                .filter(|p| p.is_absolute())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".config"));
            base.join("opencode").join("skills")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_global_skills_dir_is_home_dot_claude() {
        let dir = skills_dir(
            Host::Claude,
            false,
            Path::new("/cwd"),
            Path::new("/home/u"),
            None,
        );
        assert_eq!(dir, PathBuf::from("/home/u/.claude/skills"));
    }

    #[test]
    fn claude_project_skills_dir_is_cwd_dot_claude() {
        let dir = skills_dir(
            Host::Claude,
            true,
            Path::new("/cwd"),
            Path::new("/home/u"),
            None,
        );
        assert_eq!(dir, PathBuf::from("/cwd/.claude/skills"));
    }

    #[test]
    fn opencode_global_skills_dir_uses_xdg_config_home() {
        let dir = skills_dir(
            Host::OpenCode,
            false,
            Path::new("/cwd"),
            Path::new("/home/u"),
            Some(Path::new("/xdg")),
        );
        assert_eq!(dir, PathBuf::from("/xdg/opencode/skills"));
    }

    #[test]
    fn opencode_global_skills_dir_falls_back_to_home_config() {
        let dir = skills_dir(
            Host::OpenCode,
            false,
            Path::new("/cwd"),
            Path::new("/home/u"),
            None,
        );
        assert_eq!(dir, PathBuf::from("/home/u/.config/opencode/skills"));
    }

    #[test]
    fn opencode_global_skills_dir_ignores_relative_xdg() {
        let dir = skills_dir(
            Host::OpenCode,
            false,
            Path::new("/cwd"),
            Path::new("/home/u"),
            Some(Path::new("relative/xdg")),
        );
        assert_eq!(dir, PathBuf::from("/home/u/.config/opencode/skills"));
    }

    #[test]
    fn opencode_project_skills_dir_is_cwd_dot_opencode() {
        let dir = skills_dir(
            Host::OpenCode,
            true,
            Path::new("/cwd"),
            Path::new("/home/u"),
            None,
        );
        assert_eq!(dir, PathBuf::from("/cwd/.opencode/skills"));
    }
}
