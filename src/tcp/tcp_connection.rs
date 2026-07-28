// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Public Modbus TCP connection handle and request orchestration.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::debug;

use crate::tcp::actor::{ControlCommand, RequestCommand};
use crate::tcp::frame::MBAP_HEADER_LEN;
use crate::tcp::retry::{ModbusTcpRetry, retry_transient};
use crate::tcp::socket::ModbusTcpSocket;
use crate::tcp::status::ConnectionStatus;
#[cfg(test)]
use crate::tcp::timeouts::ModbusTcpTimeouts;
use crate::{ModbusRequest, ModbusResponse, error::ModbusError};

/// Live Modbus TCP client handle.
///
/// Construct a handle with [`ModbusTcpSocket::connect`] when custom timeouts,
/// flow control, retry, or an initial transaction-id seed are needed. Use
/// [`ModbusTcpConnection::open`] for the default configuration shortcut. Both
/// paths spawn the background actor and eagerly open the first TCP connection
/// before returning this live handle.
///
/// Clones are lightweight command senders to a single background actor that owns
/// the TCP socket, pending response map, and transaction-id counter. The actor
/// reconnects lazily after transport teardown, applies bounded in-flight flow
/// control, and uses a bounded request channel as backpressure. Queue wait is
/// bounded by flow-control settings; response timeout starts when the actor
/// writes the request on the socket. Use [`Self::with_unit_id`] to derive another
/// live handle that shares the same actor with a different default unit id.
///
/// The connection lifecycle is driven from this handle: [`Self::connect`] and
/// [`Self::disconnect`] open and close the actor's socket without replacing the
/// actor, and [`Self::status`] / [`Self::watch_status`] report the current
/// [`ConnectionStatus`], including a generation counter that identifies the
/// socket currently in use.
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    req_tx: mpsc::Sender<RequestCommand>,
    ctrl_tx: mpsc::Sender<ControlCommand>,
    unit_id: u8,
    connected: watch::Receiver<ConnectionStatus>,
    queue_timeout: Duration,
    retry: Option<ModbusTcpRetry>,
}

impl ModbusTcpConnection {
    pub(crate) fn from_actor_parts(
        req_tx: mpsc::Sender<RequestCommand>,
        ctrl_tx: mpsc::Sender<ControlCommand>,
        unit_id: u8,
        connected: watch::Receiver<ConnectionStatus>,
        queue_timeout: Duration,
        retry: Option<ModbusTcpRetry>,
    ) -> Self {
        Self {
            req_tx,
            ctrl_tx,
            unit_id,
            connected,
            queue_timeout,
            retry,
        }
    }

    /// Opens a connection to a Modbus TCP server with default construction settings.
    ///
    /// `host` may be a DNS name or numeric IP address. `port` is the TCP port,
    /// and `unit_id` is the default Modbus unit id used by the returned live
    /// handle. This is a shortcut for configuring a [`ModbusTcpSocket`] with
    /// defaults and awaiting [`ModbusTcpSocket::connect`].
    ///
    /// This associated function was named `connect` before `0.2.0`; that name
    /// now belongs to the live-handle method [`Self::connect`].
    ///
    /// # Errors
    ///
    /// Returns validation, connection, timeout, or actor-termination errors from
    /// [`ModbusTcpSocket::connect`].
    pub async fn open(
        host: impl Into<String>,
        port: u16,
        unit_id: u8,
    ) -> Result<Self, ModbusError> {
        ModbusTcpSocket::new(host, port, unit_id).connect().await
    }

    /// Returns a new handle with a different default unit id.
    ///
    /// The returned handle shares the same background actor, TCP session,
    /// in-flight request window, and retry policy. It does not spawn another
    /// actor or open another TCP connection.
    #[must_use]
    pub fn with_unit_id(&self, unit_id: u8) -> Self {
        Self {
            req_tx: self.req_tx.clone(),
            ctrl_tx: self.ctrl_tx.clone(),
            unit_id,
            connected: self.connected.clone(),
            queue_timeout: self.queue_timeout,
            retry: self.retry,
        }
    }

    pub(crate) async fn connect_stream(
        host: &str,
        port: u16,
        connect_timeout: Duration,
    ) -> Result<TcpStream, ModbusError> {
        let stream = timeout(connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| ModbusError::ConnectTimeout)?
            .map_err(|error| ModbusError::ConnectError(Arc::new(error)))?;
        debug!(host, port, "connected to Modbus TCP server");
        Ok(stream)
    }

