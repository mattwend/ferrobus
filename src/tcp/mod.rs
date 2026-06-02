// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Modbus TCP framing and transport helpers.
//!
//! The public surface exports the reusable connection handle plus its retry and
//! timeout configuration. Internal submodules keep ADU/MBAP frame construction,
//! connected-state reader lifecycle, pending request tracking, and
//! cancellation-safe writes separated by concern.

mod actor;
mod adu;
mod codec;
mod connected_state;
mod frame;
mod pending;
mod retry;
mod timeouts;
mod writer;

mod tcp_connection;
pub use retry::ModbusTcpRetry;
pub use tcp_connection::ModbusTcpConnection;
pub use timeouts::ModbusTcpTimeouts;
