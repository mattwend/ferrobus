// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Modbus TCP framing and transport helpers.
//!
//! The public construction surface is split into two phases:
//! [`ModbusTcpSocket`] stores configurable settings without opening a network
//! connection, then [`ModbusTcpSocket::connect`] validates those settings,
//! eagerly opens the first TCP connection, and returns a live
//! [`ModbusTcpConnection`] actor handle. [`ModbusTcpConnection::connect`] is the
//! default-configuration shortcut. Internal submodules keep ADU/MBAP frame
//! construction, actor-based socket ownership, MBAP codec framing, and
//! retry/timeout configuration separated by concern.

mod actor;
mod adu;
mod codec;
mod flow;
mod frame;
mod retry;
mod socket;
mod timeouts;

mod tcp_connection;
pub use flow::ModbusTcpFlowControl;
pub use retry::ModbusTcpRetry;
pub use socket::ModbusTcpSocket;
pub use tcp_connection::ModbusTcpConnection;
pub use timeouts::ModbusTcpTimeouts;
