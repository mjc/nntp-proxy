//! Session mode state management.

use crate::config::RoutingMode;

/// Session mode derived from the runtime routing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// Per-command routing.
    PerCommand,
    /// Stateful routing.
    Stateful,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RuntimeRoutingState {
    Stateful = 0,
    PerCommand = 1,
    HybridPerCommand = 2,
    HybridStateful = 3,
}

impl RuntimeRoutingState {
    const fn from_routing_mode(mode: RoutingMode) -> Self {
        match mode {
            RoutingMode::Stateful => Self::Stateful,
            RoutingMode::PerCommand => Self::PerCommand,
            RoutingMode::Hybrid => Self::HybridPerCommand,
        }
    }

    const fn mode(self) -> SessionMode {
        match self {
            Self::Stateful | Self::HybridStateful => SessionMode::Stateful,
            Self::PerCommand | Self::HybridPerCommand => SessionMode::PerCommand,
        }
    }

    const fn routing_mode(self) -> RoutingMode {
        match self {
            Self::Stateful => RoutingMode::Stateful,
            Self::PerCommand => RoutingMode::PerCommand,
            Self::HybridPerCommand | Self::HybridStateful => RoutingMode::Hybrid,
        }
    }
}

/// Result of attempting the one-way hybrid transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeTransition {
    /// The session changed from hybrid per-command to hybrid stateful.
    Switched,
    /// The session was already stateful.
    AlreadyStateful,
    /// The session was not in a hybrid per-command state.
    NotHybrid,
}

/// Manages the valid runtime routing state of a session.
///
/// A single runtime discriminant owns both configured routing and current mode,
/// so invalid combinations cannot be constructed.
#[derive(Debug)]
pub struct ModeState {
    state: RuntimeRoutingState,
}

impl ModeState {
    /// Create a session in the initial state implied by its routing configuration.
    #[must_use]
    pub const fn new(routing_mode: RoutingMode) -> Self {
        Self {
            state: RuntimeRoutingState::from_routing_mode(routing_mode),
        }
    }

    fn runtime_state(&self) -> RuntimeRoutingState {
        self.state
    }

    /// Get the current session mode.
    #[must_use]
    pub fn mode(&self) -> SessionMode {
        self.runtime_state().mode()
    }

    /// Get the configured routing mode.
    #[must_use]
    pub fn routing_mode(&self) -> RoutingMode {
        self.runtime_state().routing_mode()
    }

    /// Check if the current mode is per-command.
    #[must_use]
    pub fn is_per_command(&self) -> bool {
        matches!(self.mode(), SessionMode::PerCommand)
    }

    /// Check if the current mode is stateful.
    #[must_use]
    pub fn is_stateful(&self) -> bool {
        matches!(self.mode(), SessionMode::Stateful)
    }

    /// Check if the one-way hybrid transition is available.
    #[must_use]
    pub fn can_switch_mode(&self) -> bool {
        matches!(self.runtime_state(), RuntimeRoutingState::HybridPerCommand)
    }

    /// Switch a hybrid session to stateful mode.
    #[must_use]
    pub fn switch_to_stateful(&mut self) -> ModeTransition {
        match self.state {
            RuntimeRoutingState::HybridPerCommand => {
                self.state = RuntimeRoutingState::HybridStateful;
                ModeTransition::Switched
            }
            RuntimeRoutingState::HybridStateful | RuntimeRoutingState::Stateful => {
                ModeTransition::AlreadyStateful
            }
            RuntimeRoutingState::PerCommand => ModeTransition::NotHybrid,
        }
    }

    /// Check if the configured routing uses per-command execution.
    #[must_use]
    pub fn is_per_command_routing(&self) -> bool {
        matches!(
            self.routing_mode(),
            RoutingMode::PerCommand | RoutingMode::Hybrid
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stateful_constructor_starts_stateful() {
        let state = ModeState::new(RoutingMode::Stateful);
        assert_eq!(state.mode(), SessionMode::Stateful);
        assert_eq!(state.routing_mode(), RoutingMode::Stateful);
    }

    #[test]
    fn constructors_derive_valid_runtime_states() {
        let cases = [
            (RoutingMode::Stateful, SessionMode::Stateful),
            (RoutingMode::PerCommand, SessionMode::PerCommand),
            (RoutingMode::Hybrid, SessionMode::PerCommand),
        ];

        for (routing_mode, expected_mode) in cases {
            let state = ModeState::new(routing_mode);
            assert_eq!(state.mode(), expected_mode);
            assert_eq!(state.routing_mode(), routing_mode);
        }
    }

    #[test]
    fn hybrid_transition_table_is_exhaustive() {
        let cases = [
            (
                RoutingMode::Stateful,
                ModeTransition::AlreadyStateful,
                SessionMode::Stateful,
            ),
            (
                RoutingMode::PerCommand,
                ModeTransition::NotHybrid,
                SessionMode::PerCommand,
            ),
            (
                RoutingMode::Hybrid,
                ModeTransition::Switched,
                SessionMode::Stateful,
            ),
        ];

        for (routing_mode, expected_transition, expected_mode) in cases {
            let mut state = ModeState::new(routing_mode);
            assert_eq!(state.switch_to_stateful(), expected_transition);
            assert_eq!(state.mode(), expected_mode);
            assert_eq!(
                state.switch_to_stateful(),
                match routing_mode {
                    RoutingMode::PerCommand => ModeTransition::NotHybrid,
                    RoutingMode::Stateful | RoutingMode::Hybrid => ModeTransition::AlreadyStateful,
                }
            );
        }
    }

    #[test]
    fn hybrid_transition_is_one_way() {
        let mut state = ModeState::new(RoutingMode::Hybrid);
        assert!(state.is_per_command());
        assert!(state.can_switch_mode());

        assert_eq!(state.switch_to_stateful(), ModeTransition::Switched);
        assert!(state.is_stateful());
        assert!(!state.can_switch_mode());
        assert_eq!(state.switch_to_stateful(), ModeTransition::AlreadyStateful);
        assert_eq!(state.routing_mode(), RoutingMode::Hybrid);
    }

    #[test]
    fn per_command_routing_reflects_configuration_after_hybrid_transition() {
        let mut state = ModeState::new(RoutingMode::Hybrid);
        assert!(state.is_per_command_routing());

        let _ = state.switch_to_stateful();
        assert!(state.is_per_command_routing());
    }
}
