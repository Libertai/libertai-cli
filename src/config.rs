use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

pub const DEFAULT_API_BASE: &str = "https://api.libertai.io";
pub const DEFAULT_SEARCH_BASE: &str = "https://search.libertai.io";
pub const DEFAULT_CHAT_MODEL: &str = "qwen3.5-122b-a10b";
pub const DEFAULT_CODE_MODEL: &str = "qwen3.6-35b-a3b";
pub const DEFAULT_CODE_PROVIDER: &str = "libertai";
pub const DEFAULT_IMAGE_MODEL: &str = "z-image-turbo";
pub const DEFAULT_OPUS_MODEL: &str = "gemma-4-31b-it";
pub const DEFAULT_FAST_MODEL: &str = "qwen3.6-35b-a3b";
/// Idle timeout (seconds) for HTTP requests, including SSE token streams.
/// Pi's http client uses this as a per-chunk idle deadline — a brief
/// pause from the model (or a tool-execution gap) of more than this
/// many seconds will fail the request with "Request timed out". The 60s
/// pi default was triggering on long generations; 600s is generous
/// enough to ride out most provider hiccups while still bounding truly
/// stuck connections.
pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_CHECK_FOR_UPDATES: bool = true;
pub const DEFAULT_SMART_APPROVAL_ENABLED: bool = false;
pub const DEFAULT_SMART_APPROVAL_MODEL: &str = DEFAULT_FAST_MODEL;
pub const DEFAULT_CODE_AUTO_COMPACTION_ENABLED: bool = true;
pub const DEFAULT_CODE_COMPACTION_RESERVE_TOKENS: u32 = 16_384;
pub const DEFAULT_CODE_COMPACTION_KEEP_RECENT_TOKENS: u32 = 20_000;
pub const DEFAULT_CODE_TURN_NOTIFICATIONS: bool = false;

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
        default = "default_code_provider_s",
        skip_serializing_if = "is_default_code_provider"
    )]
    pub default_code_provider: String,
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
    #[serde(
        default = "default_smart_approval_enabled",
        skip_serializing_if = "is_default_smart_approval_enabled"
    )]
    pub smart_approval_enabled: bool,
    #[serde(
        default = "default_smart_approval_model_s",
        skip_serializing_if = "is_default_smart_approval_model"
    )]
    pub smart_approval_model: String,
    #[serde(
        default = "default_code_auto_compaction_enabled",
        skip_serializing_if = "is_default_code_auto_compaction_enabled"
    )]
    pub code_auto_compaction_enabled: bool,
    #[serde(
        default = "default_code_compaction_reserve_tokens",
        skip_serializing_if = "is_default_code_compaction_reserve_tokens"
    )]
    pub code_compaction_reserve_tokens: u32,
    #[serde(
        default = "default_code_compaction_keep_recent_tokens",
        skip_serializing_if = "is_default_code_compaction_keep_recent_tokens"
    )]
    pub code_compaction_keep_recent_tokens: u32,
    #[serde(
        default = "default_code_turn_notifications",
        skip_serializing_if = "is_default_code_turn_notifications"
    )]
    pub code_turn_notifications: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status_line_template: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status_line_command: String,
    #[serde(default, skip_serializing_if = "HooksConfig::is_default")]
    pub hooks: HooksConfig,
    #[serde(default, rename = "mcpServers", skip_serializing_if = "HashMap::is_empty")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
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
fn is_default_code_provider(s: &str) -> bool {
    s == DEFAULT_CODE_PROVIDER
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
fn is_default_smart_approval_enabled(v: &bool) -> bool {
    *v == DEFAULT_SMART_APPROVAL_ENABLED
}
fn is_default_smart_approval_model(s: &str) -> bool {
    s == DEFAULT_SMART_APPROVAL_MODEL
}
fn is_default_code_auto_compaction_enabled(v: &bool) -> bool {
    *v == DEFAULT_CODE_AUTO_COMPACTION_ENABLED
}
fn is_default_code_compaction_reserve_tokens(v: &u32) -> bool {
    *v == DEFAULT_CODE_COMPACTION_RESERVE_TOKENS
}
fn is_default_code_compaction_keep_recent_tokens(v: &u32) -> bool {
    *v == DEFAULT_CODE_COMPACTION_KEEP_RECENT_TOKENS
}
fn is_default_code_turn_notifications(v: &bool) -> bool {
    *v == DEFAULT_CODE_TURN_NOTIFICATIONS
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transport: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<McpResourceConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompts: Vec<McpPromptConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolConfig {
    pub name: String,
    #[serde(default = "default_mcp_tool_enabled", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(
        default,
        rename = "inputSchema",
        alias = "input_schema",
        skip_serializing_if = "Option::is_none"
    )]
    pub input_schema: Option<serde_json::Value>,
}

