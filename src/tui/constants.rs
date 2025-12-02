//! TUI constants and configuration

use ratatui::style::Color;

// ============================================================================
// Layout Constants
// ============================================================================

/// Layout constraints for main UI sections
pub mod layout {
    use ratatui::layout::Constraint;

    pub const TITLE_HEIGHT: u16 = 3;
    pub const SUMMARY_HEIGHT: u16 = 7; // Increased to fit cache stats
    pub const FOOTER_HEIGHT: u16 = 3;
    pub const MIN_CHART_HEIGHT: u16 = 8; // Reduced to fit cache stats in summary

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

    pub fn backend_columns() -> [Constraint; 3] {
        [
            Constraint::Percentage(BACKEND_LIST_WIDTH_PCT),
            Constraint::Percentage(CHART_WIDTH_PCT),
            Constraint::Percentage(25), // Top users column
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
    pub const ARROW_UP: &str = "↑";
    pub const ARROW_DOWN: &str = "↓";
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_constants() {
        assert_eq!(layout::TITLE_HEIGHT, 3);
        assert_eq!(layout::SUMMARY_HEIGHT, 7);
        assert_eq!(layout::FOOTER_HEIGHT, 3);
        assert_eq!(layout::MIN_CHART_HEIGHT, 8);
        assert_eq!(layout::BACKEND_LIST_WIDTH_PCT, 50);
        assert_eq!(layout::CHART_WIDTH_PCT, 50);
    }

    #[test]
    fn test_main_sections_constraints() {
        let sections = layout::main_sections();
        assert_eq!(sections.len(), 4);
        // Verify structure is correct
        assert!(matches!(
            sections[0],
            ratatui::layout::Constraint::Length(_)
        ));
    }

    #[test]
    fn test_backend_columns_constraints() {
        let columns = layout::backend_columns();
        assert_eq!(columns.len(), 3);
        // Verify structure is correct
        assert!(matches!(
            columns[0],
            ratatui::layout::Constraint::Percentage(_)
        ));
    }

    #[test]
    fn test_chart_constants() {
        assert_eq!(chart::HISTORY_POINTS, 60.0);
        assert_eq!(chart::MIN_THROUGHPUT, 1_000_000.0);
        assert_eq!(chart::X_LABEL_0S, "0s");
        assert_eq!(chart::X_LABEL_5S, "5s");
        assert_eq!(chart::X_LABEL_10S, "10s");
        assert_eq!(chart::X_LABEL_15S, "15s");
        assert_eq!(chart::Y_LABEL_ZERO, "0");
        assert_eq!(chart::TITLE, "Throughput (15s)");
    }

    #[test]
    fn test_backend_colors() {
        assert_eq!(BACKEND_COLORS.len(), 6);
        assert_eq!(BACKEND_COLORS[0], Color::Green);
        assert_eq!(BACKEND_COLORS[1], Color::Cyan);
        assert_eq!(BACKEND_COLORS[2], Color::Yellow);
        assert_eq!(BACKEND_COLORS[3], Color::Magenta);
        assert_eq!(BACKEND_COLORS[4], Color::Red);
        assert_eq!(BACKEND_COLORS[5], Color::Blue);
    }

    #[test]
    fn test_status_colors() {
        assert_eq!(status::WARNING, Color::Yellow);
    }

    #[test]
    fn test_style_colors() {
        assert_eq!(styles::LABEL, Color::Gray);
        assert_eq!(styles::VALUE_PRIMARY, Color::Green);
        assert_eq!(styles::VALUE_SECONDARY, Color::Yellow);
        assert_eq!(styles::VALUE_INFO, Color::Cyan);
        assert_eq!(styles::VALUE_NEUTRAL, Color::Blue);
        assert_eq!(styles::BORDER_ACTIVE, Color::Cyan);
        assert_eq!(styles::BORDER_NORMAL, Color::White);
    }

    #[test]
    fn test_text_constants() {
        assert_eq!(text::ARROW_UP, "↑");
        assert_eq!(text::ARROW_DOWN, "↓");
        assert_eq!(text::DEFAULT_CMD_RATE, "0.0");
        assert_eq!(text::DEFAULT_THROUGHPUT, "0 B/s");
    }

    #[test]
    fn test_throughput_constants() {
        assert_eq!(throughput::HUNDRED_MB, 100_000_000.0);
        assert_eq!(throughput::TEN_MB, 10_000_000.0);
        assert_eq!(throughput::ONE_MB, 1_000_000.0);
        assert_eq!(throughput::HUNDRED_KB, 100_000.0);
        assert_eq!(throughput::TEN_KB, 10_000.0);
        assert_eq!(throughput::ONE_KB, 1_000.0);
    }

    #[test]
    fn test_throughput_constants_ordering() {
        // Verify constants are in descending order
        assert!(throughput::HUNDRED_MB > throughput::TEN_MB);
        assert!(throughput::TEN_MB > throughput::ONE_MB);
        assert!(throughput::ONE_MB > throughput::HUNDRED_KB);
        assert!(throughput::HUNDRED_KB > throughput::TEN_KB);
        assert!(throughput::TEN_KB > throughput::ONE_KB);
    }

    #[test]
    fn test_throughput_relationships() {
        // Verify mathematical relationships
        assert_eq!(throughput::HUNDRED_MB, throughput::TEN_MB * 10.0);
        assert_eq!(throughput::TEN_MB, throughput::ONE_MB * 10.0);
        assert_eq!(throughput::ONE_MB, throughput::HUNDRED_KB * 10.0);
        assert_eq!(throughput::HUNDRED_KB, throughput::TEN_KB * 10.0);
        assert_eq!(throughput::TEN_KB, throughput::ONE_KB * 10.0);
    }
}
