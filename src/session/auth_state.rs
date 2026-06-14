//! Authentication state management for client sessions
//!
//! This module provides a type-safe wrapper around authentication state,
//! ensuring proper initialization and access patterns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

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

/// Represents the authentication state of a client session
///
/// This type encapsulates the authentication status and username in a
/// thread-safe manner using atomic operations and write-once semantics.
///
/// # Design
///
/// - **Status**: `AtomicBool` for lock-free concurrent access
/// - **Username**: `OnceLock<Arc<str>>` for write-once, cheap-clone reads
/// - Both fields are private and accessed through controlled methods
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
    /// Whether the client has successfully authenticated
    ///
    /// Starts as `false` and is set to `true` after successful authentication.
    /// Uses `Relaxed` ordering since authentication is a one-way transition
    /// and doesn't require synchronization with other memory operations.
    authenticated: AtomicBool,

    /// The authenticated username (if any)
    ///
    /// Write-once field that stores the username after successful authentication.
    /// Uses `Arc<str>` for cheap clones when reading the username.
    username: OnceLock<Arc<str>>,
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
            authenticated: AtomicBool::new(false),
            username: OnceLock::new(),
        }
    }

    /// Check if the client has authenticated
    ///
    /// This is a cheap operation (single atomic load) that can be called
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
        self.authenticated.load(Ordering::Relaxed)
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
        let username_arc: Arc<str> = username.into();
        // Set username first (write-once, safe to call multiple times with same value)
        let _ = self.username.set(username_arc);
        // Then mark as authenticated (one-way transition)
        if self.authenticated.swap(true, Ordering::Relaxed) {
            AuthenticationTransition::AlreadyAuthenticated
        } else {
            AuthenticationTransition::NewlyAuthenticated
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
        self.username.get().map(|arc| &**arc)
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
}
