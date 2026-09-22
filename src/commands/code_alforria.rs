//! `libertai code` — alforria engine cutover.
//!
//! The pi-based engine is deprecated; `libertai code` now maps its flags
//! onto the alforria (opencode-compatible) command surface and execs the
//! `alforria` binary: same engine, sessions, and TUI as running alforria
//! directly. (pi and alforria cannot coexist in one binary — both link
//! `tree-sitter` — so this is a process boundary until the desktop app
//! migrates off pi and the engine is linked in-process.)
//!
//! Mapping table (libertai flag → alforria argv):
//! - default / interactive REPL → `alforria tui`
//! - one-shot prompt args → `alforria run`
//! - `--acp` → `alforria acp`
//! - `--model` → `-m <provider>/<model>` (provider defaults to `libertai`)
//! - `--continue` → `--continue`
//! - `--dangerously-skip-permissions` → `--auto`
//! - `--agent`, trailing prompt args → passed through
//!
//! Flags with no alforria equivalent emit a warning and are dropped:
//! `--resume <path>`, `--list-sessions`, `--sandbox`, `--plan`, `--bg`,
//! `--name`, `--team`, `--teammate`.

use std::process::Command;


pub fn run(args: CodeArgs) -> i32 {
    let argv = match build_argv(&args) {
        Ok(argv) => argv,
        Err(message) => {
            eprintln!("Error: {message}");
            return 1;
        }
    };
    // Export the session's inference key for alforria's `libertai` provider
    // env leg; users may also have authenticated via `alforria auth login`.
    if let Ok(config) = crate::config::load() {
        if let Ok(key) = crate::client::require_api_key(&config) {
            std::env::set_var("LIBERTAI_API_KEY", key.to_string());
        }
    }
    let binary = alforria_binary();
    let mut command = Command::new(binary);
    let command = command.args(&argv).env_remove("LIBERTAI_SANDBOX");
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!(
                "Error: could not start the alforria engine ({err}).\n\
                 `libertai code` now runs on alforria — install it with:\n\
                 \tcargo install --git https://github.com/alforria-ai/alforria alforria"
            );
            1
        }
    }
}

fn alforria_binary() -> &'static str {
    "alforria"
}

pub struct CodeArgs {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub plan: bool,
    pub mode: Option<String>,
    pub resume: Option<String>,
    pub continue_recent: bool,
    pub list_sessions: bool,
    pub all: bool,
    pub json: bool,
    pub sandbox: bool,
    pub print: bool,
    pub bg: bool,
    pub name: Option<String>,
    pub agent: Option<String>,
    pub team: Option<String>,
    pub teammate: Option<String>,
    pub acp: bool,
    pub dangerously_skip_permissions: bool,
    pub args: Vec<String>,
}

fn build_argv(code: &CodeArgs) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();

    if code.acp {
        let mut argv = vec![
            "acp".to_string(),
            "--model".to_string(),
            qualified_model(code)?,
        ];
        if let Some(agent) = &code.agent {
            argv.push("--agent".to_string());
            argv.push(agent.clone());
        }
        return Ok(argv);
    }

    if code.resume.is_some() {
        warnings.push(
            "`--resume <path>` is not supported by the alforria engine — sessions live in its SQLite store; use `--continue`".to_string(),
        );
    }
    if code.list_sessions {
        // alforria's session list covers this; keep the output shape close.
        return Ok(vec![
            "session".to_string(),
            "list".to_string(),
            "--format".to_string(),
            if code.json { "json".into() } else { "text".into() },
        ]);
    }
    if code.sandbox {
        warnings.push(
            "`--sandbox` is not supported by the alforria engine (alforria manages bash permissions itself)".to_string(),
        );
    }
    if code.plan {
        warnings.push("`--plan` is not supported by the alforria engine yet".to_string());
    }
    if code.bg || code.name.is_some() {
        warnings.push(
            "`--bg` background runs are not supported by the alforria engine yet".to_string(),
        );
    }
    if code.team.is_some() || code.teammate.is_some() {
        warnings.push(
            "the team system is not available on the alforria engine yet — it will be redeveloped (see ALFORRIA-MIGRATION-PLAN.md)".to_string(),
        );
    }
    if code.mode.is_some() {
        warnings.push(format!(
            "`--mode {}` is not supported by the alforria engine yet",
            code.mode.as_deref().unwrap_or_default()
        ));
    }

    let interactive = !code.print && code.args.is_empty();
    let mut argv = vec![if interactive { "tui" } else { "run" }.to_string()];
    if code.model.is_some() || !interactive {
        argv.push("--model".to_string());
        argv.push(qualified_model(code)?);
    }
    if code.continue_recent {
        argv.push("--continue".to_string());
    }
    if code.dangerously_skip_permissions {
        argv.push("--auto".to_string());
    }
    if let Some(agent) = &code.agent {
        argv.push("--agent".to_string());
        argv.push(agent.clone());
    }
    if !interactive {
        argv.extend(code.args.iter().cloned());
    }
    for warning in warnings {
        eprintln!("Warning: {warning}");
    }
    Ok(argv)
}

