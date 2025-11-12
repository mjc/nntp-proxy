//! TUI constants and configuration

use ratatui::style::Color;

// ============================================================================
// Layout Constants
// ============================================================================

/// Layout constraints for main UI sections
pub mod layout {
    use ratatui::layout::Constraint;

    pub const TITLE_HEIGHT: u16 = 3;
    pub const SUMMARY_HEIGHT: u16 = 5;
    pub const FOOTER_HEIGHT: u16 = 3;
    pub const MIN_CHART_HEIGHT: u16 = 10;

    pub const BACKEND_LIST_WIDTH_PCT: u16 = 50;
    pub const CHART_WIDTH_PCT: u16 = 50;

    pub fn main_sections() -> [Constraint; 4] {
        [
            Constraint::Length(TITLE_HEIGHT),
            Constraint::Length(SUMMARY_HEIGHT),
            Constraint::Min(MIN_CHART_HEIGHT),
            Constraint::Length(FOOTER_HEIGHT),
        ]
    }

    pub fn backend_columns() -> [Constraint; 2] {
        [
            Constraint::Percentage(BACKEND_LIST_WIDTH_PCT),
            Constraint::Percentage(CHART_WIDTH_PCT),
        ]
    }
}

// ============================================================================
// Chart Configuration
// ============================================================================

/// Chart configuration
pub mod chart {
    pub const HISTORY_POINTS: f64 = 60.0;
    pub const MIN_THROUGHPUT: f64 = 1_000_000.0; // 1 MB/s

    pub const X_LABEL_0S: &str = "0s";
    pub const X_LABEL_5S: &str = "5s";
    pub const X_LABEL_10S: &str = "10s";
    pub const X_LABEL_15S: &str = "15s";

    pub const Y_LABEL_ZERO: &str = "0";
    pub const TITLE: &str = "Throughput (15s)";
}

// ============================================================================
// Color Palette
// ============================================================================

/// Color palette for backends
pub const BACKEND_COLORS: &[Color] = &[
    Color::Green,
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Red,
    Color::Blue,
];

/// Status colors
pub mod status {
    use ratatui::style::Color;

    pub const ACTIVE: Color = Color::Green;
    pub const INACTIVE: Color = Color::Gray;
    pub const ERROR: Color = Color::Red;
    #[allow(dead_code)] // May be used in future features
    pub const WARNING: Color = Color::Yellow;
}

/// UI text styles
pub mod styles {
    use ratatui::style::Color;

    pub const LABEL: Color = Color::Gray;
    pub const VALUE_PRIMARY: Color = Color::Green;
    pub const VALUE_SECONDARY: Color = Color::Yellow;
    pub const VALUE_INFO: Color = Color::Cyan;
    pub const VALUE_NEUTRAL: Color = Color::Blue;
    pub const BORDER_ACTIVE: Color = Color::Cyan;
    pub const BORDER_NORMAL: Color = Color::White;
}

// ============================================================================
// Text Constants
// ============================================================================

/// UI text constants
pub mod text {
    pub const STATUS_INDICATOR: &str = "● ";
    pub const ARROW_UP: &str = "↑";
    pub const ARROW_DOWN: &str = "↓";
    pub const WARNING_ICON: &str = " ⚠ ";
    pub const DEFAULT_CMD_RATE: &str = "0.0";
    #[allow(dead_code)] // Used in default values
    pub const DEFAULT_THROUGHPUT: &str = "0 B/s";
}

// ============================================================================
// Throughput Formatting Constants
// ============================================================================

/// Constants for throughput calculations and formatting
pub mod throughput {
    pub const HUNDRED_MB: f64 = 100_000_000.0;
    pub const TEN_MB: f64 = 10_000_000.0;
    pub const ONE_MB: f64 = 1_000_000.0;
    pub const HUNDRED_KB: f64 = 100_000.0;
    pub const TEN_KB: f64 = 10_000.0;
    pub const ONE_KB: f64 = 1_000.0;
}