impl Default for McpToolConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            description: String::new(),
            input_schema: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpResourceConfig {
    pub uri: String,
    #[serde(default = "default_mcp_tool_enabled", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, rename = "mimeType", alias = "mime_type", skip_serializing_if = "String::is_empty")]
    pub mime_type: String,
}

impl Default for McpResourceConfig {
    fn default() -> Self {
        Self {
            uri: String::new(),
            enabled: true,
            name: String::new(),
            description: String::new(),
            mime_type: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpPromptConfig {
    pub name: String,
    #[serde(default = "default_mcp_tool_enabled", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<McpPromptArgumentConfig>,
}

impl Default for McpPromptConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            description: String::new(),
            arguments: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpPromptArgumentConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
}

fn default_mcp_tool_enabled() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HooksConfig {
    #[serde(default, rename = "UserPromptSubmit", skip_serializing_if = "Vec::is_empty")]
    pub user_prompt_submit: Vec<HookCommandConfig>,
    #[serde(default, rename = "PreToolUse", skip_serializing_if = "Vec::is_empty")]
    pub pre_tool_use: Vec<HookCommandConfig>,
    #[serde(default, rename = "PostToolUse", skip_serializing_if = "Vec::is_empty")]
    pub post_tool_use: Vec<HookCommandConfig>,
    #[serde(default, rename = "SubagentStop", skip_serializing_if = "Vec::is_empty")]
    pub subagent_stop: Vec<HookCommandConfig>,
    #[serde(default, rename = "SessionStart", skip_serializing_if = "Vec::is_empty")]
    pub session_start: Vec<HookCommandConfig>,
    #[serde(default, rename = "Stop", skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<HookCommandConfig>,
    #[serde(default, rename = "SessionEnd", skip_serializing_if = "Vec::is_empty")]
    pub session_end: Vec<HookCommandConfig>,
    #[serde(default, rename = "Notification", skip_serializing_if = "Vec::is_empty")]
    pub notification: Vec<HookCommandConfig>,
}

impl<'de> Deserialize<'de> for HooksConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawHooksConfig {
            #[serde(default, rename = "UserPromptSubmit")]
            user_prompt_submit: Vec<serde_json::Value>,
            #[serde(default, rename = "PreToolUse")]
            pre_tool_use: Vec<serde_json::Value>,
            #[serde(default, rename = "PostToolUse")]
            post_tool_use: Vec<serde_json::Value>,
            #[serde(default, rename = "SubagentStop")]
            subagent_stop: Vec<serde_json::Value>,
            #[serde(default, rename = "SessionStart")]
            session_start: Vec<serde_json::Value>,
            #[serde(default, rename = "Stop")]
            stop: Vec<serde_json::Value>,
            #[serde(default, rename = "SessionEnd")]
            session_end: Vec<serde_json::Value>,
            #[serde(default, rename = "Notification")]
            notification: Vec<serde_json::Value>,
        }

        let raw = RawHooksConfig::deserialize(deserializer)?;
        Ok(Self {
            user_prompt_submit: deserialize_hook_rows(raw.user_prompt_submit)
                .map_err(serde::de::Error::custom)?,
            pre_tool_use: deserialize_hook_rows(raw.pre_tool_use)
                .map_err(serde::de::Error::custom)?,
            post_tool_use: deserialize_hook_rows(raw.post_tool_use)
                .map_err(serde::de::Error::custom)?,
            subagent_stop: deserialize_hook_rows(raw.subagent_stop)
                .map_err(serde::de::Error::custom)?,
            session_start: deserialize_hook_rows(raw.session_start)
                .map_err(serde::de::Error::custom)?,
            stop: deserialize_hook_rows(raw.stop).map_err(serde::de::Error::custom)?,
            session_end: deserialize_hook_rows(raw.session_end).map_err(serde::de::Error::custom)?,
            notification: deserialize_hook_rows(raw.notification).map_err(serde::de::Error::custom)?,
        })
    }
}