/// `<provider>/<model>` with the provider defaulting to the configured one.
fn qualified_model(code: &CodeArgs) -> Result<String, String> {
    let provider = code
        .provider
        .clone()
        .or_else(|| {
            crate::config::load()
                .ok()
                .map(|config| config.default_code_provider.clone())
        })
        .unwrap_or_else(|| "libertai".to_string());
    let model = code
        .model
        .clone()
        .or_else(|| {
            crate::config::load()
                .ok()
                .map(|config| config.default_code_model.clone())
        })
        .ok_or_else(|| "no default code model configured".to_string())?;
    if model.contains('/') {
        Ok(model)
    } else {
        Ok(format!("{provider}/{model}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_args() -> CodeArgs {
        CodeArgs {
            model: Some("test-model".into()),
            provider: Some("libertai".into()),
            plan: false,
            mode: None,
            resume: None,
            continue_recent: false,
            list_sessions: false,
            all: false,
            json: false,
            sandbox: false,
            print: false,
            bg: false,
            name: None,
            agent: None,
            team: None,
            teammate: None,
            acp: false,
            dangerously_skip_permissions: false,
            args: Vec::new(),
        }
    }

    #[test]
    fn interactive_becomes_tui() {
        let mut code = code_args();
        code.model = None;
        code.provider = None;
        let argv = build_argv(&code).unwrap();
        assert_eq!(argv, vec!["tui"]);
    }

    #[test]
    fn sandbox_warns() {
        let mut code = code_args();
        code.model = None;
        code.provider = None;
        code.sandbox = true;
        let argv = build_argv(&code).unwrap();
        assert_eq!(argv, vec!["tui"]);
    }

    #[test]
    fn prompt_args_become_run() {
        let mut code = code_args();
        code.args = vec!["fix".into(), "it".into()];
        let argv = build_argv(&code).unwrap();
        assert_eq!(
            argv,
            vec!["run", "--model", "libertai/test-model", "fix", "it"]
        );
    }

    #[test]
    fn qualified_model_prefixes_provider() {
        let mut code = code_args();
        code.model = Some("glm-5.3-thinking".into());
        code.provider = Some("libertai".into());
        assert_eq!(
            build_argv(&code).unwrap(),
            vec!["tui", "--model", "libertai/glm-5.3-thinking"]
        );
    }

    #[test]
    fn skip_permissions_maps_to_auto() {
        let mut code = code_args();
        code.model = None;
        code.provider = None;
        code.dangerously_skip_permissions = true;
        let argv = build_argv(&code).unwrap();
        assert_eq!(argv, vec!["tui", "--auto"]);
    }

    #[test]
    fn acp_maps_to_acp() {
        let mut code = code_args();
        code.acp = true;
        let argv = build_argv(&code).unwrap();
        assert_eq!(argv, vec!["acp", "--model", "libertai/test-model"]);
    }

    #[test]
    fn team_warns() {
        let mut code = code_args();
        code.model = None;
        code.provider = None;
        code.team = Some("alpha".into());
        let argv = build_argv(&code).unwrap();
        assert_eq!(argv, vec!["tui"]);
    }

    #[test]
    fn model_only_defaults_to_config() {
        let mut code = code_args();
        code.model = Some("other/any".into());
        code.provider = None;
        let argv = build_argv(&code).unwrap();
        assert_eq!(argv, vec!["tui", "--model", "other/any"]);
    }
}
