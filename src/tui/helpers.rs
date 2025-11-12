//! TUI rendering helper functions

use ratatui::style::Color;
use smallvec::SmallVec;

use super::constants::{BACKEND_COLORS, throughput};
use crate::config::ServerConfig;
use crate::tui::TuiApp;
use crate::tui::app::ThroughputPoint;

// ============================================================================
// Chart Data Types
// ============================================================================

/// Stack-allocated chart data for typical backend counts (up to 8 backends)
/// Most deployments have 1-4 backends, so this avoids heap allocation
pub type ChartDataVec = SmallVec<[BackendChartData; 8]>;

/// Stack-allocated point vectors for typical history sizes (60 points = 60 seconds)
type PointVec = SmallVec<[(f64, f64); 64]>;

/// Accumulator for building point vectors and tracking max in single fold
type PointAccumulator = ((PointVec, PointVec), f64);

/// Chart data for a single backend server
///
/// Pre-computed data points to avoid nested iterations during rendering
pub struct BackendChartData {
    /// Server name for legend
    pub name: String,
    /// Color for this backend's lines
    pub color: Color,
    /// Data points for sent throughput (x, y) where x is time index
    pub sent_points: PointVec,
    /// Data points for received throughput (x, y) where x is time index
    pub recv_points: PointVec,
}

/// Build chart data for all backends
///
/// Single-pass functional pipeline:
/// 1. Fold over servers to build chart data and track global max
/// 2. For each server, fold over history to build points and track backend max
///
/// Returns (chart_data, global_max_throughput)
pub fn build_chart_data(servers: &[ServerConfig], app: &TuiApp) -> (ChartDataVec, f64) {
    #[inline]
    fn extract_point_data(idx: usize, point: &ThroughputPoint) -> ((f64, f64), (f64, f64), f64) {
        let x = idx as f64;
        let sent = point.sent_per_sec().get();
        let recv = point.received_per_sec().get();
        ((x, sent), (x, recv), sent.max(recv))
    }

    #[inline]
    fn accumulate_points(
        ((mut sent_vec, mut recv_vec), max): PointAccumulator,
        (sent_point, recv_point, point_max): ((f64, f64), (f64, f64), f64),
    ) -> PointAccumulator {
        sent_vec.push(sent_point);
        recv_vec.push(recv_point);
        ((sent_vec, recv_vec), max.max(point_max))
    }

    servers.iter().enumerate().fold(
        (ChartDataVec::new(), 0.0_f64),
        |(mut chart_data, global_max), (index, server)| {
            let history = app.throughput_history(index);

            // Build point vectors and calculate max in single pass over history
            let ((sent_points, recv_points), backend_max) = history
                .iter()
                .enumerate()
                .map(|(idx, point)| extract_point_data(idx, point))
                .fold(
                    ((PointVec::new(), PointVec::new()), 0.0_f64),
                    accumulate_points,
                );

            chart_data.push(BackendChartData {
                name: server.name.as_str().to_string(),
                color: backend_color(index),
                sent_points,
                recv_points,
            });

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
pub fn backend_color(index: usize) -> Color {
    BACKEND_COLORS[index % BACKEND_COLORS.len()]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_color_cycles() {
        let color0 = backend_color(0);
        let color_wrap = backend_color(BACKEND_COLORS.len());
        assert_eq!(color0, color_wrap, "Should wrap around");
    }

    #[test]
    fn test_backend_color_distinct() {
        // First few backends should have distinct colors
        let color0 = backend_color(0);
        let color1 = backend_color(1);
        let color2 = backend_color(2);

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
            points.push((i as f64, (i * 1000) as f64));
        }

        assert_eq!(points.len(), 60);
        assert_eq!(points[0].0, 0.0);
        assert_eq!(points[59].0, 59.0);
    }

    #[test]
    fn test_chart_data_vec_type() {
        // Verify ChartDataVec can hold typical backend count on stack
        let mut chart_data = ChartDataVec::new();

        // Add 8 backends (at SmallVec capacity)
        for i in 0..8 {
            chart_data.push(BackendChartData {
                name: format!("Server {}", i),
                color: backend_color(i),
                sent_points: PointVec::new(),
                recv_points: PointVec::new(),
            });
        }

        assert_eq!(chart_data.len(), 8);
        assert_eq!(chart_data[0].name, "Server 0");
        assert_eq!(chart_data[7].name, "Server 7");
    }

    #[test]
    fn test_backend_chart_data_structure() {
        let data = BackendChartData {
            name: "Test Server".to_string(),
            color: backend_color(0),
            sent_points: PointVec::new(),
            recv_points: PointVec::new(),
        };

        assert_eq!(data.name, "Test Server");
        assert_eq!(data.sent_points.len(), 0);
        assert_eq!(data.recv_points.len(), 0);
    }
}
