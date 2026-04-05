// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use thiserror::Error;

/// Custom error type for the Modbus library.
#[derive(Error, Debug)]
pub enum ModbusError {
    #[error("TCP connection error: {0}")]
    ConnectionError(#[from] std::io::Error),

    #[error("Response error: {0}")]
    ResponseError(String),

    #[error("Deserialization error: {0}")]
    DeserializationError(String),

    #[error("Transaction ID mismatch: sent {expected}, received {actual}")]
    TransactionIdMismatch { expected: u16, actual: u16 },

    #[error("Request/response mismatch: {0}")]
    RequestResponseMismatch(String),
}
