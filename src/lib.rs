// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

#![deny(missing_docs)]

//! `ferrobus` provides small, explicit Modbus request/response types plus a
//! Modbus TCP transport.
//!
//! The crate is organized around typed PDUs:
//! - [`ModbusRequest`] for building requests
//! - [`ModbusResponse`] for parsing responses
//! - [`tcp::ModbusTcpConnection`] for sending requests over Modbus TCP
//! - [`tcp::ConnectionStatus`] for observing the connection lifecycle
//! - [`server::ModbusServer`] and [`tcp::ModbusTcpServer`] for serving Modbus TCP

/// Error types returned by this crate.
pub mod error;
pub use error::ModbusError;

mod limits;

/// Typed Modbus request PDUs and serialization.
mod request;
pub use request::{ModbusRequest, RequestParseError};

/// Typed Modbus response PDUs and deserialization.
mod response;
pub use response::{ExceptionCode, FunctionCode, InvalidFunctionCode, ModbusResponse};

/// Multi-register word-order helpers for wide scalar values.
mod word_order;
pub use word_order::{WordOrder, WordOrderError};

/// Server abstractions and reference data stores.
pub mod server;

pub mod tcp;
pub use tcp::ConnectionStatus;
