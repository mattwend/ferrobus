// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

#![deny(missing_docs)]

//! `tiny-mb` provides small, explicit Modbus request/response types plus a
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
pub mod request;
pub use request::ModbusRequest;

/// Typed Modbus response PDUs and deserialization.
pub mod response;
pub use response::ModbusResponse;

pub mod tcp;
