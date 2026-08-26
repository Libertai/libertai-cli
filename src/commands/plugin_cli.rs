//! `libertai plugin …` — cooked-mode management for Claude-Code-compatible
//! plugins. The interactive scan/trust gate lives here (natural `dialoguer`
//! prompts) rather than in the TUI event loop; the REPL carries only a
//! read-only `/plugin list`. All real work is delegated to the
//! [`crate::commands::code_plugins`] engine.

use anyhow::{bail, Result};

use crate::cli::{MarketplaceAction, PluginAction};
use crate::commands::code_plugin_sign;
use crate::commands::code_plugins::{self, StagedPlugin};
use crate::config;

pub fn run(action: PluginAction) -> Result<()> {
    let mut cfg = config::load()?;
    match action {
        PluginAction::List => {
            list_installed(&cfg);
            Ok(())
        }
        PluginAction::Marketplace { action } => marketplace(&mut cfg, action),
        PluginAction::Install {
            name,
            scan,
            no_scan,
            trust,
            yes,
        } => install(&mut cfg, &name, scan, no_scan, trust, yes),
        PluginAction::Audit { name } => audit(&cfg, &name),
        PluginAction::Enable { name } => set_enabled(&mut cfg, &name, true),
        PluginAction::Disable { name } => set_enabled(&mut cfg, &name, false),
        PluginAction::Remove { name } => remove(&mut cfg, &name),
        PluginAction::Sign { path, key } => sign(&path, key),
    }
}

/// Sign a plugin directory in place with the author's wallet key.
fn sign(path: &str, key: Option<String>) -> Result<()> {
    let root = std::path::Path::new(path);
    anyhow::ensure!(root.is_dir(), "not a directory: {path}");
    let key_hex = key
        .or_else(|| std::env::var("LIBERTAI_SIGNING_KEY").ok())
        .ok_or_else(|| {
            anyhow::anyhow!("no signing key — pass --key <hex> or set LIBERTAI_SIGNING_KEY")
        })?;
    let sk = crate::auth::wallet::signing_key_from_hex(&key_hex)?;
    let file = code_plugin_sign::sign_plugin(root, &sk)?;
    eprintln!("Signed {path}");
    eprintln!("  address: {}", file.address);
    eprintln!("  digest:  {}", file.digest);
    eprintln!("Wrote {}", code_plugin_sign::SIGNATURE_REL);
    Ok(())
}

fn marketplace(cfg: &mut config::Config, action: MarketplaceAction) -> Result<()> {
    match action {
        MarketplaceAction::Add { source } => {
            eprintln!("Adding marketplace from {source} …");
            let added = code_plugins::add_marketplace(cfg, &source)?;
            config::save(cfg)?;
            eprintln!(
                "Added marketplace `{}` ({} plugin(s)):",
                added.name,
                added.plugins.len()
            );
            for p in &added.plugins {
                let desc = p.description.as_deref().unwrap_or("");
                eprintln!("  - {}  {desc}", p.name);
            }
            eprintln!(
                "Install one with: libertai plugin install <name>@{}",
                added.name
            );
            Ok(())
        }
        MarketplaceAction::List => {
            if cfg.plugins.marketplaces.is_empty() {
                eprintln!("No marketplaces added. Add one with: libertai plugin marketplace add <git-url|path>");
            }
            for (name, m) in &cfg.plugins.marketplaces {
                eprintln!("{name}\t{}", m.source);
            }
            Ok(())
        }
        MarketplaceAction::Remove { name } => {
            code_plugins::remove_marketplace(cfg, &name)?;
            config::save(cfg)?;
            eprintln!("Removed marketplace `{name}`.");
            Ok(())
        }
    }
}