impl HooksConfig {
    fn is_default(&self) -> bool {
        self.user_prompt_submit.is_empty()
            && self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.subagent_stop.is_empty()
            && self.session_start.is_empty()
            && self.stop.is_empty()
            && self.session_end.is_empty()
            && self.notification.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HookCommandConfig {
    #[serde(default = "default_hook_enabled")]
    pub enabled: bool,
    #[serde(
        default,
        alias = "matchers",
        deserialize_with = "deserialize_matcher",
        skip_serializing_if = "String::is_empty"
    )]
    pub matcher: String,
    #[serde(default, rename = "if", skip_serializing_if = "String::is_empty")]
    pub if_condition: String,
    #[serde(
        default = "default_hook_type",
        rename = "type",
        skip_serializing_if = "is_default_hook_type"
    )]
    pub hook_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default, rename = "allowedEnvVars", skip_serializing_if = "Vec::is_empty")]
    pub allowed_env_vars: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(default, rename = "statusMessage", skip_serializing_if = "String::is_empty")]
    pub status_message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shell: String,
    #[serde(
        default,
        deserialize_with = "deserialize_timeout",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<u64>,
    #[serde(
        default,
        rename = "async",
        alias = "asyncHook",
        skip_serializing_if = "is_false"
    )]
    pub async_hook: bool,
    #[serde(
        default,
        rename = "continueOnBlock",
        skip_serializing_if = "is_false"
    )]
    pub continue_on_block: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub once: bool,
    #[serde(default, rename = "asyncRewake", skip_serializing_if = "is_false")]
    pub async_rewake: bool,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for HookCommandConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawHookCommandConfig {
            #[serde(default = "default_hook_enabled")]
            enabled: bool,
            #[serde(default, alias = "matchers", deserialize_with = "deserialize_matcher")]
            matcher: String,
            #[serde(default, rename = "if")]
            if_condition: String,
            #[serde(default = "default_hook_type", rename = "type")]
            hook_type: String,
            #[serde(default, deserialize_with = "deserialize_command")]
            command: CommandParts,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            url: String,
            #[serde(default)]
            headers: HashMap<String, String>,
            #[serde(default, rename = "allowedEnvVars")]
            allowed_env_vars: Vec<String>,
            #[serde(default)]
            prompt: String,
            #[serde(default)]
            model: String,
            #[serde(default)]
            source: String,
            #[serde(default)]
            server: String,
            #[serde(default)]
            tool: String,
            #[serde(default)]
            input: Option<serde_json::Value>,
            #[serde(default, rename = "statusMessage")]
            status_message: String,
            #[serde(default)]
            shell: String,
            #[serde(default, deserialize_with = "deserialize_timeout")]
            timeout: Option<u64>,
            #[serde(default, rename = "async", alias = "asyncHook")]
            async_hook: bool,
            #[serde(default, rename = "continueOnBlock")]
            continue_on_block: bool,
            #[serde(default)]
            once: bool,
            #[serde(default, rename = "asyncRewake")]
            async_rewake: bool,
            #[serde(flatten, default)]
            extra: BTreeMap<String, serde_json::Value>,
        }

        let mut raw = RawHookCommandConfig::deserialize(deserializer)?;
        let mut args = raw.command.args;
        args.append(&mut raw.args);
        Ok(Self {
            enabled: raw.enabled,
            matcher: raw.matcher,
            if_condition: raw.if_condition,
            hook_type: raw.hook_type,
            command: raw.command.command,
            args,
            url: raw.url,
            headers: raw.headers,
            allowed_env_vars: raw.allowed_env_vars,
            prompt: raw.prompt,
            model: raw.model,
            source: raw.source,
            server: raw.server,
            tool: raw.tool,
            input: raw.input,
            status_message: raw.status_message,
            shell: raw.shell,
            timeout: raw.timeout,
            async_hook: raw.async_hook,
            continue_on_block: raw.continue_on_block,
            once: raw.once,
            async_rewake: raw.async_rewake,
            extra: raw.extra,
        })
    }
}

