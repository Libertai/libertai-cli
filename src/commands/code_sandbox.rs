//! Sandbox policy for the bash tool.
//!
//! Translates the user-facing `--sandbox <off|strict|auto>` choice into
//! an argv prefix that pi's `BashTool` will splice in front of the shell
//! invocation (see `pi_agent_rust/src/tools.rs::BashTool::command_wrapper`).
//!
//! Pi itself doesn't know what `bwrap`/`firejail`/`sandbox-exec` are; we
//! build the argv here so the policy lives in libertai-cli and the
//! sandbox primitive stays an opaque-argv contract.
//!
//! Architecture:
//!
//!   detect_strict_profile(cwd)
//!         │
//!         ▼
//!   StrictProfile  ◄── apply_policy_override(&policy)
//!         │
//!         ▼
//!   build_argv()  ───►  Vec<String>  ───►  BashTool::command_wrapper
//!
//! `StrictProfile` is the single data-shaped source of truth. The CLI's
//! `libertai sandbox info` prints it. The desktop's settings card lets
//! the user edit a `SandboxPolicy` override that this module applies
//! before converting to argv. Tests pin the shape so accidental
//! "I'm sandboxed but actually I'm not" regressions are caught.
//!
//! Today only Linux + `bwrap` is implemented. macOS (`sandbox-exec`) and
//! a Windows helper-binary route are deliberate follow-ups; the public
//! API of this module stays platform-independent so the desktop's
//! detection-row UI works on every OS.

use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Walk `$PATH` looking for `name`. Pure stdlib so we don't pull in a
/// `which`-style dependency for one call. Returns the full path on
/// match so callers (e.g. the desktop sandbox-status UI) can surface
/// *where* the binary lives, not just whether it exists. Reading
/// `$PATH` errors out to `None`.
#[must_use]
pub fn binary_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Whether the strict profile can actually run on this machine right
/// now. Lets callers (the desktop's chat-pillar policy, the CLI's
/// pre-flight bail) tell apart "asked for off" from "asked for strict
/// but the binary is missing".
#[must_use]
pub fn is_strict_supported() -> bool {
    cfg!(target_os = "linux") && binary_on_path("bwrap").is_some()
}

/// User-facing `--sandbox=<mode>` choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
#[clap(rename_all = "lower")]
pub enum SandboxMode {
    #[default]
    Off,
    Strict,
    Auto,
}

impl SandboxMode {
    /// Resolve `Auto` into a concrete `Off` or `Strict`. Caller passes
    /// `is_untrusted=true` for chat-style sessions.
    #[must_use]
    pub fn resolve(self, is_untrusted: bool) -> Self {
        match self {
            Self::Auto => {
                if is_untrusted {
                    Self::Strict
                } else {
                    Self::Off
                }
            }
            other => other,
        }
    }
}

/// What kind of path entry — drives how the argv builder uses it.
///
/// * `Bin`: read-only bind + the path gets prepended to `$PATH` inside
///   the sandbox so binaries there are exec'able by name.
/// * `Lib`: read-only bind only; loader resolves shared objects through
///   these, no PATH entry.
/// * `Config`: read-only bind of a small per-file path (e.g.
///   `/etc/passwd`). Treated as bind targets, not directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindKind {
    Bin,
    Lib,
    Config,
}

/// One filesystem path the strict profile considers exposing into the
/// sandbox. Carries enough state for `libertai sandbox info` to render
/// a `[present] /usr/bin` row, and for the desktop's checkbox UI to
/// reflect both "default disabled by user" and "user added a custom
/// path".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindEntry {
    /// Absolute filesystem path the bind targets.
    pub path: PathBuf,
    /// Bin / Lib / Config; controls whether PATH gets the entry too.
    pub kind: BindKind,
    /// Whether the file/dir actually exists on this host. `false`
    /// entries are still emitted as `--ro-bind-try` (a no-op when
    /// missing), so callers can keep them in policy without breaking
    /// the sandbox if the path is re-created later.
    pub present: bool,
    /// Whether this entry will be included in the next argv build.
    /// Defaults to `true` for auto-detected entries; the desktop
    /// settings UI flips it via policy override.
    pub enabled: bool,
    /// Came from `detect_strict_profile` (`Default`) or from a user-
    /// added entry in settings (`Custom`). Lets the UI render the two
    /// in separate columns and forbid removing defaults.
    pub source: BindSource,
}

