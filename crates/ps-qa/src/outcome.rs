//! Bound outcome reads without discarding completed assertion evidence.

use std::ops::AsyncFnOnce;
use std::time::{Duration, Instant};

use eyre::Result;

use crate::inspector::{Client, InspectorRequestTimeout};

/// Leave enough time for an inspector exchange under ordinary scheduling load.
pub(crate) const MIN_REQUEST_BUDGET: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct OutcomeTimeout;

impl std::fmt::Display for OutcomeTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "OutcomeTimeout: no completed sample before the outcome request budget ended",
        )
    }
}

impl std::error::Error for OutcomeTimeout {}

pub(crate) fn budget(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining < MIN_REQUEST_BUDGET {
        return Err(OutcomeTimeout.into());
    }
    Ok(remaining)
}

pub(crate) async fn read<T>(
    client: &mut Client,
    deadline: Instant,
    read: impl AsyncFnOnce(&mut Client) -> Result<T>,
) -> Result<T> {
    let remaining = budget(deadline)?;
    let previous_timeout = client.request_timeout();
    client.set_request_timeout(previous_timeout.min(remaining));
    let result = read(client).await;
    client.set_request_timeout(previous_timeout);
    result
}

pub(crate) fn is_timeout(client: &Client, error: &eyre::Report) -> bool {
    error.downcast_ref::<OutcomeTimeout>().is_some()
        || error.downcast_ref::<InspectorRequestTimeout>().is_some()
        || client.terminal_is_timeout()
}

pub(crate) fn exhausted(last_failure: Option<String>) -> String {
    last_failure.unwrap_or_else(|| OutcomeTimeout.to_string())
}

pub(crate) fn failure(
    client: &Client,
    last_failure: Option<String>,
    error: eyre::Report,
) -> String {
    if is_timeout(client, &error) {
        last_failure.unwrap_or_else(|| error.to_string())
    } else {
        error.to_string()
    }
}
