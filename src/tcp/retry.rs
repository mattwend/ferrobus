// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Retry configuration for Modbus TCP send operations.

use std::future::Future;
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};

use crate::error::ModbusError;

/// Exponential retry policy used by [`crate::tcp::ModbusTcpConnection`].
///
/// The policy applies to one `send_message` or `send_message_with_unit_id` call.
/// Each retry performs a fresh connect/write/read attempt after the connection
/// has been invalidated by the transient failure that triggered the retry.
///
/// The [`Default::default`] policy starts at 500 ms, multiplies delays by
/// 1.5, adds jitter, and retries until 2 s of total retry delay has elapsed.
/// Set [`Self::max_times`] when you also need to cap the number of retry
/// attempts. Pass `None` to [`crate::tcp::ModbusTcpConnection::with_retry`] to
/// disable same-call retry entirely.
///
/// # Retried errors
///
/// Only errors classified as transient by [`ModbusError::is_transient`] are
/// retried. Invalid retry configuration, request validation failures, protocol
/// mismatches, Modbus exception responses, and request/response mismatches are
/// returned immediately.
///
/// # Validation
///
/// A policy is validated when converted to the internal backoff builder, which
/// happens before the send operation starts. Call [`Self::validate`] earlier if
/// you want to fail fast while building configuration.
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

impl ModbusTcpRetry {
    /// Validates this retry policy without starting a send operation.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] when any field violates its
    /// documented invariant.
    pub fn validate(&self) -> Result<(), ModbusError> {
        if self.initial_delay.is_zero() {
            return Err(ModbusError::ValidationError(
                "retry initial_delay must be > 0".to_string(),
            ));
        }

        if self
            .max_delay
            .is_some_and(|max_delay| max_delay < self.initial_delay)
        {
            return Err(ModbusError::ValidationError(
                "retry max_delay must be >= initial_delay".to_string(),
            ));
        }

        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err(ModbusError::ValidationError(
                "retry multiplier must be finite and >= 1.0".to_string(),
            ));
        }

        if self.max_elapsed < self.initial_delay {
            return Err(ModbusError::ValidationError(
                "retry max_elapsed must be >= initial_delay".to_string(),
            ));
        }

        if self.max_times.is_some_and(|max_times| max_times < 1) {
            return Err(ModbusError::ValidationError(
                "retry max_times must be >= 1".to_string(),
            ));
        }

        Ok(())
    }

    /// Converts this policy to the internal backoff builder used by the send path.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] when any field violates its
    /// documented invariant.
    pub(crate) fn to_backoff(self) -> Result<ExponentialBuilder, ModbusError> {
        self.validate()?;

        let builder = ExponentialBuilder::default()
            .with_min_delay(self.initial_delay)
            .with_factor(self.multiplier)
            .with_total_delay(Some(self.max_elapsed));
        let builder = if let Some(max_delay) = self.max_delay {
            builder.with_max_delay(max_delay)
        } else {
            builder
        };
        let builder = if self.jitter {
            builder.with_jitter()
        } else {
            builder
        };
        let builder = if let Some(max_times) = self.max_times {
            builder.with_max_times(max_times)
        } else {
            builder.without_max_times()
        };

        Ok(builder)
    }
}

pub(crate) async fn retry_transient<Operation, Fut, T>(
    operation: Operation,
    retry: ModbusTcpRetry,
) -> Result<T, ModbusError>
where
    Operation: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ModbusError>>,
{
    operation
        .retry(retry.to_backoff()?)
        .when(ModbusError::is_transient)
        .await
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
#[allow(clippy::float_cmp, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn assert_validation_message(retry: ModbusTcpRetry, expected: &str) {
        let error = retry.to_backoff().unwrap_err();

        match error {
            ModbusError::ValidationError(message) => assert_eq!(message, expected),
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

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

    #[test]
    fn to_backoff_rejects_zero_initial_delay() {
        assert_validation_message(
            ModbusTcpRetry {
                initial_delay: Duration::ZERO,
                ..ModbusTcpRetry::default()
            },
            "retry initial_delay must be > 0",
        );
    }

    #[test]
    fn to_backoff_rejects_max_delay_below_initial() {
        assert_validation_message(
            ModbusTcpRetry {
                max_delay: Some(Duration::from_millis(1)),
                ..ModbusTcpRetry::default()
            },
            "retry max_delay must be >= initial_delay",
        );
    }

    #[test]
    fn to_backoff_rejects_multiplier_below_one() {
        assert_validation_message(
            ModbusTcpRetry {
                multiplier: 0.5,
                ..ModbusTcpRetry::default()
            },
            "retry multiplier must be finite and >= 1.0",
        );
    }

    #[test]
    fn to_backoff_rejects_non_finite_multiplier() {
        for multiplier in [f32::NAN, f32::INFINITY] {
            assert_validation_message(
                ModbusTcpRetry {
                    multiplier,
                    ..ModbusTcpRetry::default()
                },
                "retry multiplier must be finite and >= 1.0",
            );
        }
    }

    #[test]
    fn to_backoff_rejects_max_elapsed_below_initial() {
        assert_validation_message(
            ModbusTcpRetry {
                max_elapsed: Duration::from_millis(1),
                ..ModbusTcpRetry::default()
            },
            "retry max_elapsed must be >= initial_delay",
        );
    }

    #[test]
    fn to_backoff_rejects_zero_max_times() {
        assert_validation_message(
            ModbusTcpRetry {
                max_times: Some(0),
                ..ModbusTcpRetry::default()
            },
            "retry max_times must be >= 1",
        );
    }

    #[test]
    fn to_backoff_accepts_unbounded_max_times() {
        ModbusTcpRetry {
            max_times: None,
            ..ModbusTcpRetry::default()
        }
        .to_backoff()
        .unwrap();
    }

    #[test]
    fn to_backoff_accepts_default() {
        ModbusTcpRetry::default().to_backoff().unwrap();
    }

    #[test]
    fn validate_accepts_default() {
        ModbusTcpRetry::default().validate().unwrap();
    }

    #[test]
    fn to_backoff_default_matches_documented_chain() {
        let expected = ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(500))
            .with_factor(1.5)
            .with_jitter()
            .with_total_delay(Some(Duration::from_secs(2)))
            .without_max_times();

        assert_eq!(
            format!("{:?}", ModbusTcpRetry::default().to_backoff().unwrap()),
            format!("{expected:?}")
        );
    }
}