    /// Opens the TCP session for this handle's actor if one is not already open.
    ///
    /// The method waits until the actor has processed the connect command. It is
    /// idempotent: if a socket is already open the actor acknowledges
    /// immediately without dialing again, leaving
    /// [`ConnectionStatus::generation`] unchanged. A successful dial increments
    /// the generation by one.
    ///
    /// Unlike [`Self::open`], this reuses the existing background actor. No
    /// second actor is spawned and every clone of this handle, including those
    /// from [`Self::with_unit_id`], observes the reopened socket.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ConnectError`] or [`ModbusError::ConnectTimeout`]
    /// if the socket cannot be opened, or a transport error if the actor has
    /// already terminated.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ferrobus::tcp::ModbusTcpConnection;
    ///
    /// # async fn run() -> Result<(), ferrobus::ModbusError> {
    /// let connection = ModbusTcpConnection::open("127.0.0.1", 502, 1).await?;
    /// let before = connection.status().generation;
    /// connection.disconnect().await;
    ///
    /// // Reopen the same actor's socket: the generation advances by one, so any
    /// // device state negotiated over the previous socket must be re-negotiated.
    /// connection.connect().await?;
    /// let after = connection.status().generation;
    /// println!("socket replaced: generation {before} -> {after}");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect(&self) -> Result<(), ModbusError> {
        let (ack, reply) = oneshot::channel();
        self.ctrl_tx
            .send(ControlCommand::Connect { ack })
            .await
            .map_err(|_| actor_terminated_error())?;
        reply.await.map_err(|_| actor_terminated_error())?
    }

    /// Closes the current TCP session if one is open.
    ///
    /// The method waits until the actor has processed the disconnect command,
    /// making subsequent `is_connected` reads observe the cleared state unless
    /// another handle reconnects afterward. A teardown never changes
    /// [`ConnectionStatus::generation`].
    pub async fn disconnect(&self) {
        let (ack, processed) = oneshot::channel();
        if let Err(error) = self.ctrl_tx.send(ControlCommand::Disconnect { ack }).await {
            debug!(%error, "connection actor already stopped during disconnect");
            return;
        }
        if processed.await.is_err() {
            debug!("connection actor stopped before acknowledging disconnect");
        }
    }

    /// Returns the current connection status of this handle's actor.
    ///
    /// This is a snapshot of the value most recently published by the actor.
    /// Cache [`ConnectionStatus::generation`] alongside any device state
    /// negotiated over the connection and re-negotiate when it changes: a moved
    /// generation means the socket was replaced and the peer may have rebooted.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ferrobus::tcp::ModbusTcpConnection;
    ///
    /// # async fn run() -> Result<(), ferrobus::ModbusError> {
    /// let connection = ModbusTcpConnection::open("127.0.0.1", 502, 1).await?;
    /// let mut negotiated_at = connection.status().generation;
    ///
    /// // ... later, before trusting negotiated device state ...
    /// let current = connection.status();
    /// if current.generation != negotiated_at {
    ///     // The socket was replaced; re-negotiate before resuming traffic and
    ///     // record the generation the fresh state belongs to.
    ///     negotiated_at = current.generation;
    /// }
    /// # let _ = negotiated_at;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn status(&self) -> ConnectionStatus {
        *self.connected.borrow()
    }

    /// Subscribes to connection status changes.
    ///
    /// The returned receiver starts with the current status already marked as
    /// seen, so the first
    /// [`changed`](tokio::sync::watch::Receiver::changed) resolves on the next
    /// transition. The actor publishes on every socket establishment and every
    /// teardown, so a drop and the following reconnect are two distinct
    /// changes. A subscriber that only samples after both still sees a changed
    /// generation. Publications that would repeat the current status are
    /// suppressed, so an idempotent [`Self::disconnect`] does not resolve
    /// `changed`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use ferrobus::tcp::ModbusTcpConnection;
    ///
    /// # async fn run() -> Result<(), ferrobus::ModbusError> {
    /// let connection = ModbusTcpConnection::open("127.0.0.1", 502, 1).await?;
    /// let mut status = connection.watch_status();
    ///
    /// tokio::spawn(async move {
    ///     while status.changed().await.is_ok() {
    ///         let current = *status.borrow_and_update();
    ///         println!(
    ///             "connected={} generation={}",
    ///             current.connected, current.generation
    ///         );
    ///     }
    /// });
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn watch_status(&self) -> watch::Receiver<ConnectionStatus> {
        let mut status = self.connected.clone();
        status.mark_unchanged();
        status
    }

    /// Returns whether this handle currently owns an open TCP stream.
    ///
    /// Equivalent to [`Self::status`]`().connected`.
    #[allow(clippy::unused_async)]
    pub async fn is_connected(&self) -> bool {
        self.status().connected
    }

    async fn send_once(
        &self,
        unit_id: u8,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        let (reply, response) = oneshot::channel();
        let command = RequestCommand {
            unit_id,
            pdu: pdu.clone(),
            reply,
            queue_deadline: tokio::time::Instant::now() + self.queue_timeout,
        };
        self.req_tx
            .send(command)
            .await
            .map_err(|_| actor_terminated_error())?;

        let response_buffer = match response.await {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => return Err(error),
            // defensive: actor death races are non-deterministic in normal operation.
            Err(_) => return Err(actor_terminated_error()),
        };

        if response_buffer.len() < MBAP_HEADER_LEN {
            return Err(ModbusError::MalformedResponse(format!(
                "Modbus TCP frame shorter than MBAP header: {} < {MBAP_HEADER_LEN}",
                response_buffer.len()
            )));
        }
        // The MBAP codec only yields complete frames with a full header and at least one PDU byte.
        let protocol_id = u16::from_be_bytes([response_buffer[2], response_buffer[3]]);
        if protocol_id != 0 {
            return Err(ModbusError::ProtocolIdMismatch {
                actual: protocol_id,
            });
        }

        let received_unit_id = response_buffer[6];
        if received_unit_id != unit_id {
            return Err(ModbusError::UnitIdMismatch {
                expected: unit_id,
                actual: received_unit_id,
            });
        }

        let pdu_bytes = &response_buffer[MBAP_HEADER_LEN..];
        let response = ModbusResponse::try_from(pdu_bytes)?;
        if let ModbusResponse::Exception { function, code } = response {
            return Err(ModbusError::ExceptionResponse { function, code });
        }
        response.align_to_request(pdu)
    }

    /// Sends one request using this connection's default unit id.
    ///
    /// Modbus exception PDUs are surfaced as [`ModbusError::ExceptionResponse`]. Gateway-busy
    /// exception codes may be retried transparently when this handle has retry enabled.
    ///
    /// # Errors
    ///
    /// Returns transport, protocol, validation, exception-response, or
    /// request/response mismatch errors.
    #[must_use = "send_message returns a future whose output reports request success or failure"]
    pub async fn send_message(&self, pdu: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        self.send_message_with_unit_id(self.unit_id, pdu).await
    }

    /// Sends one request using an explicit unit id.
    ///
    /// Modbus exception PDUs are surfaced as [`ModbusError::ExceptionResponse`]. Gateway-busy
    /// exception codes may be retried transparently when this handle has retry enabled.
    ///
    /// # Errors
    ///
    /// Returns transport, protocol, validation, exception-response, or
    /// request/response mismatch errors.
    #[must_use = "send_message_with_unit_id returns a future whose output reports request success or failure"]
    pub async fn send_message_with_unit_id(
        &self,
        unit_id: u8,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        match self.retry {
            None => self.send_once(unit_id, pdu).await,
            Some(retry) => {
                let connection = self.clone();
                let pdu = pdu.clone();
                retry_transient(
                    || {
                        let connection = connection.clone();
                        let pdu = pdu.clone();
                        async move { connection.send_once(unit_id, &pdu).await }
                    },
                    retry,
                )
                .await
            }
        }
    }
}

