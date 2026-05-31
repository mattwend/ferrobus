// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Modbus TCP framing and transport helpers.
//!
//! The public surface exports ADU construction plus the reusable connection
//! handle and timeout configuration. Internal submodules keep MBAP frame
//! parsing, connected-state reader lifecycle, pending request tracking, and
//! cancellation-safe writes separated by concern.

/// Modbus TCP ADU frame construction.
pub mod adu;
mod connected_state;
mod frame;
mod pending;
mod retry;
mod timeouts;
mod writer;
pub use adu::build_modbus_tcp_adu;

/// Reusable Modbus TCP connection type.
pub mod tcp_connection;
pub use tcp_connection::ModbusTcpConnection;
pub use timeouts::ModbusTcpTimeouts;
