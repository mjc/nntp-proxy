//! TUI rendering helper functions

use ratatui::style::Color;

use super::constants::{BACKEND_COLORS, throughput};
use super::types::{
    BackendChartData, BackendIndex, ChartDataVec, ChartPoint, ChartX, ChartY, PointVec,
};
use crate::config::Server;
use crate::tui::TuiApp;
use crate::tui::app::ThroughputPoint;

// ============================================================================
// Sparkline Rendering
// ============================================================================

/// Width of bandwidth sparkline bars in characters
const SPARKLINE_WIDTH: usize = 15;

/// Create a text-based sparkline bar visualization
///
/// Creates a bar using filled (█) and empty (░) characters scaled
/// relative to the maximum value.
///
/// # Arguments
/// * `value` - The value to visualize
/// * `max_value` - The maximum value for scaling (0-100% range)
///
/// # Returns
/// A string of length `SPARKLINE_WIDTH` with filled/empty blocks
#[must_use]
pub fn create_sparkline(value: u64, max_value: u64) -> String {
    let filled = if max_value > 0 {
        ((value as f64 / max_value as f64) * SPARKLINE_WIDTH as f64) as usize
    } else {
        0
    };

    "█".repeat(filled.min(SPARKLINE_WIDTH)) + &"░".repeat(SPARKLINE_WIDTH.saturating_sub(filled))
}

// ============================================================================
// Chart Data Building
// ============================================================================

/// Build chart data for all backends using functional pipeline
///
/// Single-pass processing:
/// 1. Maps over servers to extract history
/// 2. Folds over history points to build point vectors and track max
/// 3. Accumulates results with global max tracking
///
/// Returns (chart_data, global_max_throughput)
pub fn build_chart_data(servers: &[Server], app: &TuiApp) -> (ChartDataVec, f64) {
    /// Extract data from a single throughput point
    #[inline]
    fn extract_point_data(idx: usize, point: &ThroughputPoint) -> (ChartPoint, ChartPoint, ChartY) {
        let x = ChartX::from(idx);
        let sent = ChartY::from(point.sent_per_sec().get());
        let recv = ChartY::from(point.received_per_sec().get());
        (
            ChartPoint::new(x, sent),
            ChartPoint::new(x, recv),
            sent.max(recv),
        )
    }

    /// Accumulate points and track maximum (pure function)
    #[inline]
    fn accumulate_points(
        ((mut sent_vec, mut recv_vec), max): ((PointVec, PointVec), ChartY),
        (sent_point, recv_point, point_max): (ChartPoint, ChartPoint, ChartY),
    ) -> ((PointVec, PointVec), ChartY) {
        sent_vec.push(sent_point);
        recv_vec.push(recv_point);
        ((sent_vec, recv_vec), max.max(point_max))
    }

    /// Build chart data for a single backend (pure function)
    fn build_backend_data(
        index: usize,
        server: &Server,
        history: &std::collections::VecDeque<ThroughputPoint>,
    ) -> (BackendChartData, f64) {
        let ((sent_points, recv_points), backend_max) = history
            .iter()
            .enumerate()
            .map(|(idx, point)| extract_point_data(idx, point))
            .fold(
                ((PointVec::new(), PointVec::new()), ChartY::from(0.0)),
                accumulate_points,
            );

        (
            BackendChartData::new(
                server.name.as_str().to_string(),
                backend_color(BackendIndex::from(index)),
                sent_points,
                recv_points,
            ),
            backend_max.get(),
        )
    }

    // Functional pipeline: enumerate → map → fold
    servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            let history = app.throughput_history(index);
            build_backend_data(index, server, history)
        })
        .fold(
            (ChartDataVec::new(), 0.0_f64),
            |(mut chart_data, global_max), (backend_data, backend_max)| {
                chart_data.push(backend_data);
                (chart_data, global_max.max(backend_max))
            },
        )
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Get color for backend by index (round-robin through palette)
#[inline]
#[must_use]
pub fn backend_color(index: BackendIndex) -> Color {
    BACKEND_COLORS[index.get() % BACKEND_COLORS.len()]
}

