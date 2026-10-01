// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Configurable Modbus TCP socket phase.

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::error::ModbusError;
use crate::tcp::actor::{Actor, RequestCommand, control_channel_capacity};
use crate::tcp::flow::ModbusTcpFlowControl;
use crate::tcp::retry::ModbusTcpRetry;
use crate::tcp::status::ConnectionStatus;
use crate::tcp::tcp_connection::ModbusTcpConnection;
use crate::tcp::timeouts::ModbusTcpTimeouts;

/// Configurable Modbus TCP connection phase.
///
/// A socket stores construction-time settings as plain configuration. Creating
/// or modifying a socket does not spawn the background actor, resolve `host`, or
/// open a TCP connection. Use [`Self::with_timeouts`] for connect/write/response
/// deadlines, [`Self::with_flow_control`] for in-flight and queue limits,
/// [`Self::with_retry`] for same-call send retry, and
/// [`Self::with_initial_transaction_id`] when a diagnostic caller needs a custom
/// MBAP transaction-id seed.
///
/// This is the only public way to construct a [`ModbusTcpConnection`]. Both
/// construction paths consume the socket, validate flow control, spawn the
/// actor, and return a live handle; they differ only in when the first TCP
/// connection is opened. [`ModbusTcpSocket::connect`] opens it eagerly and fails
/// if it cannot, while [`ModbusTcpSocket::spawn`] opens none, so the handle can
/// exist before the peer is reachable. The eager path is defined as the
/// composition of the other two:
///
/// ```no_run
/// use ferrobus::tcp::ModbusTcpSocket;
///
/// # async fn run(socket: ModbusTcpSocket) -> Result<(), ferrobus::ModbusError> {
/// let connection = socket.spawn()?;
/// connection.connect().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct ModbusTcpSocket {
    host: String,
    port: u16,
    unit_id: u8,
    transaction_id: u16,
    timeouts: ModbusTcpTimeouts,
    flow_control: ModbusTcpFlowControl,
    retry: Option<ModbusTcpRetry>,
}