/// Provenance of a [`BindEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindSource {
    Default,
    Custom,
}

/// Data-shaped strict-sandbox policy. Same struct is used to:
///   - Render `libertai sandbox info` output.
///   - Feed the desktop's Settings → Bash sandbox card.
///   - Build the actual bwrap argv.
///
/// Production code never constructs this by hand — go through
/// [`detect_strict_profile`] (host-defaults) and
/// [`apply_policy_override`] (user overrides).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrictProfile {
    /// All filesystem entries the sandbox would consider binding, in
    /// the order they'll be emitted into the argv.
    pub binds: Vec<BindEntry>,
    /// Read-write bind for the session's working directory. The shell
    /// `--chdir`s here and `$HOME` is pointed here too.
    pub cwd: PathBuf,
    /// Whether the sandbox blocks network access. Hardcoded `false` ->
    /// `--share-net` for `Strict`; reserved for a future `Permissive`
    /// profile.
    pub network_allowed: bool,
    /// Environment variables explicitly set inside the sandbox. The
    /// shell starts with `--clearenv`; this is the allowlist of what
    /// gets restored. `PATH` is overwritten by argv-build from the
    /// enabled `Bin` entries, so callers don't need to keep PATH in
    /// sync here.
    pub env: Vec<(String, String)>,
}

/// User-side override applied on top of the auto-detected profile.
/// Stored in `SettingsPayload.sandbox.policy` on the desktop.
///
/// We model the override as a small diff (disabled defaults + custom
/// adds) rather than a full path list so the user's settings survive
/// future detection improvements: when we add a new default bind, all
/// existing users get it without re-saving their config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPolicy {
    /// Absolute paths the user has unchecked. Anything in here that
    /// matches a `Default` entry's `path` flips its `enabled` to false.
    /// Custom entries can also appear here so the UI's "removed
    /// custom" state survives a reload.
    #[serde(default)]
    pub disabled: Vec<PathBuf>,
    /// User-added entries that aren't in the detected defaults. Each
    /// carries its `BindKind` so the argv builder knows whether to add
    /// it to PATH.
    #[serde(default)]
    pub custom: Vec<CustomBind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomBind {
    pub path: PathBuf,
    pub kind: BindKind,
}

// ── Detection ────────────────────────────────────────────────────────

/// Build the host-default strict profile. The result is a pure data
/// description of what *would* be sandboxed if the user accepts every
/// default — no argv yet, no bwrap invocation.
///
/// `cwd` is the session's working directory, used both for the rw bind
/// and as the fake `$HOME` so dotfile-reading tools (git, npm) find
/// nothing rather than the user's real home.
#[must_use]
pub fn detect_strict_profile(cwd: &Path) -> StrictProfile {
    let cwd = cwd.to_path_buf();
    let mut binds = Vec::new();
    // Bin paths: bound + added to $PATH inside the sandbox. The first
    // five are the classic Linux layout; the next three are NixOS-
    // specific. `--ro-bind-try` ignores missing paths so it's safe to
    // list everything on every host — the FE's `present` flag (set
    // here) is how the UI shows what's actually live.
    for p in default_bin_paths() {
        binds.push(detect_bind(p, BindKind::Bin));
    }
    // Lib paths: bound, no PATH entry.
    for p in default_lib_paths() {
        binds.push(detect_bind(p, BindKind::Lib));
    }
    // Config: small read-only files. Bind targets, not dirs.
    for p in default_config_paths() {
        binds.push(detect_bind(p, BindKind::Config));
    }
    StrictProfile {
        binds,
        cwd: cwd.clone(),
        network_allowed: false,
        env: default_env(&cwd),
    }
}

fn detect_bind<P: AsRef<Path>>(path: P, kind: BindKind) -> BindEntry {
    let path = path.as_ref().to_path_buf();
    let present = path.exists();
    BindEntry {
        path,
        kind,
        present,
        enabled: true,
        source: BindSource::Default,
    }
}

