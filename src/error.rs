// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use thiserror::Error;

/// Custom error type for the Modbus library.
#[derive(Error, Debug)]
pub enum ModbusError {
    #[error("TCP connection error: {0}")]
    ConnectionError(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Response error: {0}")]
    ResponseError(String),

    #[error("Deserialization error: {0}")]
    DeserializationError(String),

    #[error("Unexpected response length: expected at least {expected}, got {actual}")]
    ResponseTooShort { expected: usize, actual: usize },

    #[error("Backoff operation failed after retries: {0}")]
    BackoffError(String),

    #[error("Transaction ID mismatch: sent {expected}, received {actual}")]
    TransactionIdMismatch { expected: u16, actual: u16 },
}