fn install(
    cfg: &mut config::Config,
    name: &str,
    scan_flag: bool,
    no_scan: bool,
    trust_flag: bool,
    yes: bool,
) -> Result<()> {
    let (marketplace, plugin) = resolve_install_target(cfg, name)?;
    eprintln!("Fetching {plugin} from {marketplace} …");
    let staged = code_plugins::stage_plugin(cfg, &marketplace, &plugin)?;
    print_capabilities(&staged);

    // Org policy: refuse unsigned/invalid plugins when require_signed is set.
    if cfg.plugins.require_signed && !staged.signature.is_valid() {
        bail!(
            "plugins.require_signed is set but {plugin}@{marketplace} is {} — refusing to install",
            staged.signature.label()
        );
    }

    if should_scan(cfg, scan_flag, no_scan, yes)? {
        run_scan(cfg, &staged);
    }

    let trusted = resolve_trust(&staged, trust_flag, yes)?;
    if !yes && !confirm(&format!("Install {plugin}@{marketplace}?"), true)? {
        eprintln!("Aborted — files left staged; re-run to resume.");
        return Ok(());
    }

    code_plugins::finalize_install(cfg, &staged, trusted);
    config::save(cfg)?;
    eprintln!(
        "Installed {plugin}@{marketplace} (enabled{}).",
        if trusted { ", trusted" } else { "" }
    );
    if staged.capabilities.runs_code() && !trusted {
        eprintln!(
            "Note: its hooks/MCP stay inert until you trust it (libertai plugin ... --trust)."
        );
    }
    Ok(())
}

fn audit(cfg: &config::Config, name: &str) -> Result<()> {
    let (marketplace, plugin) = resolve_install_target(cfg, name)?;
    eprintln!("Fetching {plugin} from {marketplace} for audit …");
    let staged = code_plugins::stage_plugin(cfg, &marketplace, &plugin)?;
    print_capabilities(&staged);
    run_scan(cfg, &staged);
    eprintln!("Audit only — nothing installed.");
    Ok(())
}

fn set_enabled(cfg: &mut config::Config, name: &str, enabled: bool) -> Result<()> {
    let key = resolve_installed_key(cfg, name)?;
    code_plugins::set_enabled(cfg, &key, enabled)?;
    config::save(cfg)?;
    eprintln!("{} {key}.", if enabled { "Enabled" } else { "Disabled" });
    Ok(())
}

fn remove(cfg: &mut config::Config, name: &str) -> Result<()> {
    let key = resolve_installed_key(cfg, name)?;
    code_plugins::uninstall(cfg, &key)?;
    config::save(cfg)?;
    eprintln!("Removed {key}.");
    Ok(())
}