impl Default for HookCommandConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            matcher: String::new(),
            if_condition: String::new(),
            hook_type: default_hook_type(),
            command: String::new(),
            args: Vec::new(),
            url: String::new(),
            headers: HashMap::new(),
            allowed_env_vars: Vec::new(),
            prompt: String::new(),
            model: String::new(),
            source: String::new(),
            server: String::new(),
            tool: String::new(),
            input: None,
            status_message: String::new(),
            shell: String::new(),
            timeout: None,
            async_hook: false,
            continue_on_block: false,
            once: false,
            async_rewake: false,
            extra: BTreeMap::new(),
        }
    }
}

fn default_hook_enabled() -> bool {
    true
}

fn default_hook_type() -> String {
    "command".to_string()
}

fn is_default_hook_type(value: &str) -> bool {
    value == "command"
}

fn deserialize_matcher<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MatcherValue {
        String(String),
        Array(Vec<String>),
    }

    match Option::<MatcherValue>::deserialize(deserializer)? {
        Some(MatcherValue::String(value)) => Ok(value),
        Some(MatcherValue::Array(values)) => Ok(values
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("|")),
        None => Ok(String::new()),
    }
}

#[derive(Debug, Clone, Default)]
struct HookGroupDefaults {
    matcher: String,
    if_condition: String,
    enabled: bool,
    async_hook: bool,
    timeout: Option<u64>,
    source: String,
    status_message: String,
    extra: BTreeMap<String, serde_json::Value>,
}

fn deserialize_hook_rows(
    rows: Vec<serde_json::Value>,
) -> std::result::Result<Vec<HookCommandConfig>, String> {
    let mut out = Vec::new();
    for row in rows {
        let Some(hooks) = row.get("hooks") else {
            out.push(deserialize_hook_row(row)?);
            continue;
        };
        let hooks = hooks
            .as_array()
            .ok_or_else(|| "Claude-style hook group `hooks` must be an array".to_string())?;
        let defaults = hook_group_defaults(&row)?;
        for hook in hooks {
            let mut child = deserialize_hook_row(hook.clone())?;
            if child.matcher.trim().is_empty() {
                child.matcher = defaults.matcher.clone();
            }
            if child.if_condition.trim().is_empty() {
                child.if_condition = defaults.if_condition.clone();
            }
            child.enabled = defaults.enabled && child.enabled;
            if defaults.async_hook {
                child.async_hook = true;
            }
            if child.timeout.is_none() {
                child.timeout = defaults.timeout;
            }
            if child.source.trim().is_empty() {
                child.source = defaults.source.clone();
            }
            if child.status_message.trim().is_empty() {
                child.status_message = defaults.status_message.clone();
            }
            for (key, value) in &defaults.extra {
                child.extra.entry(key.clone()).or_insert_with(|| value.clone());
            }
            out.push(child);
        }
    }
    Ok(out)
}

fn deserialize_hook_row(
    row: serde_json::Value,
) -> std::result::Result<HookCommandConfig, String> {
    serde_json::from_value(row).map_err(|e| format!("invalid hook row: {e}"))
}