/// Round throughput value up to next nice number for chart axis
///
/// Uses sensible rounding based on magnitude:
/// - > 100 MB/s → round to nearest 10 MB/s
/// - > 10 MB/s → round to nearest 1 MB/s  
/// - > 1 MB/s → round to nearest 100 KB/s
/// - Otherwise → round to nearest 10 KB/s
#[must_use]
pub fn round_up_throughput(value: f64) -> f64 {
    if value > throughput::HUNDRED_MB {
        (value / throughput::TEN_MB).ceil() * throughput::TEN_MB
    } else if value > throughput::TEN_MB {
        (value / throughput::ONE_MB).ceil() * throughput::ONE_MB
    } else if value > throughput::ONE_MB {
        (value / throughput::HUNDRED_KB).ceil() * throughput::HUNDRED_KB
    } else {
        (value / throughput::TEN_KB).ceil() * throughput::TEN_KB
    }
}

/// Format throughput value for axis label
///
/// Returns human-readable string like "10 MB/s", "500 KB/s", "100 B/s"
#[must_use]
pub fn format_throughput_label(value: f64) -> String {
    if value >= throughput::ONE_MB {
        format!("{:.0} MB/s", value / throughput::ONE_MB)
    } else if value >= throughput::ONE_KB {
        format!("{:.0} KB/s", value / throughput::ONE_KB)
    } else {
        format!("{:.0} B/s", value)
    }
}

// ============================================================================
// Summary Helpers
// ============================================================================

/// Format throughput strings for summary display
#[must_use]
pub fn format_summary_throughput(latest_throughput: Option<&ThroughputPoint>) -> (String, String) {
    use super::constants::text;

    latest_throughput
        .map(|point| {
            (
                format!("{}{}", text::ARROW_UP, point.sent_per_sec()),
                format!("{}{}", text::ARROW_DOWN, point.received_per_sec()),
            )
        })
        .unwrap_or_else(|| {
            (
                format!("{}{}", text::ARROW_UP, text::DEFAULT_THROUGHPUT),
                format!("{}{}", text::ARROW_DOWN, text::DEFAULT_THROUGHPUT),
            )
        })
}

// ============================================================================
// Backend Display Helpers
// ============================================================================

/// Health status icon and color
#[must_use]
pub const fn health_indicator(
    status: crate::metrics::BackendHealthStatus,
) -> (&'static str, Color) {
    use crate::metrics::BackendHealthStatus;
    match status {
        BackendHealthStatus::Healthy => ("●", Color::Green),
        BackendHealthStatus::Degraded => ("◐", Color::Yellow),
        BackendHealthStatus::Down => ("○", Color::Red),
    }
}

/// Format error rate with warning icon if critical
#[must_use]
pub fn format_error_rate(rate: f64) -> String {
    match rate {
        r if r > 5.0 => format!(" ⚠ {:.1}%", r),
        r if r > 0.0 => format!(" {:.1}%", r),
        _ => String::new(),
    }
}

/// Color for error rate display
#[must_use]
pub const fn error_rate_color(rate: f64) -> Color {
    if rate > 5.0 {
        Color::Red
    } else {
        Color::Yellow
    }
}

/// Color for load percentage (pending/capacity ratio)
#[must_use]
pub const fn load_percentage_color(percent: f64) -> Color {
    use super::constants::styles;
    match percent {
        p if p > 90.0 => Color::Red,
        p if p > 70.0 => Color::Yellow,
        _ => styles::VALUE_NEUTRAL,
    }
}

/// Color for pending count
#[must_use]
pub const fn pending_count_color(pending: usize) -> Color {
    if pending > 0 {
        Color::Yellow
    } else {
        super::constants::styles::VALUE_NEUTRAL
    }
}

/// Color for error count
#[must_use]
pub const fn error_count_color(has_errors: bool) -> Color {
    use super::constants::styles;
    if has_errors {
        Color::Yellow
    } else {
        styles::VALUE_NEUTRAL
    }
}

