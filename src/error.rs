// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use thiserror::Error;

/// Custom error type for the Modbus library.
#[derive(Error, Debug)]
pub enum ModbusError {
    #[error("TCP connection error: {0}")]
    ConnectionError(#[from] std::io::Error),

    #[error("Lock error: {0}")]
    LockError(String),

    #[error("ADU build error: {0}")]
    AduBuildError(String),

    #[error("Response error: {0}")]
    ResponseError(String),

    #[error("Deserialization error: {0}")]
    DeserializationError(String),

    #[error("Unexpected response length: expected at least {expected}, got {actual}")]
    ResponseTooShort { expected: usize, actual: usize },

    #[error("Backoff operation failed after retries: {0}")]
    BackoffError(String),
}
