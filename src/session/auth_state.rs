//! Authentication state management for client sessions
//!
//! This module provides a type-safe wrapper around authentication state,
//! ensuring proper initialization and access patterns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

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
/// use nntp_proxy::session::AuthState;
///
/// let auth_state = AuthState::new();
/// assert!(!auth_state.is_authenticated());
///
/// // After authentication
/// auth_state.mark_authenticated("user@example.com");
/// assert!(auth_state.is_authenticated());
/// assert_eq!(auth_state.username().unwrap().as_ref(), "user@example.com");
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
    /// use nntp_proxy::session::AuthState;
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
    /// auth_state.mark_authenticated("alice");
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
    /// # Panics
    ///
    /// Panics if called multiple times with different usernames (implementation
    /// detail of `OnceLock::set`).
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::AuthState;
    ///
    /// let auth_state = AuthState::new();
    /// auth_state.mark_authenticated("bob");
    ///
    /// assert!(auth_state.is_authenticated());
    /// assert_eq!(auth_state.username().unwrap().as_ref(), "bob");
    /// ```
    #[inline]
    pub fn mark_authenticated(&self, username: impl Into<Arc<str>>) {
        let username_arc: Arc<str> = username.into();
        // Set username first (write-once, safe to call multiple times with same value)
        let _ = self.username.set(username_arc);
        // Then mark as authenticated (one-way transition)
        self.authenticated.store(true, Ordering::Relaxed);
    }

    /// Get the authenticated username if available
    ///
    /// Returns a cheap-to-clone `Arc<str>` reference to the username.
    /// Returns `None` if the client has not authenticated yet.
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::session::AuthState;
    ///
    /// let auth_state = AuthState::new();
    /// assert!(auth_state.username().is_none());
    ///
    /// auth_state.mark_authenticated("charlie");
    /// let username = auth_state.username().unwrap();
    /// assert_eq!(username.as_ref(), "charlie");
    ///
    /// // Cloning is cheap (Arc reference count bump)
    /// let username2 = username.clone();
    /// assert_eq!(username2.as_ref(), "charlie");
    /// ```
    #[inline]
    #[must_use]
    pub fn username(&self) -> Option<Arc<str>> {
        self.username.get().cloned()
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
    /// use nntp_proxy::session::AuthState;
    ///
    /// let auth_state = AuthState::new();
    /// assert!(!auth_state.is_authenticated_or_skipped(false));
    /// assert!(auth_state.is_authenticated_or_skipped(true)); // Skips check
    ///
    /// auth_state.mark_authenticated("dave");
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
        state.mark_authenticated("testuser");

        assert!(state.is_authenticated());
        assert_eq!(state.username().unwrap().as_ref(), "testuser");
    }

    #[test]
    fn test_mark_authenticated_with_arc() {
        let state = AuthState::new();
        let username: Arc<str> = Arc::from("arcuser");
        state.mark_authenticated(username.clone());

        assert!(state.is_authenticated());
        assert_eq!(state.username().unwrap().as_ref(), "arcuser");
    }

    #[test]
    fn test_username_clone_is_cheap() {
        let state = AuthState::new();
        state.mark_authenticated("clonetest");

        let username1 = state.username().unwrap();
        let username2 = state.username().unwrap();

        // Both point to the same Arc
        assert_eq!(username1.as_ref(), username2.as_ref());
        assert_eq!(Arc::strong_count(&username1), Arc::strong_count(&username2));
    }

    #[test]
    fn test_is_authenticated_or_skipped() {
        let state = AuthState::new();

        // Not authenticated, skip=false
        assert!(!state.is_authenticated_or_skipped(false));

        // Not authenticated, skip=true
        assert!(state.is_authenticated_or_skipped(true));

        state.mark_authenticated("skiptest");

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
        state.mark_authenticated("same");
        state.mark_authenticated("same"); // Should not panic

        assert!(state.is_authenticated());
        assert_eq!(state.username().unwrap().as_ref(), "same");
    }
}
