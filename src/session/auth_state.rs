//! Authentication state management for client sessions
//!
//! This module provides a type-safe wrapper around authentication state,
//! ensuring proper initialization and access patterns.

use crate::types::Username;
use std::sync::{Arc, OnceLock};
/// Final authenticated identity, published exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedUser(Arc<str>);

impl AuthenticatedUser {
    fn new(username: impl Into<Arc<str>>) -> Self {
        Self(username.into())
    }

    #[must_use]
    pub fn username(&self) -> &str {
        &self.0
    }
}

/// Result of attempting to authenticate a session.
#[must_use = "authentication transitions drive user connection gauge updates"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationTransition {
    /// This call changed the session from unauthenticated to authenticated.
    NewlyAuthenticated,
    /// The session had already authenticated before this call.
    AlreadyAuthenticated,
}

impl AuthenticationTransition {
    #[must_use]
    pub const fn is_new(self) -> bool {
        matches!(self, Self::NewlyAuthenticated)
    }
}

/// Runtime authentication state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientAuthState {
    /// No username has been supplied yet.
    Anonymous,
    /// A username is waiting for its password.
    AwaitingPassword(Username),
    /// The client has successfully authenticated.
    Authenticated(Username),
}

impl ClientAuthState {
    #[must_use]
    pub const fn anonymous() -> Self {
        Self::Anonymous
    }

    #[must_use]
    pub fn username(&self) -> Option<&str> {
        match self {
            Self::Anonymous => None,
            Self::AwaitingPassword(username) | Self::Authenticated(username) => {
                Some(username.as_str())
            }
        }
    }
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::Anonymous)
    }

    /// Reduce one parsed authentication event without performing I/O.
    #[must_use]
    pub fn reduce(
        self,
        event: ClientAuthEvent,
        credentials_valid: bool,
    ) -> (Self, AuthReducerResult) {
        match (self, event) {
            (Self::Authenticated(username), _) => (
                Self::Authenticated(username),
                AuthReducerResult::AlreadyAuthenticated,
            ),
            (Self::Anonymous, ClientAuthEvent::User(username))
            | (Self::AwaitingPassword(_), ClientAuthEvent::User(username)) => (
                Self::AwaitingPassword(username),
                AuthReducerResult::PasswordRequired,
            ),
            (Self::Anonymous, ClientAuthEvent::Password { .. }) => {
                (Self::Anonymous, AuthReducerResult::OutOfSequence)
            }
            (Self::AwaitingPassword(username), ClientAuthEvent::Password { .. })
                if credentials_valid =>
            {
                (Self::Authenticated(username), AuthReducerResult::Accepted)
            }
            (Self::AwaitingPassword(username), ClientAuthEvent::Password { .. }) => (
                Self::AwaitingPassword(username),
                AuthReducerResult::Rejected,
            ),
            (state, ClientAuthEvent::Unknown) => (state, AuthReducerResult::Unknown),
        }
    }
}

/// Parsed authentication input for ClientAuthState::reduce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientAuthEvent {
    User(Username),
    Password { bytes: Vec<u8> },
    Unknown,
}

/// Result of reducing one authentication input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthReducerResult {
    PasswordRequired,
    Accepted,
    Rejected,
    OutOfSequence,
    AlreadyAuthenticated,
    Unknown,
}

/// Represents the authentication state of a client session
///
/// This type owns the final authenticated identity and publishes it once.
/// Reads observe the published identity through `OnceLock`.
///
/// # Examples
///
/// ```
/// use nntp_proxy::session::{AuthState, AuthenticationTransition};
///
/// let auth_state = AuthState::new();
/// assert!(!auth_state.is_authenticated());
///
/// // After authentication
/// assert_eq!(
///     auth_state.mark_authenticated("user@example.com"),
///     AuthenticationTransition::NewlyAuthenticated
/// );
/// assert!(auth_state.is_authenticated());
/// assert_eq!(auth_state.username().unwrap(), "user@example.com");
/// ```
#[derive(Debug)]
pub struct AuthState {
    /// The final authenticated identity, published at most once.
    identity: OnceLock<AuthenticatedUser>,
}

