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

/// Error types returned by this crate.
pub mod error;
pub use error::ModbusError;

/// Typed Modbus request PDUs and serialization.
mod request;
pub use request::ModbusRequest;

/// Typed Modbus response PDUs and deserialization.
mod response;
pub use response::ModbusResponse;

pub mod tcp;
