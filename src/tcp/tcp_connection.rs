// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Public Modbus TCP connection handle and request orchestration.

use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use tracing::debug;

use crate::tcp::actor::{Actor, COMMAND_CHANNEL_CAPACITY, Command};
use crate::tcp::frame::MBAP_HEADER_LEN;
use crate::tcp::retry::{ModbusTcpRetry, retry_transient};
use crate::tcp::timeouts::ModbusTcpTimeouts;
use crate::{ModbusRequest, ModbusResponse, error::ModbusError};

/// Reusable Modbus TCP client handle.
///
/// Clones are lightweight command senders to a single background actor that owns
/// the TCP socket, pending response map, and transaction-id counter. The actor
/// connects lazily on the first warm-up or request, reconnects after transport
/// teardown, and applies bounded channel backpressure under burst load. The
/// caller-facing read timeout deadline starts before the request enters the
/// actor, so it bounds queue wait, write, and response wait time together while
/// the actor remains the single owner of timeout enforcement and socket teardown.
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    tx: mpsc::Sender<Command>,
    unit_id: u8,
    connected: watch::Receiver<bool>,
    read_timeout: Duration,
    retry: Option<ModbusTcpRetry>,
}

impl ModbusTcpConnection {
    /// Creates a connection handle with default connect, write, and read timeouts.
    #[must_use]
    pub fn new(address: IpAddr, port: u16, unit_id: u8, transaction_id: u16) -> Self {
        Self::with_timeouts(
            address,
            port,
            unit_id,
            transaction_id,
            ModbusTcpTimeouts::default(),
        )
    }

    /// Creates a connection handle with explicit timeout settings.
    #[must_use]
    pub fn with_timeouts(
        address: IpAddr,
        port: u16,
        unit_id: u8,
        transaction_id: u16,
        timeouts: ModbusTcpTimeouts,
    ) -> Self {
        Self::spawn_with_timeouts(address, port, unit_id, transaction_id, timeouts).0
    }

