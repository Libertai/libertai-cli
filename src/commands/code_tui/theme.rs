//! Semantic color palette and glyph system for the ratatui TUI.
//!
//! One accent (cyan), semantic colors for status, dim gray for
//! metadata. No gradients, no neon, no purple. Pure black/white
//! banned — use `Reset` (terminal default) for backgrounds.

use ratatui::style::{Color, Modifier, Style};

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

/// Brand accent — used for the prompt `❯`, tool markers, and brand elements.
pub const ACCENT: Color = Color::Cyan;

/// Success — completed agents, allow.
pub const SUCCESS: Color = Color::Green;

/// Warning — needs input, plan mode.
pub const WARNING: Color = Color::Yellow;

/// Error — failed agents, deny.
pub const ERROR: Color = Color::Red;

/// Informational.
pub const INFO: Color = Color::Blue;

/// Muted text — metadata, previews, dividers.
pub const MUTED: Color = Color::DarkGray;

/// Primary text — terminal default.
pub const PRIMARY: Color = Color::Reset;

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

/// Normal text — terminal default.
pub fn primary() -> Style {
    Style::default().fg(PRIMARY)
}

/// Dim/muted text — metadata, previews, dividers.
pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

/// Bold text — emphasis.
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Bold accent — the `❯` prompt, key UI elements.
pub fn bold_accent() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Accent (non-bold) — tool markers.
pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Dim accent — spinner text.
pub fn dim_accent() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::DIM)
}

/// Dim muted — faint secondary hints (e.g. the "esc to stop" suffix).
pub fn dim_muted() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::DIM)
}

/// Success — completed status.
pub fn success() -> Style {
    Style::default().fg(SUCCESS)
}

/// Warning — needs input.
pub fn warning() -> Style {
    Style::default().fg(WARNING)
}

/// Error — failed.
pub fn error() -> Style {
    Style::default().fg(ERROR)
}

/// Bold + muted — dim emphasis.
pub fn bold_muted() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
}

/// Inline code — muted + bold, distinct from the accent+BOLD `❯` prompt
/// so backtick spans read as code rather than echoing the prompt glyph.
pub fn code() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
}

/// Markdown heading style by level: H1 bold accent, H2 bold, H3+ bold muted.
/// Reuses existing styles — no new colors.
pub fn heading(level: usize) -> Style {
    match level {
        1 => bold_accent(),
        2 => bold(),
        _ => bold_muted(),
    }
}

// ---------------------------------------------------------------------------
// Agent color rotation
// ---------------------------------------------------------------------------

/// Map an `AgentColor` to a ratatui `Color`. Single source of truth —
/// used by both the agents panel and any other widget that needs the
/// mapping.
pub fn agent_color_for(color: crate::commands::code_team::AgentColor) -> Color {
    use crate::commands::code_team::AgentColor;
    match color {
        AgentColor::Red => Color::Red,
        AgentColor::Green => Color::Green,
        AgentColor::Yellow => Color::Yellow,
        AgentColor::Blue => Color::Blue,
        AgentColor::Purple => Color::Magenta,
        AgentColor::Cyan => Color::Cyan,
        AgentColor::Orange => Color::Rgb(216, 144, 60),
        AgentColor::Pink => Color::Rgb(220, 120, 180),
        AgentColor::Dim => Color::DarkGray,
    }
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

/// Status icons — single-cell width, consistent weight.
pub mod glyph {
    use crate::commands::code_team::AgentStatus;

    pub const SPAWNING: &str = "○";
    pub const WORKING: &str = "✽";
    pub const NEEDS_INPUT: &str = "⏸";
    pub const IDLE: &str = "∙";
    pub const COMPLETED: &str = "✓";
    /// Unchecked task-list box (M4b markdown `- [ ]` items).
    pub const UNCHECKED: &str = "☐";
    pub const FAILED: &str = "✗";
    pub const STOPPED: &str = "⊘";

    /// Status icon for an agent status.
    pub fn status_icon(status: AgentStatus) -> &'static str {
        match status {
            AgentStatus::Spawning => SPAWNING,
            AgentStatus::Working => WORKING,
            AgentStatus::NeedsInput => NEEDS_INPUT,
            AgentStatus::Idle => IDLE,
            AgentStatus::Completed => COMPLETED,
            AgentStatus::Failed => FAILED,
            AgentStatus::Stopped => STOPPED,
        }
    }

    pub const ASSISTANT_MARKER: &str = "●";
    pub const TOOL_MARKER: &str = "●";
    pub const USER_PROMPT: &str = "❯";
    pub const QUEUED: &str = "›";
    pub const READ_WRITE_CAP: &str = "✎";
    pub const DIVIDER: char = '─';
}

// ---------------------------------------------------------------------------
// Spinner
// ---------------------------------------------------------------------------

/// Braille spinner frames, 80ms tick — smoother than the old 120ms.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Tick rate for the event loop (and spinner animation).
pub const TICK_RATE_MS: u64 = 80;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_color_for_maps_all_variants() {
        use crate::commands::code_team::AgentColor;
        assert_eq!(agent_color_for(AgentColor::Red), Color::Red);
        assert_eq!(agent_color_for(AgentColor::Green), Color::Green);
        assert_eq!(agent_color_for(AgentColor::Purple), Color::Magenta);
        assert_eq!(agent_color_for(AgentColor::Dim), Color::DarkGray);
        assert_eq!(
            agent_color_for(AgentColor::Orange),
            Color::Rgb(216, 144, 60)
        );
    }

    #[test]
    fn status_icon_matches_status() {
        use crate::commands::code_team::AgentStatus;
        assert_eq!(glyph::status_icon(AgentStatus::Working), "✽");
        assert_eq!(glyph::status_icon(AgentStatus::Completed), "✓");
        assert_eq!(glyph::status_icon(AgentStatus::Failed), "✗");
    }

    #[test]
    fn spinner_frames_non_empty() {
        assert_eq!(SPINNER_FRAMES.len(), 10);
        assert!(SPINNER_FRAMES.iter().all(|s| !s.is_empty()));
    }
}
