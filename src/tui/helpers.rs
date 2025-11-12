//! TUI rendering helper functions

use ratatui::style::Color;

use super::constants::{BACKEND_COLORS, throughput};

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
    fn test_format_throughput_label() {
        assert_eq!(format_throughput_label(10_000_000.0), "10 MB/s");
        assert_eq!(format_throughput_label(500_000.0), "500 KB/s");
        assert_eq!(format_throughput_label(100.0), "100 B/s");
    }
}
