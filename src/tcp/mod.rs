// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Modbus TCP framing and transport helpers.

pub mod adu;
pub use adu::build_modbus_tcp_adu;

pub mod tcp_connection;
pub use tcp_connection::{ModbusTcpConnection, ModbusTcpTimeouts};
