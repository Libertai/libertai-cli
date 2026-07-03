//! (WF-F) Live workflow progress tree, pinned at the top of the footer.
//!
//! Renders every RUNNING workflow (plus a short completion flash after a
//! run settles) as a compact tree:
//!
//! ```text
//! ⠙ workflow "audit" · running · 42s
//!   ▸ phase "find" (2/3 done)
//!     ✓ wf:find/scan-1 · completed · 12s
//!     ⠙ wf:find/scan-2 · working · 40s
//! ```
//!
//! Data comes from a per-frame `WorkflowRegistry::snapshot()` — the same
//! pull-based pattern as the agents panel. `rows_needed` and `draw` derive
//! rows from the SAME snapshot the caller took once per frame, so height
//! and render never disagree (the FooterLayout lesson). The run loop keeps
//! the tick alive with a dirty poke while `active_count() > 0`, so the
//! spinner + elapsed stay live even when the app is Idle.

use std::sync::Arc;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::commands::code_team::AgentStatus;
use crate::commands::code_tui::theme;
use crate::commands::code_tui::theme::glyph;
use crate::commands::code_workflow::{WorkflowState, WorkflowStatus};

/// How long a settled run keeps its tree visible (the completion flash).
const COMPLETION_FLASH: std::time::Duration = std::time::Duration::from_secs(5);

/// The workflows the tree shows this frame: running, or settled within
/// the flash window.
fn visible(snapshot: &[Arc<WorkflowState>]) -> Vec<&Arc<WorkflowState>> {
    snapshot
        .iter()
        .filter(|w| w.status() == WorkflowStatus::Running || w.finished_within(COMPLETION_FLASH))
        .collect()
}

/// Natural (uncapped) height of the tree for this snapshot: per visible
/// workflow, one header row + one row per phase + one row per agent.
/// 0 when nothing is visible (the footer omits the panel entirely).
pub fn rows_needed(snapshot: &[Arc<WorkflowState>]) -> u16 {
    visible(snapshot)
        .iter()
        .map(|w| {
            let phases = w.phases.lock().unwrap();
            1 + phases.iter().map(|p| 1 + p.agents.len()).sum::<usize>()
        })
        .sum::<usize>() as u16
}

fn status_style(status: WorkflowStatus) -> ratatui::style::Style {
    match status {
        WorkflowStatus::Running => theme::accent(),
        WorkflowStatus::Completed => theme::success(),
        WorkflowStatus::Failed => theme::error(),
        WorkflowStatus::Stopped => theme::muted(),
    }
}

fn agent_status_style(status: AgentStatus) -> ratatui::style::Style {
    match status {
        AgentStatus::Spawning | AgentStatus::Idle | AgentStatus::Stopped => theme::muted(),
        AgentStatus::Working => theme::accent(),
        AgentStatus::NeedsInput => theme::warning(),
        AgentStatus::Completed => theme::success(),
        AgentStatus::Failed => theme::error(),
    }
}

fn fmt_secs(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Build every tree row for the snapshot (uncapped).
fn build_rows<'a>(snapshot: &[Arc<WorkflowState>], spinner_idx: usize) -> Vec<Line<'a>> {
    let mut lines: Vec<Line> = Vec::new();
    for w in visible(snapshot) {
        let status = w.status();
        let head_icon = if status == WorkflowStatus::Running {
            theme::SPINNER_FRAMES[spinner_idx % theme::SPINNER_FRAMES.len()].to_string()
        } else {
            glyph::status_icon(match status {
                WorkflowStatus::Completed => AgentStatus::Completed,
                WorkflowStatus::Failed => AgentStatus::Failed,
                _ => AgentStatus::Stopped,
            })
            .to_string()
        };
        let label = match status {
            WorkflowStatus::Running => "running",
            WorkflowStatus::Completed => "completed",
            WorkflowStatus::Failed => "failed",
            WorkflowStatus::Stopped => "stopped",
        };
        lines.push(Line::from(vec![
            Span::styled(head_icon, status_style(status)),
            Span::raw(" "),
            Span::styled(format!("workflow \"{}\"", w.name), theme::bold()),
            Span::styled(format!(" · {label}"), status_style(status)),
            Span::styled(format!(" · {}", fmt_secs(w.elapsed())), theme::muted()),
        ]));

        let phases = w.phases.lock().unwrap();
        for phase in phases.iter() {
            let done = phase
                .agents
                .iter()
                .filter(|a| !a.status().is_active())
                .count();
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("▸ ", theme::accent()),
                Span::styled(format!("phase \"{}\"", phase.title), theme::bold_muted()),
                Span::styled(
                    format!(" ({done}/{} done)", phase.agents.len()),
                    theme::muted(),
                ),
            ]));
            for agent in &phase.agents {
                let astatus = agent.status();
                let icon = if astatus == AgentStatus::Working {
                    theme::SPINNER_FRAMES[spinner_idx % theme::SPINNER_FRAMES.len()]
                } else {
                    glyph::status_icon(astatus)
                };
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(icon.to_string(), agent_status_style(astatus)),
                    Span::raw(" "),
                    Span::styled(
                        agent.name.clone(),
                        ratatui::style::Style::default().fg(theme::agent_color_for(agent.color)),
                    ),
                    Span::styled(
                        format!(
                            " · {} · {}",
                            status_word(astatus),
                            fmt_secs(agent.elapsed())
                        ),
                        theme::muted(),
                    ),
                ]));
            }
        }
    }
    lines
}

