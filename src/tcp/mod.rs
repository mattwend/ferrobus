// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Modbus TCP framing and transport helpers.
//!
//! The public construction surface is split into two phases:
//! [`ModbusTcpSocket`] stores configurable settings without opening a network
//! connection, then [`ModbusTcpSocket::connect`] validates those settings,
//! eagerly opens the first TCP connection, and returns a live
//! [`ModbusTcpConnection`] actor handle. [`ModbusTcpConnection::open`] is the
//! default-configuration shortcut. Internal submodules keep ADU/MBAP frame
//! construction, actor-based socket ownership, MBAP codec framing, and
//! retry/timeout configuration separated by concern.
//!
//! The live handle also owns the connection lifecycle:
//! [`ModbusTcpConnection::connect`] and [`ModbusTcpConnection::disconnect`]
//! drive the actor's socket, while [`ConnectionStatus`] — observed through
//! [`ModbusTcpConnection::status`] or
//! [`ModbusTcpConnection::watch_status`] — reports whether a socket is open and
//! how many sockets this actor has established.

mod actor;
mod adu;
mod codec;
mod flow;
mod frame;
mod retry;
mod socket;
mod status;
#[cfg(test)]
mod test_support;
mod timeouts;

mod tcp_connection;
pub use flow::ModbusTcpFlowControl;
pub use retry::ModbusTcpRetry;
pub use socket::ModbusTcpSocket;
pub use status::ConnectionStatus;
pub use tcp_connection::ModbusTcpConnection;
pub use timeouts::ModbusTcpTimeouts;
