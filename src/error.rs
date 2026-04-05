// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use thiserror::Error;

/// Custom error type for the Modbus library.
#[derive(Error, Debug)]
pub enum ModbusError {
    #[error("TCP connect error: {0}")]
    ConnectError(std::io::Error),

    #[error("TCP connect timed out")]
    ConnectTimeout,

    #[error("TCP write error: {0}")]
    WriteError(std::io::Error),

    #[error("TCP write timed out")]
    WriteTimeout,

    #[error("TCP read error: {0}")]
    ReadError(std::io::Error),

    #[error("TCP read timed out")]
    ReadTimeout,

    #[error("Malformed response: {0}")]
    MalformedResponse(String),

    #[error("Deserialization error: {0}")]
    DeserializationError(String),

    #[error("Modbus exception response: function {function:#04x}, code {code:#04x}")]
    ExceptionResponse { function: u8, code: u8 },

    #[error("Transaction ID mismatch: sent {expected}, received {actual}")]
    TransactionIdMismatch { expected: u16, actual: u16 },

    #[error("Protocol ID mismatch: expected 0, received {actual}")]
    ProtocolIdMismatch { actual: u16 },

    #[error("Unit ID mismatch: expected {expected}, received {actual}")]
    UnitIdMismatch { expected: u8, actual: u8 },

    #[error("Request/response mismatch: {0}")]
    RequestResponseMismatch(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}