fn status_word(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Spawning => "spawning",
        AgentStatus::Working => "working",
        AgentStatus::NeedsInput => "needs input",
        AgentStatus::Idle => "idle",
        AgentStatus::Completed => "completed",
        AgentStatus::Failed => "failed",
        AgentStatus::Stopped => "stopped",
    }
}

/// Draw the tree into `area`. When the natural row count exceeds the
/// allotted height, rows are TOP-truncated so the newest-active edge (the
/// most recently spawned phases/agents at the bottom) stays visible.
pub fn draw(frame: &mut Frame, area: Rect, snapshot: &[Arc<WorkflowState>], spinner_idx: usize) {
    let rows = build_rows(snapshot, spinner_idx);
    let height = area.height as usize;
    let skip = rows.len().saturating_sub(height);
    let visible_rows: Vec<Line> = rows.into_iter().skip(skip).collect();
    frame.render_widget(Paragraph::new(visible_rows), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::code_team::{AgentColor, AgentKind, AgentRegistration, AgentRegistry};
    use crate::commands::code_workflow::WorkflowRegistry;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn state_with_phase() -> (
        Arc<crate::commands::code_workflow::WorkflowState>,
        Arc<AgentRegistry>,
    ) {
        let wreg = WorkflowRegistry::new();
        let areg = AgentRegistry::new();
        let s = wreg.register_for_test("wf-t-1".into(), "audit".into());
        let handle = areg.register(AgentRegistration {
            name: "wf:find/scan-1".into(),
            kind: AgentKind::Subagent {
                depth: 1,
                parent: None,
            },
            color: AgentColor::Red,
            capability: crate::commands::code_team::AgentCapability::ReadOnly,
            cwd: std::path::PathBuf::from("."),
            model: "m".into(),
            prompt_preview: String::new(),
            parent: None,
            pid: None,
            log_path: None,
        });
        handle.set_status(AgentStatus::Working);
        s.phases
            .lock()
            .unwrap()
            .push(crate::commands::code_workflow::PhaseProgress {
                title: "find".into(),
                agents: vec![handle],
            });
        (s, areg)
    }

    fn render(snapshot: &[Arc<crate::commands::code_workflow::WorkflowState>]) -> Vec<String> {
        let backend = TestBackend::new(60, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, f.area(), snapshot, 0)).unwrap();
        let buf = term.backend().buffer();
        (0..8)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// Running workflow: header + phase + agent rows, all visible.
    #[test]
    fn tree_renders_header_phase_and_agent_rows() {
        let (s, _areg) = state_with_phase();
        let snapshot = vec![s];
        assert_eq!(rows_needed(&snapshot), 3);
        let rows = render(&snapshot);
        let all = rows.join("\n");
        assert!(all.contains("workflow \"audit\""), "{all}");
        assert!(all.contains("running"), "{all}");
        assert!(all.contains("phase \"find\""), "{all}");
        assert!(all.contains("(0/1 done)"), "{all}");
        assert!(all.contains("wf:find/scan-1"), "{all}");
        assert!(all.contains("working"), "{all}");
    }

    /// A finished run stays visible during the flash window (finished_at
    /// just set → within 5s), and rows_needed drops to 0 only for runs
    /// outside it (not directly testable without clock control, so we pin
    /// the in-window behaviour).
    #[test]
    fn finished_run_flashes_then_counts() {
        let (s, _areg) = state_with_phase();
        s.set_status(WorkflowStatus::Completed);
        let snapshot = vec![s];
        assert_eq!(rows_needed(&snapshot), 3, "flash keeps the tree");
        let all = render(&snapshot).join("\n");
        assert!(all.contains("completed"), "{all}");
    }

    /// Nothing visible → zero rows (footer omits the panel).
    #[test]
    fn empty_snapshot_needs_no_rows() {
        assert_eq!(rows_needed(&[]), 0);
    }
}