/// Color for connection failures
#[must_use]
pub const fn connection_failure_color(failures: u64) -> Color {
    use super::constants::styles;
    if failures > 0 {
        Color::Red
    } else {
        styles::VALUE_NEUTRAL
    }
}

// ============================================================================
// Chart Helpers
// ============================================================================

/// Calculate chart bounds (clamped and rounded for nice axis labels)
#[must_use]
pub fn calculate_chart_bounds(max_throughput: f64) -> f64 {
    use super::constants::chart;

    let clamped = max_throughput.max(chart::MIN_THROUGHPUT);
    round_up_throughput(clamped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_color_cycles() {
        let color0 = backend_color(BackendIndex::from(0));
        let color_wrap = backend_color(BackendIndex::from(BACKEND_COLORS.len()));
        assert_eq!(color0, color_wrap, "Should wrap around");
    }

    #[test]
    fn test_backend_color_distinct() {
        // First few backends should have distinct colors
        let color0 = backend_color(BackendIndex::from(0));
        let color1 = backend_color(BackendIndex::from(1));
        let color2 = backend_color(BackendIndex::from(2));

        assert_ne!(color0, color1);
        assert_ne!(color1, color2);
        assert_ne!(color0, color2);
    }

    #[test]
    fn test_round_up_throughput() {
        // > 100 MB/s → round to 10 MB/s
        assert_eq!(round_up_throughput(105_000_000.0), 110_000_000.0);

        // > 10 MB/s → round to 1 MB/s
        assert_eq!(round_up_throughput(15_500_000.0), 16_000_000.0);

        // > 1 MB/s → round to 100 KB/s
        assert_eq!(round_up_throughput(1_250_000.0), 1_300_000.0);

        // < 1 MB/s → round to 10 KB/s
        assert_eq!(round_up_throughput(55_000.0), 60_000.0);
    }

    #[test]
    fn test_round_up_throughput_exact_boundaries() {
        // Test exact boundary values
        assert_eq!(round_up_throughput(100_000_000.0), 100_000_000.0);
        assert_eq!(round_up_throughput(10_000_000.0), 10_000_000.0);
        assert_eq!(round_up_throughput(1_000_000.0), 1_000_000.0);
    }

    #[test]
    fn test_format_throughput_label() {
        assert_eq!(format_throughput_label(10_000_000.0), "10 MB/s");
        assert_eq!(format_throughput_label(500_000.0), "500 KB/s");
        assert_eq!(format_throughput_label(100.0), "100 B/s");
    }

    #[test]
    fn test_format_throughput_label_boundaries() {
        // Test boundary values
        assert_eq!(format_throughput_label(1_000_000.0), "1 MB/s");
        assert_eq!(format_throughput_label(1_000.0), "1 KB/s");
        assert_eq!(format_throughput_label(0.0), "0 B/s");
    }

    #[test]
    fn test_point_vec_type() {
        // Verify PointVec can hold typical history size on stack
        let mut points = PointVec::new();

        // Add 60 points (typical 60-second history)
        for i in 0..60 {
            points.push(ChartPoint::new(
                ChartX::from(i),
                ChartY::from((i * 1000) as f64),
            ));
        }

        assert_eq!(points.len(), 60);
        assert_eq!(points[0].x.get(), 0.0);
        assert_eq!(points[59].x.get(), 59.0);
    }

    #[test]
    fn test_chart_data_vec_type() {
        // Verify ChartDataVec can hold typical backend count on stack
        let mut chart_data = ChartDataVec::new();

        // Add 8 backends (at SmallVec capacity)
        for i in 0..8 {
            chart_data.push(BackendChartData::new(
                format!("Server {}", i),
                backend_color(BackendIndex::from(i)),
                PointVec::new(),
                PointVec::new(),
            ));
        }

        assert_eq!(chart_data.len(), 8);
        assert_eq!(chart_data[0].name, "Server 0");
        assert_eq!(chart_data[7].name, "Server 7");
    }

    #[test]
    fn test_backend_chart_data_structure() {
        let data = BackendChartData::new(
            "Test Server".to_string(),
            backend_color(BackendIndex::from(0)),
            PointVec::new(),
            PointVec::new(),
        );

        assert_eq!(data.name, "Test Server");
        // Verify pre-computed tuples are empty
        assert_eq!(data.sent_points_as_tuples().len(), 0);
        assert_eq!(data.recv_points_as_tuples().len(), 0);
    }

    // ========================================================================
    // Summary Tests
    // ========================================================================

    #[test]
    fn test_format_summary_throughput_none() {
        use super::super::constants::text;

        let (up, down) = format_summary_throughput(None);

        assert!(up.starts_with(text::ARROW_UP));
        assert!(down.starts_with(text::ARROW_DOWN));
        assert!(up.contains(text::DEFAULT_THROUGHPUT));
        assert!(down.contains(text::DEFAULT_THROUGHPUT));
    }

    #[test]
    fn test_format_summary_throughput_with_data() {
        use super::super::constants::text;
        use crate::types::tui::{CommandsPerSecond, Throughput, Timestamp};

        let point = ThroughputPoint::new_backend(
            Timestamp::now(),
            Throughput::new(1_000_000.0),
            Throughput::new(2_000_000.0),
            CommandsPerSecond::new(10.0),
        );

        let (up, down) = format_summary_throughput(Some(&point));

        assert!(up.starts_with(text::ARROW_UP));
        assert!(down.starts_with(text::ARROW_DOWN));
        // Should contain formatted throughput values
        assert!(up.contains("MB/s") || up.contains("KB/s") || up.contains("B/s"));
        assert!(down.contains("MB/s") || down.contains("KB/s") || down.contains("B/s"));
    }

    // ========================================================================
    // Sparkline Tests
    // ========================================================================

    #[test]
    fn test_create_sparkline_full() {
        let bar = create_sparkline(100, 100);
        assert_eq!(bar.len(), SPARKLINE_WIDTH * "█".len());
        assert_eq!(bar, "█".repeat(SPARKLINE_WIDTH));
    }

    #[test]
    fn test_create_sparkline_empty() {
        let bar = create_sparkline(0, 100);
        assert_eq!(bar.len(), SPARKLINE_WIDTH * "░".len());
        assert_eq!(bar, "░".repeat(SPARKLINE_WIDTH));
    }

    #[test]
    fn test_create_sparkline_half() {
        let bar = create_sparkline(50, 100);
        let expected_filled = SPARKLINE_WIDTH / 2;
        assert!(bar.starts_with(&"█".repeat(expected_filled)));
        assert!(bar.ends_with(&"░".repeat(SPARKLINE_WIDTH - expected_filled)));
    }

    #[test]
    fn test_create_sparkline_zero_max() {
        let bar = create_sparkline(50, 0);
        // Should be all empty when max is 0
        assert_eq!(bar, "░".repeat(SPARKLINE_WIDTH));
    }

    // ========================================================================
    // Chart Bounds Tests
    // ========================================================================

    #[test]
    fn test_calculate_chart_bounds_below_min() {
        use super::super::constants::chart;

        // Should clamp to MIN_THROUGHPUT and round
        let bounds = calculate_chart_bounds(500_000.0);
        assert!(bounds >= chart::MIN_THROUGHPUT);
    }

    #[test]
    fn test_calculate_chart_bounds_above_min() {
        // Should round up nicely
        let bounds = calculate_chart_bounds(15_500_000.0);
        assert_eq!(bounds, 16_000_000.0); // Rounded to 16 MB/s
    }

    #[test]
    fn test_calculate_chart_bounds_zero() {
        use super::super::constants::chart;

        // Zero should clamp to minimum
        let bounds = calculate_chart_bounds(0.0);
        assert_eq!(bounds, round_up_throughput(chart::MIN_THROUGHPUT));
    }
}
