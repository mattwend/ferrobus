// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Modbus TCP framing and transport helpers.
//!
//! The public surface exports the reusable connection handle plus its retry and
//! timeout configuration. Internal submodules keep ADU/MBAP frame construction,
//! actor-based socket ownership, MBAP codec framing, and retry/timeout
//! configuration separated by concern.

mod actor;
mod adu;
mod codec;
mod flow;
mod frame;
mod retry;
mod timeouts;

mod tcp_connection;
pub use flow::ModbusTcpFlowControl;
pub use retry::ModbusTcpRetry;
pub use tcp_connection::ModbusTcpConnection;
pub use timeouts::ModbusTcpTimeouts;
