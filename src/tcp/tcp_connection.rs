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
#[cfg(test)]
use crate::tcp::timeouts::ModbusTcpTimeouts;
use crate::{ModbusRequest, ModbusResponse, error::ModbusError};

/// Live Modbus TCP client handle.
///
/// Construct a handle with [`ModbusTcpSocket::connect`] when custom timeouts,
/// flow control, retry, or an initial transaction-id seed are needed. Use
/// [`ModbusTcpConnection::connect`] for the default configuration shortcut. Both
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
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    req_tx: mpsc::Sender<RequestCommand>,
    pub(crate) ctrl_tx: mpsc::Sender<ControlCommand>,
    unit_id: u8,
    connected: watch::Receiver<bool>,
    queue_timeout: Duration,
    retry: Option<ModbusTcpRetry>,
}

impl ModbusTcpConnection {
    pub(crate) fn from_actor_parts(
        req_tx: mpsc::Sender<RequestCommand>,
        ctrl_tx: mpsc::Sender<ControlCommand>,
        unit_id: u8,
        connected: watch::Receiver<bool>,
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

    /// Connects to a Modbus TCP server with default construction settings.
    ///
    /// `host` may be a DNS name or numeric IP address. `port` is the TCP port,
    /// and `unit_id` is the default Modbus unit id used by the returned live
    /// handle. This is a shortcut for configuring a [`ModbusTcpSocket`] with
    /// defaults and awaiting [`ModbusTcpSocket::connect`].
    ///
    /// # Errors
    ///
    /// Returns validation, connection, timeout, or actor-termination errors from
    /// [`ModbusTcpSocket::connect`].
    pub async fn connect(
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

    /// Closes the current TCP session if one is open.
    ///
    /// The method waits until the actor has processed the disconnect command,
    /// making subsequent `is_connected` reads observe the cleared state unless
    /// another handle reconnects afterward.
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

    /// Returns whether this handle currently owns an open TCP stream.
    #[allow(clippy::unused_async)]
    pub async fn is_connected(&self) -> bool {
        *self.connected.borrow()
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
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn accept_and_hold_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        addr
    }

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

    async fn warm_up_actor(connection: &ModbusTcpConnection) -> Result<(), ModbusError> {
        let (ack, reply) = oneshot::channel();
        connection
            .ctrl_tx
            .send(ControlCommand::Connect { ack })
            .await
            .map_err(|_| actor_terminated_error())?;
        reply.await.map_err(|_| actor_terminated_error())?
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
        warm_up_actor(&connection).await.unwrap();
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
        let (_connected_tx, connected) = watch::channel(false);
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
        let (_connected_tx, connected) = watch::channel(false);
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
        let (_connected_tx, connected) = watch::channel(false);
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
        let (_connected_tx, connected) = watch::channel(false);
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
        let (_connected_tx, connected) = watch::channel(false);
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
        let (_connected_tx, connected) = watch::channel(false);
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

        warm_up_actor(&connection).await.unwrap();
        warm_up_actor(&connection).await.unwrap();

        assert!(connection.is_connected().await);
    }
}
