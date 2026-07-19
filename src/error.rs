// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

use std::sync::Arc;

use thiserror::Error;

/// Custom error type for the Modbus library.
///
/// Transport I/O variants store their original [`std::io::Error`] behind an
/// [`Arc`], so `ModbusError` values can be cloned without losing OS-specific
/// error details. Downstream matchers can continue to call methods such as
/// `kind()` on the bound error because `Arc<std::io::Error>` dereferences to
/// `std::io::Error`.
#[derive(Error, Debug, Clone)]
pub enum ModbusError {
    /// TCP connection establishment failed.
    #[error("TCP connect error: {0}")]
    ConnectError(Arc<std::io::Error>),

    /// TCP connection establishment exceeded the configured timeout.
    #[error("TCP connect timed out")]
    ConnectTimeout,

    /// Writing bytes to the TCP stream failed.
    #[error("TCP write error: {0}")]
    WriteError(Arc<std::io::Error>),

    /// Writing a frame exceeded the configured timeout.
    #[error("TCP write timed out")]
    WriteTimeout,

    /// Reading bytes from the TCP stream failed.
    #[error("TCP read error: {0}")]
    ReadError(Arc<std::io::Error>),

    /// Reading a frame exceeded the configured timeout.
    #[error("TCP read timed out")]
    ReadTimeout,

    /// A request waited longer than the configured queue timeout before reaching the wire.
    #[error("Modbus request queue timed out")]
    QueueTimeout,

    /// A Modbus TCP response frame was structurally invalid.
    #[error("Malformed response: {0}")]
    MalformedResponse(String),

    /// A Modbus PDU could not be decoded.
    #[error("Deserialization error: {0}")]
    DeserializationError(String),

    /// A slave returned a Modbus exception response.
    #[error("Modbus exception response: function {function:#04x}, code {code:#04x}")]
    ExceptionResponse {
        /// Exception function code, including the high exception bit.
        function: u8,
        /// Modbus exception code.
        code: u8,
    },

    /// The response transaction identifier did not match the request.
    ///
    /// This is unreachable in normal operation and indicates internal response
    /// routing corruption or a reader bug.
    #[error("Transaction ID mismatch: sent {expected}, received {actual}")]
    TransactionIdMismatch {
        /// Transaction identifier sent in the request.
        expected: u16,
        /// Transaction identifier received in the response.
        actual: u16,
    },

    /// The response protocol identifier was not Modbus TCP protocol id 0.
    #[error("Protocol ID mismatch: expected 0, received {actual}")]
    ProtocolIdMismatch {
        /// Protocol identifier received in the response.
        actual: u16,
    },

    /// All candidate Modbus transaction identifiers are already in flight.
    #[error("no free Modbus transaction id available")]
    NoFreeTransactionId,

    /// The response unit identifier did not match the request.
    #[error("Unit ID mismatch: expected {expected}, received {actual}")]
    UnitIdMismatch {
        /// Unit identifier sent in the request.
        expected: u8,
        /// Unit identifier received in the response.
        actual: u8,
    },

    /// The decoded response PDU did not match the request PDU.
    #[error("Request/response mismatch: {0}")]
    RequestResponseMismatch(String),

    /// A request failed protocol limit validation.
    #[error("Validation error: {0}")]
    ValidationError(String),
}

