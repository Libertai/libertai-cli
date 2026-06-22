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

// ---------------------------------------------------------------------------
// Agent color rotation
// ---------------------------------------------------------------------------

/// Rotation of colors for agent identity, matching the existing
/// `AgentColor` palette in `code_team.rs`.
pub const AGENT_COLORS: [Color; 9] = [
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Rgb(216, 144, 60), // orange
    Color::Rgb(220, 120, 180), // pink
    Color::DarkGray,
];

/// Map an agent color index to a ratatui `Color`.
pub fn agent_color(index: usize) -> Color {
    AGENT_COLORS[index % AGENT_COLORS.len()]
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
pub const SPINNER_FRAMES: [&str; 10] = [
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

/// Tick rate for the event loop (and spinner animation).
pub const TICK_RATE_MS: u64 = 80;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_color_rotates() {
        assert_eq!(agent_color(0), Color::Red);
        assert_eq!(agent_color(1), Color::Green);
        assert_eq!(agent_color(9), Color::Red); // wraps
        assert_eq!(agent_color(10), Color::Green);
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