fn hook_group_defaults(
    row: &serde_json::Value,
) -> std::result::Result<HookGroupDefaults, String> {
    let matcher = row
        .get("matcher")
        .or_else(|| row.get("matchers"))
        .map(matcher_from_json_value)
        .transpose()?
        .unwrap_or_default();
    Ok(HookGroupDefaults {
        matcher,
        if_condition: row
            .get("if")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        enabled: row
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        async_hook: json_bool_field(row, "async") || json_bool_field(row, "asyncHook"),
        timeout: timeout_from_json_value(row)?,
        source: row
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status_message: row
            .get("statusMessage")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        extra: hook_group_extra_fields(row),
    })
}

fn matcher_from_json_value(value: &serde_json::Value) -> std::result::Result<String, String> {
    match value {
        serde_json::Value::Null => Ok(String::new()),
        serde_json::Value::String(value) => Ok(value.clone()),
        serde_json::Value::Array(values) => Ok(values
            .iter()
            .filter_map(|value| value.as_str().map(str::trim))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
            .join("|")),
        _ => Err("hook matcher must be a string or string array".to_string()),
    }
}

fn timeout_from_json_value(
    row: &serde_json::Value,
) -> std::result::Result<Option<u64>, String> {
    match row.get("timeout") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(value)) => value
            .as_u64()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| "hook timeout must be a positive integer number of seconds".to_string()),
        Some(serde_json::Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0)
                    .map(Some)
                    .ok_or_else(|| {
                        "hook timeout must be a positive integer number of seconds".to_string()
                    })
            }
        }
        Some(_) => Err("hook timeout must be a positive integer number of seconds".to_string()),
    }
}