impl ModbusTcpSocket {
    /// Creates a configurable socket with default construction settings.
    ///
    /// `host` may be a DNS name or numeric IP address and is not resolved until
    /// the actor dials, which is the eager connect of [`Self::connect`] or the
    /// first connect after [`Self::spawn`]. `port` is the TCP port used for every actor
    /// dial, and `unit_id` is the default Modbus unit id used by the live
    /// connection. The initial transaction-id seed defaults to `1`; timeouts,
    /// flow control, and retry use their documented defaults.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16, unit_id: u8) -> Self {
        Self {
            host: host.into(),
            port,
            unit_id,
            transaction_id: 1,
            timeouts: ModbusTcpTimeouts::default(),
            flow_control: ModbusTcpFlowControl::default(),
            retry: Some(ModbusTcpRetry::default()),
        }
    }

    /// Overrides the connect, write, and response timeouts used by the actor.
    ///
    /// `timeouts` is stored on the configurable socket and handed to the actor
    /// when it is spawned. Returns the updated socket.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: ModbusTcpTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Overrides the actor flow-control settings.
    ///
    /// `flow_control` configures queue depth, in-flight request capacity, queue
    /// timeout, and quarantine lifetime. It is validated when the actor is
    /// spawned, before any channel is allocated. Returns the updated socket.
    #[must_use]
    pub fn with_flow_control(mut self, flow_control: ModbusTcpFlowControl) -> Self {
        self.flow_control = flow_control;
        self
    }

    /// Overrides the same-call retry policy used by the live connection.
    ///
    /// `retry` is copied into the live [`ModbusTcpConnection`]. Pass `None` to
    /// disable retry for sends through that handle. Returns the updated socket.
    #[must_use]
    pub fn with_retry(mut self, retry: Option<ModbusTcpRetry>) -> Self {
        self.retry = retry;
        self
    }

    /// Overrides the MBAP transaction-id counter seed.
    ///
    /// `transaction_id` is the id used by the first request after connection,
    /// with subsequent requests incrementing from it with wrap-around. Returns
    /// the updated socket.
    #[must_use]
    pub fn with_initial_transaction_id(mut self, transaction_id: u16) -> Self {
        self.transaction_id = transaction_id;
        self
    }

    /// Spawns the actor, eagerly opens the first TCP connection, and returns a live handle.
    ///
    /// Flow control is validated before channels are allocated. On success, the
    /// actor is spawned and [`ModbusTcpConnection::connect`] performs an eager
    /// warm-up connect before the live [`ModbusTcpConnection`] is returned. The
    /// returned handle therefore reports
    /// [`ConnectionStatus`](crate::tcp::ConnectionStatus) `{ connected: true,
    /// generation: 1 }`.
    ///
    /// This is the eager construction path and the normal choice. Because the
    /// handle is only returned on success, a failed initial dial also discards
    /// the actor that was just spawned. A caller that must retain the actor and
    /// retry through it has to use [`Self::spawn`] followed by
    /// [`ModbusTcpConnection::connect`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] if flow-control settings are
    /// invalid. Returns [`ModbusError::ConnectError`] or
    /// [`ModbusError::ConnectTimeout`] if the first TCP connection cannot be
    /// opened, or a transport error if the actor terminates before acknowledging
    /// the warm-up command.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ferrobus::tcp::ModbusTcpSocket;
    ///
    /// # async fn run() -> Result<(), ferrobus::ModbusError> {
    /// let connection = ModbusTcpSocket::new("127.0.0.1", 502, 1)
    ///     .connect()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect(self) -> Result<ModbusTcpConnection, ModbusError> {
        let connection = self.spawn()?;
        connection.connect().await?;
        Ok(connection)
    }

    /// Spawns the actor and returns a live handle without opening a TCP connection.
    ///
    /// Flow control is validated before channels are allocated, and the actor is
    /// spawned on the current Tokio runtime, so this must be called from within a
    /// runtime context. No dial is attempted: `host` is not resolved and the
    /// returned handle reports
    /// [`ConnectionStatus`](crate::tcp::ConnectionStatus) `{ connected: false,
    /// generation: 0 }`.
    ///
    /// The socket is opened by the actor's on-demand connect when the first
    /// request is dispatched, or eagerly by [`ModbusTcpConnection::connect`].
    /// Use this instead of [`Self::connect`] when handle construction must be
    /// infallible with respect to reachability — a supervised transport that is
    /// built before its lifecycle decides when to dial, for example.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] if flow-control settings are
    /// invalid.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ferrobus::tcp::ModbusTcpSocket;
    ///
    /// # async fn run() -> Result<(), ferrobus::ModbusError> {
    /// let connection = ModbusTcpSocket::new("127.0.0.1", 502, 1).spawn()?;
    /// let mut status = connection.watch_status();
    ///
    /// // Later, when lifecycle policy permits dialing:
    /// connection.connect().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn spawn(self) -> Result<ModbusTcpConnection, ModbusError> {
        self.flow_control.validate()?;
        let (connection, _actor_task) = self.spawn_actor();
        Ok(connection)
    }

    pub(crate) fn spawn_actor(self) -> (ModbusTcpConnection, JoinHandle<()>) {
        let (req_tx, req_rx) = mpsc::channel::<RequestCommand>(self.flow_control.max_queue_depth);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(control_channel_capacity());
        let (connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        let actor = Actor::new(
            self.host,
            self.port,
            self.timeouts,
            self.flow_control,
            self.transaction_id,
            ctrl_rx,
            req_rx,
            connected_tx,
        );
        let actor_task = tokio::spawn(actor.run());
        let connection = ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            self.unit_id,
            connected,
            self.flow_control.queue_timeout,
            self.retry,
        );
        (connection, actor_task)
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn new_stores_defaults_without_spawning() {
        let socket = ModbusTcpSocket::new("localhost", 502, 7);

        assert_eq!(socket.host, "localhost");
        assert_eq!(socket.port, 502);
        assert_eq!(socket.unit_id, 7);
        assert_eq!(socket.transaction_id, 1);
        assert_eq!(socket.timeouts, ModbusTcpTimeouts::default());
        assert_eq!(socket.flow_control, ModbusTcpFlowControl::default());
        assert_eq!(socket.retry, Some(ModbusTcpRetry::default()));
    }

    #[test]
    fn modifiers_store_custom_settings() {
        let timeouts = ModbusTcpTimeouts {
            connect_timeout: Duration::from_millis(11),
            write_timeout: Duration::from_millis(12),
            response_timeout: Duration::from_millis(13),
        };
        let flow_control = ModbusTcpFlowControl {
            max_in_flight: 2,
            max_queue_depth: 3,
            queue_timeout: Duration::from_millis(14),
            quarantine_ttl: Duration::from_millis(15),
        };
        let retry = ModbusTcpRetry {
            initial_delay: Duration::from_millis(1),
            max_delay: Some(Duration::from_millis(2)),
            multiplier: 1.5,
            max_elapsed: Duration::from_millis(16),
            max_times: Some(2),
            jitter: false,
            retry_gateway_busy: false,
        };

        let socket = ModbusTcpSocket::new("127.0.0.1", 502, 1)
            .with_timeouts(timeouts)
            .with_flow_control(flow_control)
            .with_retry(Some(retry))
            .with_initial_transaction_id(42);

        assert_eq!(socket.timeouts, timeouts);
        assert_eq!(socket.flow_control, flow_control);
        assert_eq!(socket.retry, Some(retry));
        assert_eq!(socket.transaction_id, 42);

        let socket = socket.with_retry(None);
        assert_eq!(socket.retry, None);
    }

    /// The eager path must return a handle whose first socket is already open, which is
    /// the generation the connection contract counts from.
    #[tokio::test]
    async fn connect_success_returns_connected_handle() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .connect()
            .await
            .unwrap();

        assert_eq!(
            connection.status(),
            ConnectionStatus {
                connected: true,
                generation: 1,
            }
        );
        assert!(connection.is_connected().await);
    }

    /// A spawned handle must be usable before the peer exists: no dial is attempted,
    /// and the status reports the pre-connect state the generation contract starts from.
    /// A reachable peer is offered so the absence of a dial is observed rather than
    /// inferred from a status a background connect could still race.
    #[tokio::test]
    async fn spawn_returns_a_live_handle_without_dialing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .spawn()
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "spawn must not dial the peer"
        );
        assert_eq!(connection.status(), ConnectionStatus::disconnected());
        assert!(!connection.is_connected().await);
    }

    /// `spawn` must not resolve the host either: a handle for a name that cannot be
    /// resolved is still constructed, so name resolution is deferred to the first dial
    /// along with the connect itself.
    #[tokio::test]
    async fn spawn_does_not_resolve_the_host() {
        let connection = ModbusTcpSocket::new("host.invalid", 502, 1)
            .spawn()
            .unwrap();

        assert_eq!(connection.status(), ConnectionStatus::disconnected());
    }

    /// The socket a spawned handle never dialed is opened by the first explicit connect,
    /// which is the same actor path a reconnect takes.
    #[tokio::test]
    async fn a_spawned_handle_connects_through_its_own_actor() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .spawn()
            .unwrap();
        connection.connect().await.unwrap();

        assert_eq!(
            connection.status(),
            ConnectionStatus {
                connected: true,
                generation: 1,
            }
        );
    }

    /// Invalid flow control is a construction bug and must be rejected before any
    /// channel or task exists, on both construction paths.
    #[test]
    fn spawn_validates_flow_control() {
        let flow_control = ModbusTcpFlowControl {
            max_in_flight: 0,
            ..ModbusTcpFlowControl::default()
        };

        let result = ModbusTcpSocket::new("127.0.0.1", 502, 1)
            .with_flow_control(flow_control)
            .spawn();

        assert!(matches!(result, Err(ModbusError::ValidationError(_))));
    }

    #[tokio::test]
    async fn connect_validates_flow_control() {
        let flow_control = ModbusTcpFlowControl {
            max_in_flight: 0,
            ..ModbusTcpFlowControl::default()
        };

        let result = ModbusTcpSocket::new("127.0.0.1", 502, 1)
            .with_flow_control(flow_control)
            .connect()
            .await;

        assert!(matches!(result, Err(ModbusError::ValidationError(_))));
    }
}
