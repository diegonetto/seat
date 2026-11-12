//! Backoff bounds shared by the queued-state and webhook workers.
use std::time::Duration;

/// Each caller keeps its own initial-wait and reset rules.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Waits {
    pub first: Duration,
    pub max: Duration,
}

/// The poll slice both worker loops sleep in: the dispatcher's scan
/// wait and mid-backoff wake check, the webhook ring's wait for the
/// next re-ring.
pub(crate) const SLICE: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_characterization_100ms() {
        assert_eq!(SLICE, Duration::from_millis(100));
    }
}