fn list_installed(cfg: &config::Config) {
    if cfg.plugins.installed.is_empty() {
        eprintln!(
            "No plugins installed. Add a marketplace and install one with `libertai plugin`."
        );
        return;
    }
    let (h_name, h_en, h_tr, h_ver) = ("PLUGIN@MARKETPLACE", "ENABLED", "TRUSTED", "VERSION");
    eprintln!("{h_name:<28} {h_en:<8} {h_tr:<8} {h_ver}");
    for (key, p) in &cfg.plugins.installed {
        eprintln!(
            "{:<28} {:<8} {:<8} {}",
            key,
            yes_no(p.enabled),
            yes_no(p.trusted),
            p.version.as_deref().unwrap_or("-")
        );
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// Whether to run scanners: explicit flags win, else the configured policy
/// (`ask` prompts unless `--yes`).
fn should_scan(cfg: &config::Config, scan_flag: bool, no_scan: bool, yes: bool) -> Result<bool> {
    if scan_flag {
        return Ok(true);
    }
    if no_scan {
        return Ok(false);
    }
    Ok(match cfg.plugins.scan_on_install {
        config::ScanPolicy::Always => true,
        config::ScanPolicy::Never => false,
        config::ScanPolicy::Ask => {
            yes || confirm(
                "Run external security scan (e.g. skillspector) before installing?",
                true,
            )?
        }
    })
}

fn run_scan(cfg: &config::Config, staged: &StagedPlugin) {
    let results = code_plugins::run_applicable_scanners(cfg, &staged.path, &staged.capabilities);
    if results.is_empty() {
        eprintln!("No applicable scanners configured.");
        return;
    }
    for r in &results {
        if !r.ran {
            eprintln!("  {} (skipped): {}", r.scanner, r.summary);
        } else if r.passed {
            eprintln!("  {} ✓ no findings", r.scanner);
        } else {
            eprintln!("  {} ✗ findings:\n{}", r.scanner, indent(&r.summary));
        }
    }
}

/// Decide the trust flag (may the plugin's hooks/MCP run). Prompts only when
/// the plugin actually ships code and `--trust`/`--yes` weren't given.
fn resolve_trust(staged: &StagedPlugin, trust_flag: bool, yes: bool) -> Result<bool> {
    if trust_flag {
        return Ok(true);
    }
    if !staged.capabilities.runs_code() {
        return Ok(false);
    }
    if yes {
        return Ok(false); // non-interactive never auto-trusts code execution
    }
    confirm(
        "This plugin ships hooks/MCP servers that execute code. Trust it to run them?",
        false,
    )
}

fn print_capabilities(staged: &StagedPlugin) {
    let c = &staged.capabilities;
    eprintln!();
    eprintln!(
        "{}@{}  (format: {}, risk: {})",
        staged.name,
        staged.marketplace,
        staged.format.as_str(),
        c.risk().label()
    );
    if let Some(v) = &staged.version {
        eprintln!("  version: {v}");
    }
    if let Some(sha) = &staged.sha {
        eprintln!("  pinned:  {}", short_sha(sha));
    }
    eprintln!("  signature: {}", staged.signature.label());
    if !c.components.is_empty() {
        let parts: Vec<String> = c
            .components
            .iter()
            .map(|(k, n)| format!("{n} {k}"))
            .collect();
        eprintln!("  provides: {}", parts.join(", "));
    }
    if !c.hook_commands.is_empty() {
        eprintln!("  hooks that would run:");
        for cmd in &c.hook_commands {
            eprintln!("    $ {cmd}");
        }
    }
    if !c.mcp_servers.is_empty() {
        eprintln!("  MCP servers:");
        for s in &c.mcp_servers {
            eprintln!("    {s}");
        }
    }
    if !c.flags.is_empty() {
        eprintln!("  ⚠ danger flags:");
        for f in &c.flags {
            eprintln!("    ! {f}");
        }
    }
    eprintln!();
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn confirm(prompt: &str, default: bool) -> Result<bool> {
    Ok(dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

/// Resolve a possibly-bare plugin name to `(marketplace, plugin)` for install.
fn resolve_install_target(cfg: &config::Config, name: &str) -> Result<(String, String)> {
    if let Some((plugin, marketplace)) = name.split_once('@') {
        return Ok((marketplace.to_string(), plugin.to_string()));
    }
    let mut hits = Vec::new();
    for market in cfg.plugins.marketplaces.keys() {
        if let Ok(plugins) = code_plugins::marketplace_plugins(cfg, market) {
            if plugins.iter().any(|p| p.name == name) {
                hits.push(market.clone());
            }
        }
    }
    match hits.as_slice() {
        [] => bail!("plugin `{name}` not found in any added marketplace"),
        [market] => Ok((market.clone(), name.to_string())),
        _ => bail!("plugin `{name}` is in multiple marketplaces — qualify it as name@marketplace"),
    }
}

/// Resolve a possibly-bare name to an installed `"<plugin>@<marketplace>"` key.
fn resolve_installed_key(cfg: &config::Config, name: &str) -> Result<String> {
    if cfg.plugins.installed.contains_key(name) {
        return Ok(name.to_string());
    }
    let hits: Vec<String> = cfg
        .plugins
        .installed
        .keys()
        .filter(|k| k.split('@').next() == Some(name))
        .cloned()
        .collect();
    match hits.as_slice() {
        [] => bail!("plugin `{name}` is not installed"),
        [key] => Ok(key.clone()),
        _ => bail!("`{name}` matches multiple installs — qualify it as name@marketplace"),
    }
}