fn json_bool_field(row: &serde_json::Value, key: &str) -> bool {
    row.get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn hook_group_extra_fields(row: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    let Some(object) = row.as_object() else {
        return BTreeMap::new();
    };
    object
        .iter()
        .filter(|(key, _)| !is_known_hook_group_field(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn is_known_hook_group_field(key: &str) -> bool {
    matches!(
        key,
        "enabled"
            | "matcher"
            | "matchers"
            | "if"
            | "hooks"
            | "async"
            | "asyncHook"
            | "timeout"
            | "source"
            | "statusMessage"
    )
}

#[derive(Debug, Clone, Default)]
struct CommandParts {
    command: String,
    args: Vec<String>,
}

fn deserialize_command<'de, D>(deserializer: D) -> std::result::Result<CommandParts, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum CommandValue {
        String(String),
        Array(Vec<String>),
    }

    match Option::<CommandValue>::deserialize(deserializer)? {
        Some(CommandValue::String(command)) => Ok(CommandParts {
            command,
            args: Vec::new(),
        }),
        Some(CommandValue::Array(mut values)) => {
            let command = values.first().cloned().unwrap_or_default();
            let args = if values.is_empty() {
                Vec::new()
            } else {
                values.drain(1..).collect()
            };
            Ok(CommandParts { command, args })
        }
        None => Ok(CommandParts::default()),
    }
}

fn deserialize_timeout<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum TimeoutValue {
        Integer(u64),
        String(String),
    }

    match Option::<TimeoutValue>::deserialize(deserializer)? {
        Some(TimeoutValue::Integer(value)) if value > 0 => Ok(Some(value)),
        Some(TimeoutValue::Integer(_)) => Err(serde::de::Error::custom(
            "hook timeout must be a positive integer number of seconds",
        )),
        Some(TimeoutValue::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .map(Some)
                .ok_or_else(|| {
                    serde::de::Error::custom(
                        "hook timeout must be a positive integer number of seconds",
                    )
                })
        }
        None => Ok(None),
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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
fn default_code_provider_s() -> String {
    DEFAULT_CODE_PROVIDER.into()
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
fn default_smart_approval_enabled() -> bool {
    DEFAULT_SMART_APPROVAL_ENABLED
}
fn default_smart_approval_model_s() -> String {
    DEFAULT_SMART_APPROVAL_MODEL.into()
}
fn default_code_auto_compaction_enabled() -> bool {
    DEFAULT_CODE_AUTO_COMPACTION_ENABLED
}
fn default_code_compaction_reserve_tokens() -> u32 {
    DEFAULT_CODE_COMPACTION_RESERVE_TOKENS
}
fn default_code_compaction_keep_recent_tokens() -> u32 {
    DEFAULT_CODE_COMPACTION_KEEP_RECENT_TOKENS
}
fn default_code_turn_notifications() -> bool {
    DEFAULT_CODE_TURN_NOTIFICATIONS
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base: default_api_base(),
            account_base: default_account_base(),
            search_base: default_search_base_s(),
            default_chat_model: default_chat_model_s(),
            default_code_model: default_code_model_s(),
            default_code_provider: default_code_provider_s(),
            default_image_model: default_image_model_s(),
            launcher_defaults: LauncherDefaults::default(),
            http_timeout_secs: DEFAULT_HTTP_TIMEOUT_SECS,
            check_for_updates: DEFAULT_CHECK_FOR_UPDATES,
            smart_approval_enabled: DEFAULT_SMART_APPROVAL_ENABLED,
            smart_approval_model: default_smart_approval_model_s(),
            code_auto_compaction_enabled: DEFAULT_CODE_AUTO_COMPACTION_ENABLED,
            code_compaction_reserve_tokens: DEFAULT_CODE_COMPACTION_RESERVE_TOKENS,
            code_compaction_keep_recent_tokens: DEFAULT_CODE_COMPACTION_KEEP_RECENT_TOKENS,
            code_turn_notifications: DEFAULT_CODE_TURN_NOTIFICATIONS,
            status_line_template: String::new(),
            status_line_command: String::new(),
            hooks: HooksConfig::default(),
            mcp_servers: HashMap::new(),
            auth: Auth::default(),
        }
    }
}

/// Returns `~/.config/libertai`, respecting `$XDG_CONFIG_HOME`.
pub fn libertai_config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine user config dir")?;
    Ok(base.join("libertai"))
}

/// Returns `~/.config/libertai/config.toml`, respecting `$XDG_CONFIG_HOME`.
pub fn config_path() -> Result<PathBuf> {
    Ok(libertai_config_dir()?.join("config.toml"))
}

/// Returns `~/.config/libertai/allow-rules.toml`, respecting `$XDG_CONFIG_HOME`.
pub fn allow_rules_path() -> Result<PathBuf> {
    Ok(libertai_config_dir()?.join("allow-rules.toml"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_server_config_preserves_cached_tools() {
        let raw = r#"
            [mcpServers.docs]
            command = "server"

            [[mcpServers.docs.tools]]
            name = "search"
            description = "Search docs"

            input_schema = { type = "object", required = ["query"] }

            [[mcpServers.docs.tools]]
            name = "admin"
            enabled = false

            [[mcpServers.docs.resources]]
            uri = "file:///repo/README.md"
            name = "README"
            mimeType = "text/markdown"

            [[mcpServers.docs.prompts]]
            name = "summarize"
            description = "Summarize docs"

            [[mcpServers.docs.prompts.arguments]]
            name = "topic"
            required = true
        "#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let server = cfg.mcp_servers.get("docs").unwrap();
        assert_eq!(server.tools.len(), 2);
        assert_eq!(server.tools[0].name, "search");
        assert!(server.tools[0].enabled);
        assert_eq!(server.tools[0].description, "Search docs");
        assert_eq!(
            server.tools[0].input_schema.as_ref().unwrap()["required"],
            json!(["query"])
        );
        assert!(!server.tools[1].enabled);
        assert_eq!(server.resources[0].uri, "file:///repo/README.md");
        assert_eq!(server.resources[0].mime_type, "text/markdown");
        assert_eq!(server.prompts[0].name, "summarize");
        assert_eq!(server.prompts[0].arguments[0].name, "topic");
        assert!(server.prompts[0].arguments[0].required);

        let encoded = toml::to_string(&cfg).unwrap();
        assert!(encoded.contains("[[mcpServers.docs.tools]]"));
        assert!(encoded.contains("[[mcpServers.docs.resources]]"));
        assert!(encoded.contains("[[mcpServers.docs.prompts]]"));
        assert!(encoded.contains("inputSchema"));
    }
}
