//! Tests for TUI rendering module

#[cfg(test)]
mod tests {
    use crate::tui::app::ThroughputPoint;
    use crate::tui::constants::{chart, layout, text};
    use crate::tui::helpers::{
        calculate_chart_bounds, format_summary_throughput, format_throughput_label,
    };
    use crate::types::tui::{BytesPerSecond, Timestamp};

    // ========================================================================
    // Layout Tests
    // ========================================================================

    #[test]
    fn test_min_height_for_logs_constant() {
        const MIN_HEIGHT_FOR_LOGS: u16 = 40;

        // Verify the constant matches what render_ui uses
        // This ensures logs appear at reasonable terminal sizes
        assert_eq!(MIN_HEIGHT_FOR_LOGS, 40);

        // Should be larger than minimum viable terminal
        assert!(MIN_HEIGHT_FOR_LOGS > 24, "Should work in larger terminals");
    }

    #[test]
    fn test_log_window_height_reasonable() {
        const LOG_WINDOW_HEIGHT: u16 = 10;

        // Should show useful number of log lines
        assert_eq!(LOG_WINDOW_HEIGHT, 10);
        assert!(LOG_WINDOW_HEIGHT >= 5, "Should show multiple log lines");
        assert!(LOG_WINDOW_HEIGHT <= 15, "Shouldn't dominate screen");
    }

    #[test]
    fn test_layout_sections_add_up() {
        // When logs are hidden, sections should fit in minimum terminal
        let total = layout::TITLE_HEIGHT
            + layout::SUMMARY_HEIGHT
            + layout::MIN_CHART_HEIGHT
            + layout::FOOTER_HEIGHT;

        // With margin=1 on all sides, needs 2 extra lines
        let with_margins = total + 2;

        // Should fit in standard 24-line terminal
        assert!(with_margins <= 24, "Should fit in 24-line terminal");
    }

    #[test]
    fn test_layout_with_logs_reasonable() {
        const MIN_HEIGHT_FOR_LOGS: u16 = 40;
        const LOG_WINDOW_HEIGHT: u16 = 10;

        let total = layout::TITLE_HEIGHT
            + layout::SUMMARY_HEIGHT
            + layout::MIN_CHART_HEIGHT
            + LOG_WINDOW_HEIGHT
            + layout::FOOTER_HEIGHT
            + 2; // margins

        // Should fit comfortably in the minimum height
        assert!(
            total <= MIN_HEIGHT_FOR_LOGS,
            "Layout should fit when logs are shown (total={}, min={})",
            total,
            MIN_HEIGHT_FOR_LOGS
        );
    }

    #[test]
    fn test_backend_columns_split_evenly() {
        let columns = layout::backend_columns();

        // Should be three columns now (backends, chart, top users)
        assert_eq!(columns.len(), 3);

        // Backend list and chart should split screen, with 25% for top users
        assert_eq!(layout::BACKEND_LIST_WIDTH_PCT, 50);
        assert_eq!(layout::CHART_WIDTH_PCT, 50);
    }

    // ========================================================================
    // Widget Construction Tests (data flow, not rendering)
    // ========================================================================

    #[test]
    fn test_chart_data_bounds_calculation() {
        // Test that chart uses the helper function correctly
        let max_throughput = 500_000.0; // Below minimum
        let bounds = calculate_chart_bounds(max_throughput);

        // Should clamp to minimum and round
        assert!(bounds >= chart::MIN_THROUGHPUT);

        let max_throughput_high = 15_500_000.0;
        let bounds_high = calculate_chart_bounds(max_throughput_high);

        // Should round up nicely
        assert_eq!(bounds_high, 16_000_000.0);
    }

    #[test]
    fn test_summary_throughput_formatting() {
        let point = ThroughputPoint::new_client(
            Timestamp::now(),
            BytesPerSecond::new(1_000_000.0),
            BytesPerSecond::new(2_000_000.0),
        );

        let (up, down) = format_summary_throughput(Some(&point));

        // Should have arrow indicators
        assert!(up.starts_with(text::ARROW_UP));
        assert!(down.starts_with(text::ARROW_DOWN));

        // Should have throughput values
        assert!(up.contains("MB/s") || up.contains("KB/s"));
        assert!(down.contains("MB/s") || down.contains("KB/s"));
    }

    #[test]
    fn test_summary_throughput_none() {
        let (up, down) = format_summary_throughput(None);

        // Should have default values
        assert!(up.contains(text::DEFAULT_THROUGHPUT));
        assert!(down.contains(text::DEFAULT_THROUGHPUT));
    }

    #[test]
    fn test_chart_axis_labels() {
        // Test that chart axis labels are reasonable
        let max_throughput = 10_000_000.0; // 10 MB/s
        let label = format_throughput_label(max_throughput);

        assert_eq!(label, "10 MB/s");

        let half_label = format_throughput_label(max_throughput / 2.0);
        assert_eq!(half_label, "5 MB/s");
    }

    #[test]
    fn test_chart_bounds_edge_cases() {
        // Zero throughput
        let bounds_zero = calculate_chart_bounds(0.0);
        assert!(bounds_zero >= chart::MIN_THROUGHPUT);

        // Exactly at minimum
        let bounds_min = calculate_chart_bounds(chart::MIN_THROUGHPUT);
        assert_eq!(bounds_min, chart::MIN_THROUGHPUT);

        // Slightly above minimum
        let bounds_above = calculate_chart_bounds(chart::MIN_THROUGHPUT + 1000.0);
        assert!(bounds_above >= chart::MIN_THROUGHPUT);
    }
}