    fn spawn_with_timeouts(
        address: IpAddr,
        port: u16,
        unit_id: u8,
        transaction_id: u16,
        timeouts: ModbusTcpTimeouts,
    ) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (connected_tx, connected) = watch::channel(false);
        let actor = Actor::new(address, port, timeouts, transaction_id, rx, connected_tx);
        let actor_task = tokio::spawn(actor.run());
        (
            Self {
                tx,
                unit_id,
                connected,
                read_timeout: timeouts.read_timeout,
                retry: Some(ModbusTcpRetry::default()),
            },
            actor_task,
        )
    }

    /// Configures the retry policy used for future send operations on this handle.
    #[must_use]
    pub fn with_retry(mut self, retry: Option<ModbusTcpRetry>) -> Self {
        self.retry = retry;
        self
    }

    /// Returns a new handle that shares the same transport but overrides the default unit id.
    #[must_use]
    pub fn with_unit_id(&self, unit_id: u8) -> Self {
        Self {
            tx: self.tx.clone(),
            unit_id,
            connected: self.connected.clone(),
            read_timeout: self.read_timeout,
            retry: self.retry,
        }
    }

    pub(crate) async fn connect_stream(
        address: IpAddr,
        port: u16,
        connect_timeout: Duration,
    ) -> Result<TcpStream, ModbusError> {
        let server_addr = format!("{address}:{port}");
        let stream = timeout(connect_timeout, TcpStream::connect(&server_addr))
            .await
            .map_err(|_| ModbusError::ConnectTimeout)?
            .map_err(|error| ModbusError::ConnectError(Arc::new(error)))?;
        debug!(server_addr, "connected to Modbus TCP server");
        Ok(stream)
    }

    /// Opens the TCP connection eagerly.
    ///
    /// # Errors
    ///
    /// Returns connect or transport errors reported by the actor.
    pub async fn connect(&self) -> Result<(), ModbusError> {
        let (ack, reply) = oneshot::channel();
        self.tx
            .send(Command::Connect { ack })
            .await
            .map_err(|_| actor_terminated_error())?;
        reply.await.map_err(|_| actor_terminated_error())?
    }

    /// Closes the current TCP session if one is open.
    ///
    /// The method waits until the actor has processed the disconnect command,
    /// making subsequent `is_connected` reads observe the cleared state unless
    /// another handle reconnects afterward.
    pub async fn disconnect(&self) {
        let (ack, processed) = oneshot::channel();
        if let Err(error) = self.tx.send(Command::Disconnect { ack }).await {
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
        let deadline = Instant::now() + self.read_timeout;
        let (reply, response) = oneshot::channel();
        let command = Command::Request {
            unit_id,
            pdu: pdu.clone(),
            reply,
            deadline,
        };
        match timeout_at(deadline, self.tx.send(command)).await {
            Ok(Ok(())) => {}
            // defensive: actor death races are non-deterministic in normal operation.
            Ok(Err(_)) => return Err(actor_terminated_error()),
            Err(_) => return Err(ModbusError::ReadTimeout),
        }

        let response_buffer = match response.await {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => return Err(error),
            // defensive: actor death races are non-deterministic in normal operation.
            Err(_) => return Err(actor_terminated_error()),
        };

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
    /// # Errors
    ///
    /// Returns transport, protocol, validation, exception-response, or
    /// request/response mismatch errors.
    pub async fn send_message(&self, pdu: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        self.send_message_with_unit_id(self.unit_id, pdu).await
    }

    /// Sends one request using an explicit unit id.
    ///
    /// # Errors
    ///
    /// Returns transport, protocol, validation, exception-response, or
    /// request/response mismatch errors.
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

fn actor_terminated_error() -> ModbusError {
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

    #[tokio::test]
    async fn connection_starts_disconnected_and_connect_sets_connected() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
        assert!(!connection.is_connected().await);
        connection.connect().await.unwrap();
        assert!(connection.is_connected().await);
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_and_clears_connected() {
        let addr = accept_and_hold_server().await;
        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
        connection.connect().await.unwrap();
        connection.disconnect().await;
        connection.disconnect().await;
        assert!(!connection.is_connected().await);
    }

    #[tokio::test]
    async fn actor_exits_after_handles_drop_while_disconnected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (connection, actor_task) = ModbusTcpConnection::spawn_with_timeouts(
            addr.ip(),
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
        let (connection, actor_task) = ModbusTcpConnection::spawn_with_timeouts(
            addr.ip(),
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
        let connection =
            ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0).with_retry(None);
        let child = connection.with_unit_id(7);
        assert_eq!(child.retry, None);
        assert_eq!(child.unit_id, 7);
    }

    #[tokio::test]
    async fn send_once_bounds_wait_for_full_command_channel() {
        let (tx, _rx) = mpsc::channel(1);
        let (connected_tx, connected) = watch::channel(false);
        let (ack, _processed) = oneshot::channel();
        tx.try_send(Command::Disconnect { ack }).unwrap();
        drop(connected_tx);
        let connection = ModbusTcpConnection {
            tx,
            unit_id: 1,
            connected,
            read_timeout: Duration::from_millis(10),
            retry: None,
        };

        let result = connection
            .send_once(
                1,
                &ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 1,
                },
            )
            .await;

        assert!(matches!(result, Err(ModbusError::ReadTimeout)));
    }

    fn handle_with_dead_actor() -> ModbusTcpConnection {
        let (tx, rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(false);
        drop(rx);
        ModbusTcpConnection {
            tx,
            unit_id: 1,
            connected,
            read_timeout: Duration::from_millis(50),
            retry: None,
        }
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
        let (tx, mut rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(false);
        let connection = ModbusTcpConnection {
            tx,
            unit_id: 1,
            connected,
            read_timeout: Duration::from_millis(50),
            retry: None,
        };
        tokio::spawn(async move {
            if let Some(Command::Disconnect { ack }) = rx.recv().await {
                drop(ack);
            }
        });

        connection.disconnect().await;
    }

    /// If the actor drops a request waiter without replying, the caller must see
    /// a terminated-actor transport error.
    #[tokio::test]
    async fn send_once_reports_dropped_response_waiter() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_connected_tx, connected) = watch::channel(false);
        let connection = ModbusTcpConnection {
            tx,
            unit_id: 1,
            connected,
            read_timeout: Duration::from_millis(50),
            retry: None,
        };
        tokio::spawn(async move {
            if let Some(Command::Request { reply, .. }) = rx.recv().await {
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
    async fn connect_failure_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let timeouts = ModbusTcpTimeouts {
            connect_timeout: Duration::from_millis(50),
            write_timeout: Duration::from_millis(50),
            read_timeout: Duration::from_millis(50),
        };
        let connection = ModbusTcpConnection::with_timeouts(addr.ip(), addr.port(), 1, 0, timeouts)
            .with_retry(None);
        assert!(matches!(
            connection.connect().await,
            Err(ModbusError::ConnectError(_) | ModbusError::ConnectTimeout)
        ));
    }
}
