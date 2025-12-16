//! TUI-specific types

use ratatui::style::Color;
use smallvec::SmallVec;

// ============================================================================
// Chart Coordinate Types
// ============================================================================

/// Type-safe X-axis coordinate (time index)
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct ChartX(f64);

impl ChartX {
    /// Create from raw value
    #[must_use]
    #[inline]
    pub const fn new(x: f64) -> Self {
        Self(x)
    }

    /// Get raw value
    #[must_use]
    #[inline]
    pub const fn get(&self) -> f64 {
        self.0
    }
}

impl From<usize> for ChartX {
    fn from(index: usize) -> Self {
        Self::new(index as f64)
    }
}

impl From<f64> for ChartX {
    fn from(x: f64) -> Self {
        Self::new(x)
    }
}

/// Type-safe Y-axis coordinate (throughput value)
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct ChartY(f64);

impl ChartY {
    /// Create from raw value
    #[must_use]
    #[inline]
    pub const fn new(y: f64) -> Self {
        Self(y)
    }

    /// Get raw value
    #[must_use]
    #[inline]
    pub const fn get(&self) -> f64 {
        self.0
    }

    /// Get maximum of two values
    #[must_use]
    #[inline]
    pub fn max(self, other: Self) -> Self {
        Self::new(self.0.max(other.0))
    }
}

impl From<f64> for ChartY {
    fn from(y: f64) -> Self {
        Self::new(y)
    }
}

/// Type-safe chart point (X, Y coordinates)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartPoint {
    /// X-axis coordinate (time index)
    pub x: ChartX,
    /// Y-axis coordinate (throughput value)
    pub y: ChartY,
}

impl ChartPoint {
    /// Create a new chart point
    #[must_use]
    #[inline]
    pub const fn new(x: ChartX, y: ChartY) -> Self {
        Self { x, y }
    }

    /// Convert to tuple for ratatui compatibility
    #[must_use]
    #[inline]
    pub const fn as_tuple(&self) -> (f64, f64) {
        (self.x.get(), self.y.get())
    }
}

impl From<(f64, f64)> for ChartPoint {
    fn from((x, y): (f64, f64)) -> Self {
        Self::new(ChartX::new(x), ChartY::new(y))
    }
}

// ============================================================================
// Chart Data Types
// ============================================================================

/// Stack-allocated point vectors for typical history sizes (60 points = 60 seconds)
pub type PointVec = SmallVec<[ChartPoint; 64]>;

/// Stack-allocated chart data for typical backend counts (up to 8 backends)
/// Most deployments have 1-4 backends, so this avoids heap allocation
pub type ChartDataVec = SmallVec<[BackendChartData; 8]>;

/// Chart data for a single backend server
///
/// Pre-computed data points to avoid nested iterations during rendering
#[derive(Debug, Clone)]
pub struct BackendChartData {
    /// Server name for legend
    pub name: String,
    /// Color for this backend's lines
    pub color: Color,
    /// Pre-computed tuples for ratatui (cached to avoid allocation on render)
    sent_tuples: Vec<(f64, f64)>,
    /// Pre-computed tuples for ratatui (cached to avoid allocation on render)
    recv_tuples: Vec<(f64, f64)>,
}

impl BackendChartData {
    /// Create new chart data with pre-computed tuples
    #[must_use]
    pub fn new(name: String, color: Color, sent_points: PointVec, recv_points: PointVec) -> Self {
        let sent_tuples = sent_points.iter().map(|p| p.as_tuple()).collect();
        let recv_tuples = recv_points.iter().map(|p| p.as_tuple()).collect();
        Self {
            name,
            color,
            sent_tuples,
            recv_tuples,
        }
    }

    /// Get sent points as tuples for ratatui (pre-computed, zero-copy)
    #[must_use]
    #[inline]
    pub fn sent_points_as_tuples(&self) -> &[(f64, f64)] {
        &self.sent_tuples
    }

    /// Get received points as tuples for ratatui (pre-computed, zero-copy)
    #[must_use]
    #[inline]
    pub fn recv_points_as_tuples(&self) -> &[(f64, f64)] {
        &self.recv_tuples
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chart_x() {
        let x = ChartX::new(10.5);
        assert_eq!(x.get(), 10.5);

        let x2: ChartX = 5usize.into();
        assert_eq!(x2.get(), 5.0);
    }

    #[test]
    fn test_chart_y() {
        let y1 = ChartY::new(100.0);
        let y2 = ChartY::new(200.0);

        assert_eq!(y1.max(y2).get(), 200.0);
        assert_eq!(y2.max(y1).get(), 200.0);
    }

    #[test]
    fn test_chart_point() {
        let point = ChartPoint::new(ChartX::new(1.0), ChartY::new(2.0));
        assert_eq!(point.as_tuple(), (1.0, 2.0));

        let point2: ChartPoint = (3.0, 4.0).into();
        assert_eq!(point2.x.get(), 3.0);
        assert_eq!(point2.y.get(), 4.0);
    }

    #[test]
    fn test_point_vec() {
        let mut points = PointVec::new();
        for i in 0..60 {
            points.push(ChartPoint::new(
                ChartX::from(i),
                ChartY::new(i as f64 * 1000.0),
            ));
        }

        assert_eq!(points.len(), 60);
        assert_eq!(points[0].x.get(), 0.0);
        assert_eq!(points[59].x.get(), 59.0);
    }

    #[test]
    fn test_backend_chart_data_conversions() {
        let mut sent_points = PointVec::new();
        sent_points.push(ChartPoint::new(ChartX::new(0.0), ChartY::new(100.0)));
        sent_points.push(ChartPoint::new(ChartX::new(1.0), ChartY::new(200.0)));

        let data = BackendChartData::new(
            "Test".to_string(),
            Color::Green,
            sent_points,
            PointVec::new(),
        );

        let tuples = data.sent_points_as_tuples();
        assert_eq!(tuples.len(), 2);
        assert_eq!(tuples[0], (0.0, 100.0));
        assert_eq!(tuples[1], (1.0, 200.0));
    }
}
