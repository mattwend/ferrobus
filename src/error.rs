// tinymb - A simple Modbus library for Rust
// Copyright (C) 2025 tinymb authors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

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
