// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! `tiny-mb` provides small, explicit Modbus request/response types plus a
//! Modbus TCP transport.
//!
//! The crate is organized around typed PDUs:
//! - [`ModbusRequest`] for building requests
//! - [`ModbusResponse`] for parsing responses
//! - [`tcp::ModbusTcpConnection`] for sending requests over Modbus TCP

pub mod error;
pub use error::ModbusError;

pub mod request;
pub use request::ModbusRequest;

pub mod response;
pub use response::ModbusResponse;

pub mod tcp;

#[cfg(feature = "test-support")]
pub mod test_support;
