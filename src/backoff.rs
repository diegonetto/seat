//! Backoff bounds shared by the queued-state and webhook workers.
use std::time::Duration;

/// Each caller keeps its own initial-wait and reset rules.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Waits {
    pub first: Duration,
    pub max: Duration,
}
