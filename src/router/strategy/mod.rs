//! Routing strategies for backend selection

pub mod adaptive;
pub mod round_robin;

pub use adaptive::AdaptiveStrategy;
pub use round_robin::RoundRobinStrategy;
