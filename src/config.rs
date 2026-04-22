use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_API_BASE: &str = "https://api.libertai.io";
pub const DEFAULT_SEARCH_BASE: &str = "https://search.libertai.io";
pub const DEFAULT_CHAT_MODEL: &str = "qwen3.5-122b-a10b";
pub const DEFAULT_CODE_MODEL: &str = "qwen3.6-35b-a3b";
pub const DEFAULT_IMAGE_MODEL: &str = "z-image-turbo";
pub const DEFAULT_OPUS_MODEL: &str = "gemma-4-31b-it";
pub const DEFAULT_FAST_MODEL: &str = "qwen3.6-35b-a3b";
pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_CHECK_FOR_UPDATES: bool = true;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_api_base", skip_serializing_if = "is_default_api_base")]
    pub api_base: String,
    #[serde(
        default = "default_account_base",
        skip_serializing_if = "is_default_account_base"
    )]
    pub account_base: String,
    #[serde(
        default = "default_search_base_s",
        skip_serializing_if = "is_default_search_base"
    )]
    pub search_base: String,
    #[serde(
        default = "default_chat_model_s",
        skip_serializing_if = "is_default_chat_model"
    )]
    pub default_chat_model: String,
    #[serde(
        default = "default_code_model_s",
        skip_serializing_if = "is_default_code_model"
    )]
    pub default_code_model: String,
    #[serde(
        default = "default_image_model_s",
        skip_serializing_if = "is_default_image_model"
    )]
    pub default_image_model: String,
    #[serde(default, skip_serializing_if = "LauncherDefaults::is_default")]
    pub launcher_defaults: LauncherDefaults,
    #[serde(
        default = "default_http_timeout_secs",
        skip_serializing_if = "is_default_http_timeout_secs"
    )]
    pub http_timeout_secs: u64,
    #[serde(
        default = "default_check_for_updates",
        skip_serializing_if = "is_default_check_for_updates"
    )]
    pub check_for_updates: bool,
    #[serde(default)]
    pub auth: Auth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherDefaults {
    #[serde(
        default = "default_opus_model_s",
        skip_serializing_if = "is_default_opus_model"
    )]
    pub opus_model: String,
    #[serde(
        default = "default_fast_model_s",
        skip_serializing_if = "is_default_sonnet_model"
    )]
    pub sonnet_model: String,
    #[serde(
        default = "default_fast_model_s",
        skip_serializing_if = "is_default_haiku_model"
    )]
    pub haiku_model: String,
}

impl LauncherDefaults {
    fn is_default(&self) -> bool {
        is_default_opus_model(&self.opus_model)
            && is_default_sonnet_model(&self.sonnet_model)
            && is_default_haiku_model(&self.haiku_model)
    }
}

fn is_default_api_base(s: &str) -> bool {
    s == DEFAULT_API_BASE
}
fn is_default_account_base(s: &str) -> bool {
    s == DEFAULT_API_BASE
}
fn is_default_search_base(s: &str) -> bool {
    s == DEFAULT_SEARCH_BASE
}
fn is_default_chat_model(s: &str) -> bool {
    s == DEFAULT_CHAT_MODEL
}
fn is_default_code_model(s: &str) -> bool {
    s == DEFAULT_CODE_MODEL
}
fn is_default_image_model(s: &str) -> bool {
    s == DEFAULT_IMAGE_MODEL
}
fn is_default_opus_model(s: &str) -> bool {
    s == DEFAULT_OPUS_MODEL
}
fn is_default_sonnet_model(s: &str) -> bool {
    s == DEFAULT_FAST_MODEL
}
fn is_default_haiku_model(s: &str) -> bool {
    s == DEFAULT_FAST_MODEL
}
fn is_default_http_timeout_secs(v: &u64) -> bool {
    *v == DEFAULT_HTTP_TIMEOUT_SECS
}
fn is_default_check_for_updates(v: &bool) -> bool {
    *v == DEFAULT_CHECK_FOR_UPDATES
}

impl Default for LauncherDefaults {
    fn default() -> Self {
        Self {
            opus_model: DEFAULT_OPUS_MODEL.into(),
            sonnet_model: DEFAULT_FAST_MODEL.into(),
            haiku_model: DEFAULT_FAST_MODEL.into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Auth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
}

fn default_api_base() -> String {
    DEFAULT_API_BASE.into()
}
fn default_account_base() -> String {
    DEFAULT_API_BASE.into()
}
fn default_search_base_s() -> String {
    DEFAULT_SEARCH_BASE.into()
}
fn default_chat_model_s() -> String {
    DEFAULT_CHAT_MODEL.into()
}
fn default_code_model_s() -> String {
    DEFAULT_CODE_MODEL.into()
}
fn default_image_model_s() -> String {
    DEFAULT_IMAGE_MODEL.into()
}
fn default_opus_model_s() -> String {
    DEFAULT_OPUS_MODEL.into()
}
fn default_fast_model_s() -> String {
    DEFAULT_FAST_MODEL.into()
}
fn default_http_timeout_secs() -> u64 {
    DEFAULT_HTTP_TIMEOUT_SECS
}
fn default_check_for_updates() -> bool {
    DEFAULT_CHECK_FOR_UPDATES
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base: default_api_base(),
            account_base: default_account_base(),
            search_base: default_search_base_s(),
            default_chat_model: default_chat_model_s(),
            default_code_model: default_code_model_s(),
            default_image_model: default_image_model_s(),
            launcher_defaults: LauncherDefaults::default(),
            http_timeout_secs: DEFAULT_HTTP_TIMEOUT_SECS,
            check_for_updates: DEFAULT_CHECK_FOR_UPDATES,
            auth: Auth::default(),
        }
    }
}

/// Returns `~/.config/libertai/config.toml`, respecting `$XDG_CONFIG_HOME`.
pub fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine user config dir")?;
    Ok(base.join("libertai").join("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    enforce_https_bases(&cfg)?;
    Ok(cfg)
}

fn enforce_https_bases(cfg: &Config) -> Result<()> {
    for (name, base) in [
        ("api_base", &cfg.api_base),
        ("account_base", &cfg.account_base),
        ("search_base", &cfg.search_base),
    ] {
        let trimmed = base.trim();
        let parsed = url::Url::parse(trimmed).map_err(|_| {
            anyhow::anyhow!("config: {name} must be a plain https://host URL — got {trimmed}")
        })?;
        let path_ok = parsed.path().is_empty() || parsed.path() == "/";
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || !path_ok
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.host().is_none()
        {
            anyhow::bail!(
                "config: {name} must be a plain https://host URL — got {trimmed}"
            );
        }
    }
    Ok(())
}

pub fn save(cfg: &Config) -> Result<()> {
    enforce_https_bases(cfg)?;
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        create_dir_secure(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(cfg).context("serializing config")?;
    write_file_secure(&path, raw.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn create_dir_secure(parent: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if parent.exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn create_dir_secure(parent: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(parent)?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn write_file_secure(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    // Re-apply mode in case the file already existed with different perms.
    set_file_mode_600(path)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn write_file_secure(path: &std::path::Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data)?;
    Ok(())
}

#[cfg(unix)]
pub fn set_file_mode_600(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(0o600);
    std::fs::set_permissions(path, perm)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn set_file_mode_600(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

/// `LTAI_****abcd` — first 4 + last 4 of a key.
pub fn mask_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 8 {
        return "*".repeat(len);
    }
    let prefix: String = key.chars().take(4).collect();
    let suffix: String = key.chars().skip(len - 4).collect();
    format!("{prefix}****{suffix}")
}