fn default_bin_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
        // NixOS layout — symlink farms that resolve into /nix/store/...
        // /nix itself is bound below as a lib path so the resolved
        // targets work.
        "/run/current-system/sw/bin",
        "/run/wrappers/bin",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    // Per-user NixOS profile dir is suffixed with $USER.
    if let Ok(user) = std::env::var("USER") {
        v.push(PathBuf::from(format!("/etc/profiles/per-user/{user}/bin")));
    }
    v
}

fn default_lib_paths() -> Vec<PathBuf> {
    [
        "/usr",
        "/lib",
        "/lib64",
        "/etc/alternatives",
        // /nix is the NixOS store. Bound as a lib path because:
        //   (a) its contents are loader/linker dependencies, not
        //       things you exec by name;
        //   (b) the FS view it provides is huge and we don't want
        //       every binary in /nix/store to appear in PATH.
        "/nix",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

fn default_config_paths() -> Vec<PathBuf> {
    [
        "/etc/passwd",
        "/etc/group",
        "/etc/resolv.conf",
        "/etc/ssl",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

fn default_env(cwd: &Path) -> Vec<(String, String)> {
    let cwd_s = cwd.to_string_lossy().into_owned();
    vec![
        ("HOME".to_string(), cwd_s),
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ]
}

// ── Policy overlay ────────────────────────────────────────────────────

/// Mutate `profile` in place to reflect the user's policy override.
///
/// Two effects:
///   1. Any default `BindEntry` whose `path` appears in `policy.disabled`
///      gets `enabled = false`. Stays in the list so the UI can keep
///      showing the unchecked row.
///   2. Each `policy.custom` entry is appended as `BindSource::Custom`,
///      with `present` re-checked at apply time so the UI's "missing
///      path" warning is current.
pub fn apply_policy_override(profile: &mut StrictProfile, policy: &SandboxPolicy) {
    use std::collections::HashSet;
    let disabled: HashSet<&PathBuf> = policy.disabled.iter().collect();
    for entry in profile.binds.iter_mut() {
        if disabled.contains(&entry.path) {
            entry.enabled = false;
        }
    }
    for c in &policy.custom {
        let enabled = !disabled.contains(&c.path);
        profile.binds.push(BindEntry {
            path: c.path.clone(),
            kind: c.kind,
            present: c.path.exists(),
            enabled,
            source: BindSource::Custom,
        });
    }
}

// ── Argv build ───────────────────────────────────────────────────────

/// Convert the data-shaped profile into a `bwrap …` argv prefix. Pi
/// appends `<shell> -c <command>` after the trailing `--` we emit at
/// the end.
///
/// `enabled=false` entries are skipped. PATH is composed from the
/// `enabled Bin` entries (existence-checked at runtime; the path is
/// emitted whether present or not because `--ro-bind-try` is a no-op
/// when missing).
#[cfg(target_os = "linux")]
#[must_use]
pub fn profile_to_argv(profile: &StrictProfile) -> Vec<String> {
    let cwd_s = profile.cwd.to_string_lossy().into_owned();
    let mut argv: Vec<String> = vec!["bwrap".into()];

    // Filesystem binds, in declared order. Config entries (small files)
    // bind the same way as dirs from bwrap's POV.
    for b in profile.binds.iter().filter(|b| b.enabled) {
        argv.push("--ro-bind-try".into());
        argv.push(b.path.to_string_lossy().into_owned());
        argv.push(b.path.to_string_lossy().into_owned());
    }

    // Writable cwd + ephemeral /tmp + kernel interfaces.
    argv.push("--bind".into());
    argv.push(cwd_s.clone());
    argv.push(cwd_s.clone());
    argv.push("--tmpfs".into());
    argv.push("/tmp".into());
    argv.push("--proc".into());
    argv.push("/proc".into());
    argv.push("--dev".into());
    argv.push("/dev".into());

    // Namespace isolation. We always --unshare-all; if a future
    // profile wants network back, add `--share-net` conditional on
    // `profile.network_allowed`.
    argv.push("--unshare-all".into());
    if profile.network_allowed {
        argv.push("--share-net".into());
    }
    argv.push("--die-with-parent".into());
    argv.push("--new-session".into());

    // Env: clear then restore the allowlist + the composed PATH.
    argv.push("--clearenv".into());
    let path_env = profile
        .binds
        .iter()
        .filter(|b| b.enabled && matches!(b.kind, BindKind::Bin))
        .map(|b| b.path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    argv.push("--setenv".into());
    argv.push("PATH".into());
    argv.push(path_env);
    for (k, v) in &profile.env {
        argv.push("--setenv".into());
        argv.push(k.clone());
        argv.push(v.clone());
    }

    argv.push("--chdir".into());
    argv.push(cwd_s);
    argv.push("--".into());
    argv.shrink_to_fit();
    argv
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn profile_to_argv(_profile: &StrictProfile) -> Vec<String> {
    // Non-Linux: strict isn't supported yet. Returning an empty vec
    // means callers that ignore `is_strict_supported()` would feed pi
    // an empty argv, which pi treats as "no wrapper". The CLI's
    // `--sandbox=strict` bail path catches this earlier with a proper
    // error message.
    Vec::new()
}

// ── Top-level glue ───────────────────────────────────────────────────

/// Build the argv prefix to hand to pi's `BashTool::command_wrapper`.
///
/// Returns `None` for `Off` (and on non-Linux platforms for now) or
/// when the host can't actually deliver strict (bwrap missing). The
/// caller compares against `is_strict_supported()` to differentiate
/// the two cases.
///
/// When `policy` is `Some`, its overrides are applied on top of the
/// auto-detected profile before argv composition.
#[must_use]
pub fn build_command_wrapper(
    mode: SandboxMode,
    cwd: &Path,
    policy: Option<&SandboxPolicy>,
) -> Option<Vec<String>> {
    match mode {
        SandboxMode::Off | SandboxMode::Auto => None,
        SandboxMode::Strict => {
            if !cfg!(target_os = "linux") {
                return None;
            }
            let mut profile = detect_strict_profile(cwd);
            if let Some(p) = policy {
                apply_policy_override(&mut profile, p);
            }
            let argv = profile_to_argv(&profile);
            // Don't hand pi an argv whose first element doesn't exist
            // on disk; pi would spawn it and the call would die deep
            // in the bash tool with a confusing OS error.
            let bin = argv.first()?;
            if binary_on_path(bin).is_some() {
                Some(argv)
            } else {
                None
            }
        }
    }
}

// ── CLI subcommand: `libertai sandbox info` ──────────────────────────

/// Pretty-printer for `libertai sandbox info`. Renders a `StrictProfile`
/// as a human-readable summary; suitable to copy-paste into a bug
/// report. The desktop calls into the JSON-shaped data directly via
/// Tauri instead of parsing this output.
#[must_use]
pub fn format_profile_text(profile: &StrictProfile) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "Strict sandbox profile (Linux + bwrap)");
    let _ = writeln!(out);
    let bwrap = binary_on_path("bwrap");
    let _ = writeln!(
        out,
        "  bwrap:    {}",
        bwrap
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "NOT INSTALLED".to_string())
    );
    let _ = writeln!(out, "  cwd (rw): {}", profile.cwd.display());
    let _ = writeln!(
        out,
        "  network:  {}",
        if profile.network_allowed { "ALLOWED" } else { "blocked" }
    );
    let _ = writeln!(out);
    for (label, kind) in [
        ("Binary paths (bound, prepended to $PATH)", BindKind::Bin),
        ("Library paths (bound, no $PATH entry)",     BindKind::Lib),
        ("Config files",                              BindKind::Config),
    ] {
        let _ = writeln!(out, "  {label}:");
        for b in profile.binds.iter().filter(|b| b.kind == kind) {
            let presence = if b.present { "present" } else { "missing" };
            let state = if b.enabled { " " } else { " [disabled]" };
            let src = match b.source {
                BindSource::Default => "",
                BindSource::Custom => " [custom]",
            };
            let _ = writeln!(
                out,
                "    [{presence}]{state}{src} {}",
                b.path.display()
            );
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "  Environment:");
    let path_env = profile
        .binds
        .iter()
        .filter(|b| b.enabled && b.kind == BindKind::Bin)
        .map(|b| b.path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let _ = writeln!(out, "    PATH = {path_env}");
    for (k, v) in &profile.env {
        let _ = writeln!(out, "    {k} = {v}");
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_resolves_to_off_when_trusted() {
        assert_eq!(SandboxMode::Auto.resolve(false), SandboxMode::Off);
    }

    #[test]
    fn auto_resolves_to_strict_when_untrusted() {
        assert_eq!(SandboxMode::Auto.resolve(true), SandboxMode::Strict);
    }

    #[test]
    fn off_returns_none() {
        let argv = build_command_wrapper(SandboxMode::Off, &PathBuf::from("/tmp/x"), None);
        assert!(argv.is_none());
    }

    #[test]
    fn detect_marks_present_and_disabled_correctly() {
        let p = detect_strict_profile(&PathBuf::from("/work"));
        // Every default entry should appear once.
        assert!(p.binds.iter().any(|b| b.path == PathBuf::from("/usr/bin")));
        assert!(p.binds.iter().any(|b| b.path == PathBuf::from("/etc/passwd")));
        // All defaults start enabled.
        assert!(p.binds.iter().all(|b| b.enabled));
    }

    #[test]
    fn policy_override_disables_default() {
        let mut p = detect_strict_profile(&PathBuf::from("/work"));
        let policy = SandboxPolicy {
            disabled: vec![PathBuf::from("/usr/bin")],
            custom: vec![],
        };
        apply_policy_override(&mut p, &policy);
        let entry = p
            .binds
            .iter()
            .find(|b| b.path == PathBuf::from("/usr/bin"))
            .unwrap();
        assert!(!entry.enabled);
    }

    #[test]
    fn policy_override_appends_custom() {
        let mut p = detect_strict_profile(&PathBuf::from("/work"));
        let policy = SandboxPolicy {
            disabled: vec![],
            custom: vec![CustomBind {
                path: PathBuf::from("/opt/extra/bin"),
                kind: BindKind::Bin,
            }],
        };
        apply_policy_override(&mut p, &policy);
        let entry = p
            .binds
            .iter()
            .find(|b| b.path == PathBuf::from("/opt/extra/bin"))
            .unwrap();
        assert_eq!(entry.kind, BindKind::Bin);
        assert_eq!(entry.source, BindSource::Custom);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_linux_argv_shape() {
        let profile = detect_strict_profile(&PathBuf::from("/work"));
        let argv = profile_to_argv(&profile);
        assert_eq!(argv.first().map(String::as_str), Some("bwrap"));
        assert_eq!(argv.last().map(String::as_str), Some("--"));
        assert!(argv.iter().any(|a| a == "/work"));
        assert!(argv.iter().any(|a| a == "--chdir"));
        assert!(!argv.iter().any(|a| a == "--share-net"));
        assert!(argv.iter().any(|a| a == "--clearenv"));
    }

    /// PATH composition includes all enabled Bin entries (default OR
    /// custom), joined by ':'. Disabled entries are excluded.
    #[cfg(target_os = "linux")]
    #[test]
    fn path_env_reflects_enabled_bins() {
        let mut profile = detect_strict_profile(&PathBuf::from("/work"));
        // Disable /usr/bin to make sure it drops out.
        apply_policy_override(&mut profile, &SandboxPolicy {
            disabled: vec![PathBuf::from("/usr/bin")],
            custom: vec![CustomBind {
                path: PathBuf::from("/opt/custom/bin"),
                kind: BindKind::Bin,
            }],
        });
        let argv = profile_to_argv(&profile);
        let path_idx = argv.iter().position(|a| a == "PATH").unwrap();
        let path_val = &argv[path_idx + 1];
        assert!(!path_val.contains("/usr/bin"));
        assert!(path_val.contains("/opt/custom/bin"));
        assert!(path_val.contains("/bin")); // still on
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_returns_none_when_bwrap_missing() {
        if super::binary_on_path("bwrap").is_some() {
            return;
        }
        assert!(build_command_wrapper(SandboxMode::Strict, &PathBuf::from("/work"), None).is_none());
        assert!(!is_strict_supported());
    }

    #[test]
    fn format_profile_text_renders_sections() {
        let p = detect_strict_profile(&PathBuf::from("/work"));
        let text = format_profile_text(&p);
        assert!(text.contains("Strict sandbox profile"));
        assert!(text.contains("Binary paths"));
        assert!(text.contains("Library paths"));
        assert!(text.contains("Config files"));
        assert!(text.contains("cwd (rw): /work"));
    }
}
