//! Session handlers for different routing modes
//!
//! This module contains the core session handling logic split by routing mode:
//! - `stateful`: Stateful 1:1 routing with dedicated backend connection
//! - `per_command`: Per-command routing where each command can go to a different backend (stateless)
//! - `hybrid`: Hybrid mode that starts with per-command routing and switches to stateful
//!
//! Shared utilities are in the parent `session::common` module.
//!
//! All handler functions are implemented as methods on `ClientSession` in their
//! respective modules. No need to re-export since they're all impl blocks.
use crate::pool::ConnectionGuard;
use crate::session::backend::BackendResponseComplete;
use crate::types::BackendId;

pub(super) struct BackendLease {
    pub(super) backend_id: BackendId,
    pub(super) connection: ConnectionGuard,
    pub(super) completion: BackendResponseComplete,
}

impl BackendLease {
    pub(super) const fn new(
        backend_id: BackendId,
        connection: ConnectionGuard,
        completion: BackendResponseComplete,
    ) -> Self {
        Self {
            backend_id,
            connection,
            completion,
        }
    }

    pub(super) fn complete_success(self) {
        let _ = self.connection.complete_success(self.completion);
    }

    pub(super) fn fail_backend(self) {
        self.connection.fail_backend();
    }
}

mod article_retry;
mod cache_operations;
mod command_execution;
pub(crate) use command_execution::should_sample_backend_timing;
mod hybrid;
mod per_command;
mod pipeline;
mod stateful;