pub(crate) fn actor_terminated_error() -> ModbusError {
    ModbusError::ReadError(Arc::new(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "reader task terminated",
    )))
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tcp::test_support::accept_and_hold_server;
    use tokio::net::TcpListener;

    fn spawn_actor_with_timeouts(
        host: impl Into<String>,
        port: u16,
        unit_id: u8,
        transaction_id: u16,
        timeouts: ModbusTcpTimeouts,
    ) -> (ModbusTcpConnection, tokio::task::JoinHandle<()>) {
        ModbusTcpSocket::new(host, port, unit_id)
            .with_initial_transaction_id(transaction_id)
            .with_timeouts(timeouts)
            .spawn_actor()
    }

    #[tokio::test]
    async fn socket_connect_returns_connected_handle() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .with_initial_transaction_id(0)
            .connect()
            .await
            .unwrap();
        assert!(connection.is_connected().await);
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_and_clears_connected() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .with_initial_transaction_id(0)
            .connect()
            .await
            .unwrap();
        connection.disconnect().await;
        connection.disconnect().await;
        assert!(!connection.is_connected().await);
    }

    #[tokio::test]
    async fn connect_accepts_domain_style_host() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new("localhost", addr.port(), 1)
            .with_initial_transaction_id(0)
            .connect()
            .await
            .unwrap();

        assert!(connection.is_connected().await);
    }

    #[tokio::test]
    async fn actor_exits_after_handles_drop_while_disconnected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (connection, actor_task) = spawn_actor_with_timeouts(
            addr.ip().to_string(),
            addr.port(),
            1,
            0,
            ModbusTcpTimeouts::default(),
        );
        let clone = connection.clone();

        drop(connection);
        drop(clone);

        timeout(Duration::from_secs(1), actor_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn actor_exits_after_handles_drop_while_connected() {
        let addr = accept_and_hold_server().await;
        let (connection, actor_task) = spawn_actor_with_timeouts(
            addr.ip().to_string(),
            addr.port(),
            1,
            0,
            ModbusTcpTimeouts::default(),
        );
        let clone = connection.clone();
        connection.connect().await.unwrap();
        assert!(connection.is_connected().await);

        drop(connection);
        drop(clone);

        timeout(Duration::from_secs(1), actor_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn with_unit_id_keeps_retry_policy_and_overrides_unit() {
        let (req_tx, _req_rx) = mpsc::channel(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        let connection = ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            1,
            connected,
            Duration::from_millis(50),
            None,
        );
        let child = connection.with_unit_id(7);
        assert_eq!(child.retry, None);
        assert_eq!(child.unit_id, 7);
    }

    #[tokio::test]
    async fn send_once_backpressures_on_full_request_channel() {
        let (req_tx, _req_rx) = mpsc::channel(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        let (reply, _response) = oneshot::channel();
        req_tx
            .try_send(RequestCommand {
                unit_id: 1,
                pdu: ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 1,
                },
                reply,
                queue_deadline: tokio::time::Instant::now() + Duration::from_secs(1),
            })
            .unwrap();
        let connection = ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            1,
            connected,
            Duration::from_millis(10),
            None,
        );

        let result = timeout(
            Duration::from_millis(10),
            connection.send_once(
                1,
                &ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 1,
                },
            ),
        )
        .await;

        assert!(result.is_err());
    }

    fn handle_with_dead_actor() -> ModbusTcpConnection {
        let (req_tx, req_rx) = mpsc::channel(1);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        drop(req_rx);
        drop(ctrl_rx);
        ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            1,
            connected,
            Duration::from_millis(50),
            None,
        )
    }

    /// Disconnecting a handle whose actor task has already stopped must return
    /// gracefully instead of panicking on the closed command channel.
    #[tokio::test]
    async fn disconnect_returns_when_actor_already_stopped() {
        let connection = handle_with_dead_actor();

        // The closed command channel must be handled gracefully: reaching this point
        // without panicking or hanging is the contract under test.
        connection.disconnect().await;
    }

    /// A send issued after the actor task has stopped must surface a terminated-actor
    /// transport error rather than blocking forever on the dropped channel.
    #[tokio::test]
    async fn send_once_reports_terminated_actor() {
        let connection = handle_with_dead_actor();

        let result = connection
            .send_once(
                1,
                &ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 1,
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(ModbusError::ReadError(error)) if error.kind() == io::ErrorKind::ConnectionAborted
        ));
    }

    /// If the actor drops the disconnect acknowledgement without replying, the
    /// public disconnect call must return after logging the terminated actor.
    #[tokio::test]
    async fn disconnect_returns_when_actor_drops_ack() {
        let (req_tx, _req_rx) = mpsc::channel(1);
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        let connection = ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            1,
            connected,
            Duration::from_millis(50),
            None,
        );
        tokio::spawn(async move {
            if let Some(ControlCommand::Disconnect { ack }) = ctrl_rx.recv().await {
                drop(ack);
            }
        });

        connection.disconnect().await;
    }

    /// If the actor drops a request waiter without replying, the caller must see
    /// a terminated-actor transport error.
    #[tokio::test]
    async fn send_once_reports_dropped_response_waiter() {
        let (req_tx, mut req_rx) = mpsc::channel(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        let connection = ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            1,
            connected,
            Duration::from_millis(50),
            None,
        );
        tokio::spawn(async move {
            if let Some(RequestCommand { reply, .. }) = req_rx.recv().await {
                drop(reply);
            }
        });

        let result = connection
            .send_once(
                1,
                &ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 1,
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(ModbusError::ReadError(error)) if error.kind() == io::ErrorKind::ConnectionAborted
        ));
    }

    #[tokio::test]
    async fn send_once_rejects_short_frame_before_header_indexing() {
        let (req_tx, mut req_rx) = mpsc::channel(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        let connection = ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            1,
            connected,
            Duration::from_millis(50),
            None,
        );
        tokio::spawn(async move {
            if let Some(RequestCommand { reply, .. }) = req_rx.recv().await {
                assert!(reply.send(Ok(vec![0; MBAP_HEADER_LEN - 1])).is_ok());
            }
        });

        let result = connection
            .send_once(
                1,
                &ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 1,
                },
            )
            .await;

        assert!(matches!(result, Err(ModbusError::MalformedResponse(_))));
    }

    #[tokio::test]
    async fn connect_failure_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let timeouts = ModbusTcpTimeouts {
            connect_timeout: Duration::from_millis(50),
            write_timeout: Duration::from_millis(50),
            response_timeout: Duration::from_millis(50),
        };
        assert!(matches!(
            ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
                .with_initial_transaction_id(0)
                .with_timeouts(timeouts)
                .connect()
                .await,
            Err(ModbusError::ConnectError(_) | ModbusError::ConnectTimeout)
        ));
    }

    #[tokio::test]
    async fn connect_is_idempotent_while_already_connected() {
        let addr = accept_and_hold_server().await;
        let (connection, _actor_task) = spawn_actor_with_timeouts(
            addr.ip().to_string(),
            addr.port(),
            1,
            0,
            ModbusTcpTimeouts::default(),
        );

        connection.connect().await.unwrap();
        connection.connect().await.unwrap();

        assert!(connection.is_connected().await);
    }

    /// A live handle must be able to open its own actor's socket, and doing so twice
    /// must not dial again or move the generation.
    #[tokio::test]
    async fn handle_connect_opens_socket_and_is_idempotent() {
        let addr = accept_and_hold_server().await;
        let (connection, _actor_task) = spawn_actor_with_timeouts(
            addr.ip().to_string(),
            addr.port(),
            1,
            0,
            ModbusTcpTimeouts::default(),
        );
        assert_eq!(
            connection.status(),
            ConnectionStatus {
                connected: false,
                generation: 0,
            }
        );

        connection.connect().await.unwrap();
        assert_eq!(
            connection.status(),
            ConnectionStatus {
                connected: true,
                generation: 1,
            }
        );

        connection.connect().await.unwrap();
        assert_eq!(
            connection.status(),
            ConnectionStatus {
                connected: true,
                generation: 1,
            }
        );
    }

    /// An unreachable peer must surface a connect error and leave the generation alone.
    ///
    /// The failed attempt publishes nothing, so `disconnected` on its own proves little:
    /// the check that matters is the *following* successful connect landing on
    /// generation 1. A counter bumped before the dial would show up here as 2.
    #[tokio::test]
    async fn handle_connect_reports_unreachable_peer_without_bumping_generation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);
        let (connection, _actor_task) = spawn_actor_with_timeouts(
            dead_addr.ip().to_string(),
            dead_addr.port(),
            1,
            0,
            ModbusTcpTimeouts {
                connect_timeout: Duration::from_millis(200),
                ..ModbusTcpTimeouts::default()
            },
        );

        let result = connection.connect().await;

        assert!(matches!(
            result,
            Err(ModbusError::ConnectError(_) | ModbusError::ConnectTimeout)
        ));
        assert_eq!(
            connection.status(),
            ConnectionStatus {
                connected: false,
                generation: 0,
            }
        );

        // Re-bind the same address so the retry succeeds against the same actor.
        let listener = TcpListener::bind(dead_addr).await.unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        connection.connect().await.unwrap();

        assert_eq!(
            connection.status(),
            ConnectionStatus {
                connected: true,
                generation: 1,
            }
        );
    }

    /// Connecting through a handle whose actor already stopped must report the
    /// terminated actor instead of hanging on the closed control channel.
    #[tokio::test]
    async fn handle_connect_reports_terminated_actor() {
        let connection = handle_with_dead_actor();

        let result = connection.connect().await;

        assert!(matches!(
            result,
            Err(ModbusError::ReadError(error)) if error.kind() == io::ErrorKind::ConnectionAborted
        ));
    }

    /// If the actor drops the connect acknowledgement without replying, the caller must
    /// see a terminated-actor error rather than waiting forever.
    #[tokio::test]
    async fn handle_connect_reports_dropped_ack() {
        let (req_tx, _req_rx) = mpsc::channel(1);
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(ConnectionStatus::disconnected());
        let connection = ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            1,
            connected,
            Duration::from_millis(50),
            None,
        );
        tokio::spawn(async move {
            if let Some(ControlCommand::Connect { ack }) = ctrl_rx.recv().await {
                drop(ack);
            }
        });

        let result = connection.connect().await;

        assert!(matches!(
            result,
            Err(ModbusError::ReadError(error)) if error.kind() == io::ErrorKind::ConnectionAborted
        ));
    }

    /// A subscriber must observe the drop and the following reconnect as two distinct
    /// changes, and the generation must identify the replacement socket. Reopening the
    /// socket must reuse the same actor, so the generation continues rather than restarts.
    ///
    /// Both `changed` awaits are bounded: a missing publication must fail the test
    /// rather than hang CI forever.
    #[tokio::test]
    async fn watch_status_observes_drop_and_reconnect_as_distinct_changes() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .connect()
            .await
            .unwrap();
        let mut status = connection.watch_status();

        connection.disconnect().await;
        timeout(Duration::from_millis(500), status.changed())
            .await
            .expect("teardown must publish a status change")
            .unwrap();
        assert_eq!(
            *status.borrow_and_update(),
            ConnectionStatus {
                connected: false,
                generation: 1,
            }
        );

        connection.connect().await.unwrap();
        timeout(Duration::from_millis(500), status.changed())
            .await
            .expect("reconnect must publish a status change")
            .unwrap();
        assert_eq!(
            *status.borrow_and_update(),
            ConnectionStatus {
                connected: true,
                generation: 2,
            }
        );
    }

    /// A subscriber that samples only before and after a full drop/reconnect cycle sees
    /// `connected: true` both times, but a changed generation — the property a
    /// `watch<bool>` collapses away.
    #[tokio::test]
    async fn watch_status_exposes_reconnect_to_a_late_sampler() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .connect()
            .await
            .unwrap();
        let status = connection.watch_status();
        let before = *status.borrow();

        connection.disconnect().await;
        connection.connect().await.unwrap();
        connection.disconnect().await;
        connection.connect().await.unwrap();

        let after = *status.borrow();
        assert_eq!(
            before,
            ConnectionStatus {
                connected: true,
                generation: 1,
            }
        );
        assert_eq!(
            after,
            ConnectionStatus {
                connected: true,
                generation: 3,
            }
        );
    }

    /// A redundant `disconnect` must not fabricate a transition for subscribers.
    #[tokio::test]
    async fn watch_status_ignores_redundant_disconnect() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .connect()
            .await
            .unwrap();
        let mut status = connection.watch_status();

        connection.disconnect().await;
        status.changed().await.unwrap();
        let after_first = *status.borrow_and_update();

        connection.disconnect().await;

        assert!(
            timeout(Duration::from_millis(50), status.changed())
                .await
                .is_err()
        );
        assert_eq!(*status.borrow(), after_first);
    }

    /// A fresh subscription must start with the current value marked as seen, so
    /// `changed` only resolves on an actual transition.
    #[tokio::test]
    async fn watch_status_starts_with_current_value_marked_seen() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .connect()
            .await
            .unwrap();
        let mut status = connection.watch_status();

        assert!(
            timeout(Duration::from_millis(50), status.changed())
                .await
                .is_err()
        );
    }

    /// Clones and unit-id derivations share one actor, hence one generation counter.
    #[tokio::test]
    async fn clones_share_one_generation_counter() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .connect()
            .await
            .unwrap();
        let sibling = connection.with_unit_id(7);

        connection.disconnect().await;
        sibling.connect().await.unwrap();

        assert_eq!(connection.status(), sibling.status());
        assert_eq!(connection.status().generation, 2);
        assert!(connection.is_connected().await);
    }
}
