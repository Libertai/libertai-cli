//! The `skill` tool — load an Agent Skill's full body on demand.
//!
//! (M5/#7, per docs/overhaul-plan.md) Skill bodies are deliberately kept
//! OUT of the system prompt: `code_skills::prompt_for_pillar` surfaces
//! only a latent registry (name + description). When the model decides a
//! task matches a skill, it calls `skill(name=...)` to load that skill's
//! complete `SKILL.md` body — the same body `prompt_for_pillar` used to
//! inline, but now fetched lazily so a 40-skill session doesn't ship 40
//! bodies in the system prompt every turn.
//!
//! The tool is read-only (filesystem reads only, no writes), so the
//! factory registers it unwrapped — same rationale as `todo`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};

use crate::commands::code_skills::{self, AgentSkill, SkillPillar};

const NAME: &str = "skill";
const LABEL: &str = "Skill";

const DESCRIPTION: &str = concat!(
    "Load an Agent Skill's full instructions by name. The available skills are listed in the ",
    "system prompt under 'Available Agent Skills' (name + description only); their full bodies ",
    "are NOT in the prompt. When a task seems to match a listed skill, call this tool with the ",
    "skill's exact `name` to retrieve its complete body, then follow those instructions. ",
    "Optional `args` is forwarded for skills that document argument substitution. If the ",
    "name is unknown, the result lists the available skill names — pick the right one and retry."
);

#[derive(Debug, Clone, Deserialize)]
struct SkillInput {
    name: String,
    #[serde(default)]
    args: Option<String>,
}

/// The `skill` tool. Carries the pillar + cwd so it can resolve the
/// active-skill set the same way `prompt_for_pillar` does — a skill is
/// only loadable if it's active for this pillar and not disabled.
pub struct SkillTool {
    pillar: SkillPillar,
    cwd: Option<PathBuf>,
}

impl SkillTool {
    /// Construct for a given pillar + working directory. The code
    /// sessions always pass `SkillPillar::Code` (matching every
    /// `prompt_for_pillar` call site).
    pub fn new(pillar: SkillPillar, cwd: Option<PathBuf>) -> Self {
        Self { pillar, cwd }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact name of the skill to load (from the 'Available Agent Skills' list)."
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments forwarded to skills that document argument substitution."
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: SkillInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(unknown_skill_output(&format!(
                    "invalid `skill` payload: {e}"
                ), &[]));
            }
        };

        let name = parsed.name.trim();
        match code_skills::load_skill_by_name(self.pillar, self.cwd.as_deref(), name) {
            Ok(Some(skill)) => Ok(loaded_output(&skill, parsed.args.as_deref())),
            Ok(None) => {
                // Unknown / disabled / wrong-pillar skill — list the
                // available names so the model can retry with the right
                // spelling instead of guessing.
                let names = code_skills::active_skill_name_list(
                    self.pillar,
                    self.cwd.as_deref(),
                )
                .unwrap_or_default();
                Ok(unknown_skill_output(
                    &format!("No active skill named `{name}`."),
                    &names,
                ))
            }
            Err(e) => Ok(err_output(&format!("loading skill `{name}`: {e}"))),
        }
    }

    fn is_read_only(&self) -> bool {
        // Only reads SKILL.md files from disk; no writes, no network.
        true
    }
}

/// The success body returned to the model — the skill's full `SKILL.md`
/// body, prefixed with its name + allowed-tools hint so the gating is
/// visible even though the frontmatter is stripped from the prompt.
fn loaded_output(skill: &AgentSkill, args: Option<&str>) -> ToolExecution {
    let mut text = format!("# Skill: {}\n", skill.name);
    if let Some(allowed) = skill.allowed_tools.as_deref() {
        if !allowed.trim().is_empty() {
            text.push_str(&format!("Allowed tools: {allowed}\n"));
        }
    }
    if let Some(args) = args {
        text.push_str(&format!("Args: {args}\n"));
    }
    text.push('\n');
    text.push_str(skill.body.trim());
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: false,
    }
    .into()
}

