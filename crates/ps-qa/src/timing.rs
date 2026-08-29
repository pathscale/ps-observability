//! Shared latency policy for driven interactions.

use std::time::Duration;

use crate::cli;

/// Inter-event delay. Zero saturates the input queue for ceiling measurements.
pub(crate) fn pace() -> Duration {
    Duration::from_secs_f64(cli::pace().max(0.0))
}

/// A fixed check budget, widened only by an explicit runner multiplier.
pub(crate) fn check_timeout(milliseconds: u64) -> Duration {
    Duration::from_secs_f64(
        Duration::from_millis(milliseconds).as_secs_f64() * cli::timeout_scale(),
    )
}

pub(crate) async fn sleep_pace() {
    let pace = pace();
    if !pace.is_zero() {
        tokio::time::sleep(pace).await;
    }
}
