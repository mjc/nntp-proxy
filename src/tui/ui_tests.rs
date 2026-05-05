//! Tests for TUI rendering module

#[cfg(test)]
mod tests {
    use crate::tui::app::ThroughputPoint;
    use crate::tui::constants::{chart, layout, text};
    use crate::tui::helpers::{
        calculate_chart_bounds, format_summary_throughput, format_throughput_label,
    };
    use crate::types::tui::{Throughput, Timestamp};

    fn assert_f64_eq(actual: f64, expected: f64) {
        assert_eq!(actual.to_bits(), expected.to_bits());
    }

    // ========================================================================
    // Layout Tests
    // ========================================================================

    #[test]
    fn test_min_height_for_logs_constant() {
        // Verify constraints on the log panel threshold
        const { assert!(layout::MIN_HEIGHT_FOR_LOGS > 24) };
    }

    #[test]
    fn test_log_window_height_reasonable() {
        const { assert!(layout::LOG_WINDOW_HEIGHT >= 5) };
        const { assert!(layout::LOG_WINDOW_HEIGHT <= 15) };
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
        let total = layout::TITLE_HEIGHT
            + layout::SUMMARY_HEIGHT
            + layout::MIN_CHART_HEIGHT
            + layout::LOG_WINDOW_HEIGHT
            + layout::FOOTER_HEIGHT
            + 2; // margins

        // Should fit comfortably in the minimum height
        assert!(
            total <= layout::MIN_HEIGHT_FOR_LOGS,
            "Layout should fit when logs are shown (total={}, min={})",
            total,
            layout::MIN_HEIGHT_FOR_LOGS,
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
        assert_f64_eq(bounds_high, 15_728_640.0);
    }

    #[test]
    fn test_summary_throughput_formatting() {
        let point = ThroughputPoint::new_client(
            Timestamp::now(),
            Throughput::new(1_000_000.0),
            Throughput::new(2_000_000.0),
        );

        let (up, down) = format_summary_throughput(Some(&point));

        // Should have arrow indicators
        assert!(up.starts_with(text::ARROW_UP));
        assert!(down.starts_with(text::ARROW_DOWN));

        // Should have throughput values
        assert!(up.contains("MiB/s") || up.contains("KiB/s"));
        assert!(down.contains("MiB/s") || down.contains("KiB/s"));
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
        let max_throughput = 10_485_760.0; // 10 MiB/s
        let label = format_throughput_label(max_throughput);

        assert_eq!(label, "10 MiB/s");

        let half_label = format_throughput_label(max_throughput / 2.0);
        assert_eq!(half_label, "5 MiB/s");
    }

    #[test]
    fn test_chart_bounds_edge_cases() {
        // Zero throughput
        let bounds_zero = calculate_chart_bounds(0.0);
        assert!(bounds_zero >= chart::MIN_THROUGHPUT);

        // Exactly at minimum
        let bounds_min = calculate_chart_bounds(chart::MIN_THROUGHPUT);
        assert_f64_eq(bounds_min, chart::MIN_THROUGHPUT);

        // Slightly above minimum
        let bounds_above = calculate_chart_bounds(chart::MIN_THROUGHPUT + 1000.0);
        assert!(bounds_above >= chart::MIN_THROUGHPUT);
    }
}