/// "Unknown skill" — `is_error: true` so the model treats it as a
/// retry-able failure, with the available names appended so the next
/// call uses a real name.
fn unknown_skill_output(msg: &str, available: &[String]) -> ToolExecution {
    let mut text = msg.to_string();
    if !available.is_empty() {
        text.push_str(" Available skill names: ");
        text.push_str(&available.join(", "));
        text.push('.');
    } else {
        text.push_str(" No skills are active for this session.");
    }
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error: true,
    }
    .into()
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code_skills::{parse_skill_md, SkillSource};

    fn make_skill(name: &str, body: &str, allowed: Option<&str>) -> AgentSkill {
        let front = match allowed {
            Some(a) => format!(
                "---\nname: {name}\ndescription: {name} skill.\nallowed-tools: {a}\nmetadata:\n  libertai.pillars: code\n---\n"
            ),
            None => format!(
                "---\nname: {name}\ndescription: {name} skill.\nmetadata:\n  libertai.pillars: code\n---\n"
            ),
        };
        parse_skill_md(&(front + body), Some(name), SkillSource::Builtin).expect("parse")
    }

    #[test]
    fn loaded_output_includes_name_allowed_tools_and_body() {
        let skill = make_skill(
            "demo-skill",
            "# Demo\nDo the thing with {{args}}.",
            Some("read fetch"),
        );
        let out = loaded_output(&skill, Some("the-thing"));
        let ToolExecution::Done(tool_output) = out else {
            panic!("expected Done");
        };
        assert!(!tool_output.is_error);
        let text = tool_output
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text,
                _ => String::new(),
            })
            .collect::<String>();
        assert!(text.contains("# Skill: demo-skill"), "name header: {text:?}");
        assert!(text.contains("Allowed tools: read fetch"), "allowed: {text:?}");
        assert!(text.contains("Args: the-thing"), "args forwarded: {text:?}");
        assert!(text.contains("Do the thing"), "body present: {text:?}");
    }

    #[test]
    fn loaded_output_omits_empty_allowed_tools_and_args() {
        let skill = make_skill("demo-skill", "# Demo\nBody.", None);
        let out = loaded_output(&skill, None);
        let ToolExecution::Done(tool_output) = out else {
            panic!("expected Done");
        };
        let text = tool_output
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text,
                _ => String::new(),
            })
            .collect::<String>();
        assert!(!text.contains("Allowed tools"));
        assert!(!text.contains("Args:"));
        assert!(text.contains("Body."));
    }

    #[test]
    fn unknown_skill_output_is_error_and_lists_names() {
        let out = unknown_skill_output("No active skill named `bogus`.", &[
            "libertai-harness".to_string(),
            "libertai-code-workflow".to_string(),
        ]);
        let ToolExecution::Done(tool_output) = out else {
            panic!("expected Done");
        };
        assert!(tool_output.is_error, "unknown skill must be an error");
        let text = tool_output
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text,
                _ => String::new(),
            })
            .collect::<String>();
        assert!(text.contains("No active skill named `bogus`"));
        assert!(text.contains("libertai-harness"));
        assert!(text.contains("libertai-code-workflow"));
    }

    #[test]
    fn unknown_skill_output_with_no_skills_says_so() {
        let out = unknown_skill_output("No active skill named `bogus`.", &[]);
        let ToolExecution::Done(tool_output) = out else {
            panic!("expected Done");
        };
        assert!(tool_output.is_error);
        let text = tool_output
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text,
                _ => String::new(),
            })
            .collect::<String>();
        assert!(text.contains("No skills are active"));
    }

    #[test]
    fn load_skill_by_name_resolves_active_builtin_and_rejects_unknown() {
        // libertai-harness ships with `libertai.pillars: any` so it's
        // active for the Code pillar; an unknown name resolves to None.
        // Held under the config lock: the parallel `rejects_disabled`
        // test disables harness mid-run, which would make this `.expect`
        // panic on None.
        let _lock = code_skills::SKILLS_CONFIG_TEST_LOCK
            .lock()
            .expect("skills config test lock");
        let cwd = std::env::current_dir().ok();
        let harness =
            code_skills::load_skill_by_name(SkillPillar::Code, cwd.as_deref(), "libertai-harness")
                .expect("load")
                .expect("harness is active for code");
        assert_eq!(harness.name, "libertai-harness");
        assert!(harness.body.contains("## Auto memory"), "harness body: missing auto-memory section");

        let none =
            code_skills::load_skill_by_name(SkillPillar::Code, cwd.as_deref(), "no-such-skill")
                .expect("load");
        assert!(none.is_none(), "unknown skill must resolve to None");
    }

    #[test]
    fn load_skill_by_name_rejects_disabled_skill() {
        // Disable a real builtin, confirm load_skill_by_name hides it,
        // then re-enable so other tests aren't polluted. The disabled
        // list lives in the libertai config dir (process-global state),
        // so we hold the shared config lock for the whole test body — a
        // parallel sibling asserting "harness is in the prompt" would
        // otherwise see harness missing while the DisabledGuard holds.
        let _lock = code_skills::SKILLS_CONFIG_TEST_LOCK
            .lock()
            .expect("skills config test lock");
        let target = "libertai-chat-research";
        // It's only active for the chat pillar, so for Code it's already
        // not loadable — assert that as the baseline, then confirm the
        // helper honors the disabled set by disabling a code-active skill.
        let cwd = std::env::current_dir().ok();
        assert!(
            code_skills::load_skill_by_name(SkillPillar::Code, cwd.as_deref(), target)
                .expect("load")
                .is_none(),
            "chat-research is not a code-pillar skill"
        );
        // libertai-harness IS code-active; disable + reload to prove the
        // helper honors the disabled list.
        let _guard = DisabledGuard::new("libertai-harness");
        assert!(
            code_skills::load_skill_by_name(SkillPillar::Code, cwd.as_deref(), "libertai-harness")
                .expect("load")
                .is_none(),
            "disabled skill must not be loadable"
        );
    }

    /// Toggle a skill disabled for the duration of a test, restoring it
    /// on drop so the process-global disabled list isn't polluted.
    struct DisabledGuard(&'static str);
    impl DisabledGuard {
        fn new(name: &'static str) -> Self {
            let _ = code_skills::set_skill_enabled(name, false);
            Self(name)
        }
    }
    impl Drop for DisabledGuard {
        fn drop(&mut self) {
            let _ = code_skills::set_skill_enabled(self.0, true);
        }
    }
}
