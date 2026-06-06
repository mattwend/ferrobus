// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use thiserror::Error;

/// Custom error type for the Modbus library.
#[derive(Error, Debug)]
pub enum ModbusError {
    /// TCP connection establishment failed.
    #[error("TCP connect error: {0}")]
    ConnectError(std::io::Error),

    /// TCP connection establishment exceeded the configured timeout.
    #[error("TCP connect timed out")]
    ConnectTimeout,

    /// Writing bytes to the TCP stream failed.
    #[error("TCP write error: {0}")]
    WriteError(std::io::Error),

    /// Writing a frame exceeded the configured timeout.
    #[error("TCP write timed out")]
    WriteTimeout,

    /// Reading bytes from the TCP stream failed.
    #[error("TCP read error: {0}")]
    ReadError(std::io::Error),

    /// Reading a frame exceeded the configured timeout.
    #[error("TCP read timed out")]
    ReadTimeout,

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
                | Self::MalformedResponse(_)
        )
    }
}
