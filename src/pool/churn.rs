//! Backend connection churn counters.
//!
//! A backend that keeps dropping or refusing connections looks healthy from
//! request counters alone until the pool runs dry. These counters record each
//! connection's birth and death so a dashboard can show churn before it turns
//! into failed requests. They change once per connection, not per command.
//!
//! Closes are counted when the connection object is destroyed, whichever
//! path destroys it (including deadpool dropping idle objects when a pool
//! shrinks), so `created` minus the sum of closes is the live connection
//! count. Code that retires a connection tags it with a reason first; the
//! first tag wins.

use std::sync::atomic::{AtomicU64, Ordering};

/// How a pool tried to open a backend connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectOutcome {
    /// TCP, TLS, login, and mode negotiation all succeeded.
    Created,
    /// The network, TLS, or negotiation failed for a reason other than login.
    Failed,
    /// The backend refused the login (for example, too many connections).
    LoginRefused,
    /// Login backoff refused the attempt locally without touching the network.
    BackoffDenied,
}

/// Why a backend connection was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The backend misbehaved or the connection broke.
    BackendError,
    /// The request using it was cancelled (deadline expiry or a dropped
    /// future) mid-exchange, leaving unread bytes on the socket.
    Cancelled,
    /// The client went away mid-response, leaving the socket unreusable.
    ClientAbort,
    /// The periodic health check found it dead.
    HealthCheck,
    /// A pooled idle connection failed its liveness check on checkout.
    Stale,
    /// The pool dropped it without a tag: idle-timeout clearing, a pool
    /// shrinking during replacement cooldown, or shutdown.
    Idle,
}

/// Per-backend connection lifecycle counters, shared by every pool slot.
#[derive(Debug, Default)]
pub struct ConnectionChurn {
    created: AtomicU64,
    failed: AtomicU64,
    login_refused: AtomicU64,
    backoff_denied: AtomicU64,
    closed_backend_error: AtomicU64,
    closed_cancelled: AtomicU64,
    closed_client_abort: AtomicU64,
    closed_health_check: AtomicU64,
    closed_stale: AtomicU64,
    closed_idle: AtomicU64,
}

/// Plain copy of [`ConnectionChurn`] for the scrape endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectionChurnSnapshot {
    pub created: u64,
    pub failed: u64,
    pub login_refused: u64,
    pub backoff_denied: u64,
    pub closed_backend_error: u64,
    pub closed_cancelled: u64,
    pub closed_client_abort: u64,
    pub closed_health_check: u64,
    pub closed_stale: u64,
    pub closed_idle: u64,
}

impl ConnectionChurn {
    pub fn note_connect(&self, outcome: ConnectOutcome) {
        let counter = match outcome {
            ConnectOutcome::Created => &self.created,
            ConnectOutcome::Failed => &self.failed,
            ConnectOutcome::LoginRefused => &self.login_refused,
            ConnectOutcome::BackoffDenied => &self.backoff_denied,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_close(&self, reason: CloseReason) {
        let counter = match reason {
            CloseReason::BackendError => &self.closed_backend_error,
            CloseReason::Cancelled => &self.closed_cancelled,
            CloseReason::ClientAbort => &self.closed_client_abort,
            CloseReason::HealthCheck => &self.closed_health_check,
            CloseReason::Stale => &self.closed_stale,
            CloseReason::Idle => &self.closed_idle,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> ConnectionChurnSnapshot {
        ConnectionChurnSnapshot {
            created: self.created.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            login_refused: self.login_refused.load(Ordering::Relaxed),
            backoff_denied: self.backoff_denied.load(Ordering::Relaxed),
            closed_backend_error: self.closed_backend_error.load(Ordering::Relaxed),
            closed_cancelled: self.closed_cancelled.load(Ordering::Relaxed),
            closed_client_abort: self.closed_client_abort.load(Ordering::Relaxed),
            closed_health_check: self.closed_health_check.load(Ordering::Relaxed),
            closed_stale: self.closed_stale.load(Ordering::Relaxed),
            closed_idle: self.closed_idle.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_outcome_and_reason_has_its_own_counter() {
        let churn = ConnectionChurn::default();
        churn.note_connect(ConnectOutcome::Created);
        churn.note_connect(ConnectOutcome::Created);
        churn.note_connect(ConnectOutcome::Failed);
        churn.note_connect(ConnectOutcome::LoginRefused);
        churn.note_connect(ConnectOutcome::BackoffDenied);
        churn.note_close(CloseReason::BackendError);
        churn.note_close(CloseReason::Cancelled);
        churn.note_close(CloseReason::ClientAbort);
        churn.note_close(CloseReason::HealthCheck);
        churn.note_close(CloseReason::Stale);
        churn.note_close(CloseReason::Stale);
        churn.note_close(CloseReason::Idle);

        assert_eq!(
            churn.snapshot(),
            ConnectionChurnSnapshot {
                created: 2,
                failed: 1,
                login_refused: 1,
                backoff_denied: 1,
                closed_backend_error: 1,
                closed_cancelled: 1,
                closed_client_abort: 1,
                closed_health_check: 1,
                closed_stale: 2,
                closed_idle: 1,
            }
        );
    }
}
