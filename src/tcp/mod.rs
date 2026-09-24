// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Modbus TCP framing and transport helpers.
//!
//! The public construction surface is split into two phases and
//! [`ModbusTcpSocket`] is its only entry point: a socket stores configurable
//! settings without opening a network connection, then one of two consuming
//! methods validates those settings, spawns the background actor, and returns a
//! live [`ModbusTcpConnection`] handle. [`ModbusTcpSocket::connect`] eagerly
//! opens the first TCP connection and fails if it cannot, discarding the actor
//! it just spawned. [`ModbusTcpSocket::spawn`] is the variant for callers that
//! need the handle before the peer is reachable: it validates and spawns without
//! dialing, leaving the first connect to the actor's on-demand connect or to an
//! explicit [`ModbusTcpConnection::connect`], so the same actor can be retried
//! after a failure. Internal submodules keep ADU/MBAP frame construction,
//! actor-based socket ownership, MBAP codec framing, and retry/timeout
//! configuration separated by concern.
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
mod tcp_server;
pub use flow::ModbusTcpFlowControl;
pub use retry::ModbusTcpRetry;
pub use socket::ModbusTcpSocket;
pub use status::ConnectionStatus;
pub use tcp_connection::ModbusTcpConnection;
pub use tcp_server::{BoundModbusTcpServer, ModbusTcpServer, ModbusTcpServerTimeouts};
pub use timeouts::ModbusTcpTimeouts;