impl PartialEq for ModbusError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ConnectError(lhs), Self::ConnectError(rhs))
            | (Self::WriteError(lhs), Self::WriteError(rhs))
            | (Self::ReadError(lhs), Self::ReadError(rhs)) => io_errors_equal(lhs, rhs),
            (Self::ConnectTimeout, Self::ConnectTimeout)
            | (Self::WriteTimeout, Self::WriteTimeout)
            | (Self::ReadTimeout, Self::ReadTimeout)
            | (Self::QueueTimeout, Self::QueueTimeout)
            | (Self::NoFreeTransactionId, Self::NoFreeTransactionId) => true,
            (Self::MalformedResponse(lhs), Self::MalformedResponse(rhs))
            | (Self::DeserializationError(lhs), Self::DeserializationError(rhs))
            | (Self::RequestResponseMismatch(lhs), Self::RequestResponseMismatch(rhs))
            | (Self::ValidationError(lhs), Self::ValidationError(rhs)) => lhs == rhs,
            (
                Self::ExceptionResponse {
                    function: lhs_function,
                    code: lhs_code,
                },
                Self::ExceptionResponse {
                    function: rhs_function,
                    code: rhs_code,
                },
            ) => lhs_function == rhs_function && lhs_code == rhs_code,
            (
                Self::TransactionIdMismatch {
                    expected: lhs_expected,
                    actual: lhs_actual,
                },
                Self::TransactionIdMismatch {
                    expected: rhs_expected,
                    actual: rhs_actual,
                },
            ) => lhs_expected == rhs_expected && lhs_actual == rhs_actual,
            (
                Self::ProtocolIdMismatch { actual: lhs_actual },
                Self::ProtocolIdMismatch { actual: rhs_actual },
            ) => lhs_actual == rhs_actual,
            (
                Self::UnitIdMismatch {
                    expected: lhs_expected,
                    actual: lhs_actual,
                },
                Self::UnitIdMismatch {
                    expected: rhs_expected,
                    actual: rhs_actual,
                },
            ) => lhs_expected == rhs_expected && lhs_actual == rhs_actual,
            _ => false,
        }
    }
}

fn io_errors_equal(lhs: &std::io::Error, rhs: &std::io::Error) -> bool {
    lhs.kind() == rhs.kind() && lhs.to_string() == rhs.to_string()
}

impl ModbusError {
    /// Returns `true` if this error represents a transient transport failure
    /// that callers may safely retry (connect, read, or write I/O failures,
    /// malformed TCP response frames, and their associated timeouts).
    ///
    /// Malformed TCP response frames are treated as transient because a corrupt or
    /// desynchronized stream can often be recovered by reconnecting and retrying
    /// on a fresh socket. Protocol-level and validation errors are considered
    /// permanent.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::ConnectError(_)
                | Self::ConnectTimeout
                | Self::WriteError(_)
                | Self::WriteTimeout
                | Self::ReadError(_)
                | Self::ReadTimeout
                | Self::QueueTimeout
                | Self::MalformedResponse(_)
        )
    }

    /// Returns true for Modbus exception codes that indicate temporary gateway/slave busy states.
    #[must_use]
    pub fn is_gateway_busy(&self) -> bool {
        matches!(
            self,
            Self::ExceptionResponse { code, .. } if matches!(code, 0x05 | 0x06 | 0x0A | 0x0B)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modbus_error_equality_compares_payloads() {
        assert_eq!(ModbusError::ConnectTimeout, ModbusError::ConnectTimeout);
        assert_eq!(
            ModbusError::ValidationError("bad quantity".to_string()),
            ModbusError::ValidationError("bad quantity".to_string())
        );
        assert_ne!(
            ModbusError::ValidationError("left".to_string()),
            ModbusError::ValidationError("right".to_string())
        );
        assert_eq!(
            ModbusError::ExceptionResponse {
                function: 0x83,
                code: 0x06,
            },
            ModbusError::ExceptionResponse {
                function: 0x83,
                code: 0x06,
            }
        );
    }

    #[test]
    fn io_error_equality_compares_kind_and_message() {
        assert_eq!(
            ModbusError::ReadError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "read timed out",
            ))),
            ModbusError::ReadError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "read timed out",
            )))
        );
        assert_ne!(
            ModbusError::ReadError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "read timed out",
            ))),
            ModbusError::ReadError(Arc::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "read timed out",
            )))
        );
    }

    #[test]
    fn gateway_busy_exception_codes_are_classified() {
        for code in [0x05, 0x06, 0x0A, 0x0B] {
            assert!(
                ModbusError::ExceptionResponse {
                    function: 0x83,
                    code
                }
                .is_gateway_busy()
            );
        }
        assert!(
            !ModbusError::ExceptionResponse {
                function: 0x83,
                code: 0x02
            }
            .is_gateway_busy()
        );
    }

    #[test]
    fn queue_timeout_is_transient() {
        assert!(ModbusError::QueueTimeout.is_transient());
    }
}
