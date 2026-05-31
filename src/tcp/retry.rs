// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Retry configuration for Modbus TCP send operations.

use std::time::Duration;

/// Exponential retry policy used by [`crate::tcp::ModbusTcpConnection`].
///
/// The [`Default::default`] policy starts at 500 ms, multiplies delays by
/// 1.5, adds jitter, and retries until 2 s of total retry delay has elapsed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModbusTcpRetry {
    /// Initial delay before the first retry attempt, in wall-clock time.
    ///
    /// Must be greater than zero.
    pub initial_delay: Duration,
    /// Optional maximum delay for an individual retry sleep, in wall-clock time.
    ///
    /// When set, this must be greater than or equal to [`Self::initial_delay`].
    /// `None` leaves individual sleeps uncapped.
    pub max_delay: Option<Duration>,
    /// Multiplier applied to each subsequent retry delay.
    ///
    /// Must be finite and greater than or equal to `1.0`.
    pub multiplier: f32,
    /// Maximum total retry delay for one send call, in wall-clock time.
    ///
    /// Must be greater than or equal to [`Self::initial_delay`].
    pub max_elapsed: Duration,
    /// Optional cap on the number of retries within one send call.
    ///
    /// When set, this must be greater than or equal to `1`. `None` means retry
    /// count is unbounded and governed only by [`Self::max_elapsed`].
    pub max_times: Option<usize>,
    /// Whether to add jitter to retry sleeps.
    pub jitter: bool,
}

impl Default for ModbusTcpRetry {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: None,
            multiplier: 1.5,
            max_elapsed: Duration::from_secs(2),
            max_times: None,
            jitter: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_default_is_documented_policy() {
        let retry = ModbusTcpRetry::default();

        assert_eq!(retry.initial_delay, Duration::from_millis(500));
        assert_eq!(retry.max_delay, None);
        assert_eq!(retry.multiplier, 1.5);
        assert_eq!(retry.max_elapsed, Duration::from_secs(2));
        assert_eq!(retry.max_times, None);
        assert!(retry.jitter);
    }
}
