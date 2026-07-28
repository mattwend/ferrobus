// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Observable connection state published by the Modbus TCP actor.

/// Observable connection state of the actor owning the socket.
///
/// A value of this type is published by the actor on every socket
/// establishment and every teardown that closes an open socket, and can be
/// observed through
/// [`ModbusTcpConnection::status`](crate::tcp::ModbusTcpConnection::status) or
/// subscribed to through
/// [`ModbusTcpConnection::watch_status`](crate::tcp::ModbusTcpConnection::watch_status).
///
/// # Examples
///
/// ```
/// use ferrobus::tcp::ConnectionStatus;
///
/// let before_connect = ConnectionStatus {
///     connected: false,
///     generation: 0,
/// };
/// assert!(!before_connect.connected);
/// assert_eq!(before_connect.generation, 0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionStatus {
    /// Whether a socket is currently open.
    pub connected: bool,
    /// Counts successful socket establishments on this actor.
    ///
    /// Starts at `0` before the first connect and increments by one each time a
    /// socket is opened, including reconnects after a teardown and explicit
    /// [`ModbusTcpConnection::connect`](crate::tcp::ModbusTcpConnection::connect)
    /// calls following a
    /// [`ModbusTcpConnection::disconnect`](crate::tcp::ModbusTcpConnection::disconnect).
    /// It never decreases, a teardown leaves it unchanged, and a failed connect
    /// attempt leaves it unchanged.
    ///
    /// A changed `generation` therefore means the socket was replaced,
    /// regardless of how many drop/reconnect cycles happened in between and
    /// regardless of when the observer sampled. Callers that negotiate device
    /// state over a connection (word order, scaling calibration, a device-side
    /// watchdog) should cache the generation alongside that state and
    /// invalidate it when the value moves.
    ///
    /// The counter is per *actor*: a handle obtained from a new
    /// [`ModbusTcpSocket::connect`](crate::tcp::ModbusTcpSocket::connect) starts
    /// its own count at `1`. Clones of a handle (including
    /// [`ModbusTcpConnection::with_unit_id`](crate::tcp::ModbusTcpConnection::with_unit_id))
    /// share one actor and therefore one counter.
    pub generation: u64,
}

impl ConnectionStatus {
    /// State published before the first socket is opened.
    pub(crate) const fn disconnected() -> Self {
        Self {
            connected: false,
            generation: 0,
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Pins the literal initial state. This is the only assertion in the crate
    /// that is not written against `ConnectionStatus::disconnected()` itself, so
    /// it is the test that fails if the constructor ever stops meaning
    /// "no socket has been opened yet".
    #[test]
    fn disconnected_starts_at_generation_zero() {
        let status = ConnectionStatus::disconnected();

        assert!(!status.connected);
        assert_eq!(status.generation, 0);
    }
}