impl AuthState {
    /// Create a new unauthenticated state
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{AuthState, AuthenticationTransition};
    ///
    /// let auth_state = AuthState::new();
    /// assert!(!auth_state.is_authenticated());
    /// assert!(auth_state.username().is_none());
    /// ```
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            identity: OnceLock::new(),
        }
    }

    /// Check if the client has authenticated
    ///
    /// This is a cheap identity check that can be called
    /// frequently without performance concerns.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::AuthState;
    ///
    /// let auth_state = AuthState::new();
    /// assert!(!auth_state.is_authenticated());
    ///
    /// assert_eq!(
    ///     auth_state.mark_authenticated("alice"),
    ///     AuthenticationTransition::NewlyAuthenticated
    /// );
    /// assert!(auth_state.is_authenticated());
    /// ```
    #[inline]
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.identity.get().is_some()
    }

    /// Mark the client as authenticated with the given username
    ///
    /// This is a one-way operation - once authenticated, the state cannot
    /// be reverted. The username is stored in a write-once field.
    ///
    /// # Arguments
    ///
    /// * `username` - The authenticated username
    ///
    /// # Returns
    ///
    /// Returns whether this call changed the session from unauthenticated to
    /// authenticated. Repeated successful auth commands return
    /// [`AuthenticationTransition::AlreadyAuthenticated`] so callers can keep
    /// connection-open side effects idempotent.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{AuthState, AuthenticationTransition};
    ///
    /// let auth_state = AuthState::new();
    /// assert_eq!(
    ///     auth_state.mark_authenticated("bob"),
    ///     AuthenticationTransition::NewlyAuthenticated
    /// );
    ///
    /// assert!(auth_state.is_authenticated());
    /// assert_eq!(auth_state.username().unwrap(), "bob");
    /// ```
    #[inline]
    pub fn mark_authenticated(&self, username: impl Into<Arc<str>>) -> AuthenticationTransition {
        if self.identity.set(AuthenticatedUser::new(username)).is_ok() {
            AuthenticationTransition::NewlyAuthenticated
        } else {
            AuthenticationTransition::AlreadyAuthenticated
        }
    }

    /// Get the authenticated username if available
    ///
    /// Returns a cheap-to-clone `Arc<str>` reference to the username.
    /// Returns `None` if the client has not authenticated yet.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{AuthState, AuthenticationTransition};
    ///
    /// let auth_state = AuthState::new();
    /// assert!(auth_state.username().is_none());
    ///
    /// assert_eq!(
    ///     auth_state.mark_authenticated("charlie"),
    ///     AuthenticationTransition::NewlyAuthenticated
    /// );
    /// let username = auth_state.username().unwrap();
    /// assert_eq!(username, "charlie");
    ///
    /// // Cloning is cheap (Arc reference count bump)
    /// let username2 = username.clone();
    /// assert_eq!(username2, "charlie");
    /// ```
    #[inline]
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        self.identity.get().map(AuthenticatedUser::username)
    }

    /// Check if authenticated, optionally bypassing the check
    ///
    /// This method is useful when authentication checks can be skipped
    /// (e.g., when the backend doesn't require authentication).
    ///
    /// # Arguments
    ///
    /// * `skip_check` - If `true`, always returns `true`. Otherwise, returns actual auth state.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::{AuthState, AuthenticationTransition};
    ///
    /// let auth_state = AuthState::new();
    /// assert!(!auth_state.is_authenticated_or_skipped(false));
    /// assert!(auth_state.is_authenticated_or_skipped(true)); // Skips check
    ///
    /// assert_eq!(
    ///     auth_state.mark_authenticated("dave"),
    ///     AuthenticationTransition::NewlyAuthenticated
    /// );
    /// assert!(auth_state.is_authenticated_or_skipped(false));
    /// assert!(auth_state.is_authenticated_or_skipped(true));
    /// ```
    #[inline]
    #[must_use]
    pub fn is_authenticated_or_skipped(&self, skip_check: bool) -> bool {
        skip_check || self.is_authenticated()
    }
}

impl Default for AuthState {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_unauthenticated() {
        let state = AuthState::new();
        assert!(!state.is_authenticated());
        assert!(state.username().is_none());
    }

    #[test]
    fn test_mark_authenticated() {
        let state = AuthState::new();
        assert_eq!(
            state.mark_authenticated("testuser"),
            AuthenticationTransition::NewlyAuthenticated
        );

        assert!(state.is_authenticated());
        assert_eq!(state.username().unwrap(), "testuser");
    }

    #[test]
    fn test_mark_authenticated_with_arc() {
        let state = AuthState::new();
        let username: Arc<str> = Arc::from("arcuser");
        assert_eq!(
            state.mark_authenticated(username),
            AuthenticationTransition::NewlyAuthenticated
        );

        assert!(state.is_authenticated());
        assert_eq!(state.username().unwrap(), "arcuser");
    }

    #[test]
    fn test_is_authenticated_or_skipped() {
        let state = AuthState::new();

        // Not authenticated, skip=false
        assert!(!state.is_authenticated_or_skipped(false));

        // Not authenticated, skip=true
        assert!(state.is_authenticated_or_skipped(true));

        assert_eq!(
            state.mark_authenticated("skiptest"),
            AuthenticationTransition::NewlyAuthenticated
        );

        // Authenticated, skip=false
        assert!(state.is_authenticated_or_skipped(false));

        // Authenticated, skip=true
        assert!(state.is_authenticated_or_skipped(true));
    }

    #[test]
    fn test_default() {
        let state = AuthState::default();
        assert!(!state.is_authenticated());
        assert!(state.username().is_none());
    }

    #[test]
    fn test_multiple_mark_same_username() {
        let state = AuthState::new();
        assert_eq!(
            state.mark_authenticated("same"),
            AuthenticationTransition::NewlyAuthenticated
        );
        assert_eq!(
            state.mark_authenticated("same"),
            AuthenticationTransition::AlreadyAuthenticated
        );

        assert!(state.is_authenticated());
        assert_eq!(state.username().unwrap(), "same");
    }

    #[test]
    fn test_multiple_mark_keeps_original_username() {
        let state = AuthState::new();
        assert_eq!(
            state.mark_authenticated("first"),
            AuthenticationTransition::NewlyAuthenticated
        );
        assert_eq!(
            state.mark_authenticated("second"),
            AuthenticationTransition::AlreadyAuthenticated
        );

        assert!(state.is_authenticated());
        assert_eq!(state.username().unwrap(), "first");
    }
    #[test]
    fn reducer_covers_authentication_ordering() {
        let alice = Username::try_new("alice".to_owned()).unwrap();
        let (state, result) =
            ClientAuthState::anonymous().reduce(ClientAuthEvent::User(alice.clone()), false);
        assert_eq!(state, ClientAuthState::AwaitingPassword(alice.clone()));
        assert_eq!(result, AuthReducerResult::PasswordRequired);

        let (state, result) = ClientAuthState::anonymous().reduce(
            ClientAuthEvent::Password {
                bytes: b"secret".to_vec(),
            },
            true,
        );
        assert_eq!(state, ClientAuthState::Anonymous);
        assert_eq!(result, AuthReducerResult::OutOfSequence);

        let (state, result) = ClientAuthState::AwaitingPassword(alice.clone()).reduce(
            ClientAuthEvent::Password {
                bytes: b"wrong".to_vec(),
            },
            false,
        );
        assert_eq!(state, ClientAuthState::AwaitingPassword(alice.clone()));
        assert_eq!(result, AuthReducerResult::Rejected);

        let (state, result) = ClientAuthState::AwaitingPassword(alice.clone()).reduce(
            ClientAuthEvent::Password {
                bytes: b"secret".to_vec(),
            },
            true,
        );
        assert_eq!(state, ClientAuthState::Authenticated(alice.clone()));
        assert_eq!(result, AuthReducerResult::Accepted);

        let (state, result) = ClientAuthState::Authenticated(alice.clone()).reduce(
            ClientAuthEvent::User(Username::try_new("bob".to_owned()).unwrap()),
            false,
        );
        assert_eq!(state, ClientAuthState::Authenticated(alice));
        assert_eq!(result, AuthReducerResult::AlreadyAuthenticated);
    }

    #[test]
    fn reducer_preserves_unknown_events() {
        let (state, result) = ClientAuthState::anonymous().reduce(ClientAuthEvent::Unknown, false);
        assert_eq!(state, ClientAuthState::Anonymous);
        assert_eq!(result, AuthReducerResult::Unknown);
    }
}
