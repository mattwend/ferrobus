// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Public Modbus TCP connection handle and request orchestration.

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
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
/// teardown, and applies bounded channel backpressure under burst load.
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
        let (tx, rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (connected_tx, connected) = watch::channel(false);
        let actor = Actor::new(address, port, timeouts, transaction_id, rx, connected_tx);
        tokio::spawn(actor.run());
        Self {
            tx,
            unit_id,
            connected,
            read_timeout: timeouts.read_timeout,
            retry: Some(ModbusTcpRetry::default()),
        }
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
            .map_err(ModbusError::ConnectError)?;
        debug!(server_addr, "connected to Modbus TCP server");
        Ok(stream)
    }

    /// Opens the TCP connection eagerly.
    pub async fn connect(&self) -> Result<(), ModbusError> {
        let (ack, reply) = oneshot::channel();
        self.tx
            .send(Command::Connect { ack })
            .await
            .map_err(|_| actor_terminated_error())?;
        reply.await.map_err(|_| actor_terminated_error())?
    }

    /// Closes the current TCP session if one is open.
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
    pub async fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    async fn send_once(
        &self,
        unit_id: u8,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Request {
                unit_id,
                pdu: pdu.clone(),
                reply,
            })
            .await
            .map_err(|_| actor_terminated_error())?;

        let response_buffer = match timeout(self.read_timeout, response).await {
            Ok(Ok(Ok(frame))) => frame,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_)) => return Err(actor_terminated_error()),
            Err(_) => return Err(ModbusError::ReadTimeout),
        };

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
    pub async fn send_message(&self, pdu: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        self.send_message_with_unit_id(self.unit_id, pdu).await
    }

    /// Sends one request using an explicit unit id.
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
    ModbusError::ReadError(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "reader task terminated",
    ))
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    async fn spawn_accept_once_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        addr
    }

    #[test]
    fn new_connection_uses_default_timeouts() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0);

        assert_eq!(connection.timeouts, ModbusTcpTimeouts::default());
    }

    #[test]
    fn connection_with_retry_none_disables_retry() {
        let connection =
            ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0).with_retry(None);

        assert_eq!(connection.retry, None);
    }

    #[test]
    fn with_timeouts_preserves_default_retry() {
        let connection = ModbusTcpConnection::with_timeouts(
            "127.0.0.1".parse().unwrap(),
            502,
            1,
            0,
            ModbusTcpTimeouts::default(),
        );

        assert_eq!(connection.retry, Some(ModbusTcpRetry::default()));
    }

    #[test]
    fn with_timeouts_stores_custom_timeouts() {
        let timeouts = ModbusTcpTimeouts {
            connect_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(3),
        };

        let connection =
            ModbusTcpConnection::with_timeouts("127.0.0.1".parse().unwrap(), 502, 1, 0, timeouts);

        assert_eq!(connection.timeouts, timeouts);
    }

    #[test]
    fn with_unit_id_returns_child_handle_with_shared_state() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 7);

        let child = connection.with_unit_id(42);

        assert_eq!(connection.unit_id, 1);
        assert_eq!(child.unit_id, 42);
        assert_eq!(child.timeouts, connection.timeouts);
        assert!(Arc::ptr_eq(&connection.state, &child.state));
        assert!(Arc::ptr_eq(
            &connection.transaction_id,
            &child.transaction_id,
        ));
    }

    #[test]
    fn with_unit_id_propagates_retry_config() {
        let retry = ModbusTcpRetry {
            initial_delay: Duration::from_millis(10),
            ..ModbusTcpRetry::default()
        };
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 7)
            .with_retry(Some(retry));

        let child = connection.with_unit_id(42);

        assert_eq!(child.retry, Some(retry));
    }

    #[tokio::test]
    async fn is_connected_is_false_before_connect() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0);

        assert!(!connection.is_connected().await);
    }

    #[tokio::test]
    async fn is_connected_is_true_after_connect() {
        let addr = spawn_accept_once_server().await;
        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);

        connection.connect().await.unwrap();

        assert!(connection.is_connected().await);
    }

    #[tokio::test]
    async fn disconnect_clears_connected_state() {
        let addr = spawn_accept_once_server().await;
        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);

        connection.connect().await.unwrap();
        assert!(connection.is_connected().await);

        connection.disconnect().await;

        assert!(!connection.is_connected().await);
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_when_already_disconnected() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0);

        connection.disconnect().await;
        connection.disconnect().await;

        assert!(!connection.is_connected().await);
    }

    #[tokio::test]
    async fn invalidate_with_stale_generation_is_noop() {
        let addr = spawn_accept_once_server().await;
        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);

        connection.connect().await.unwrap();
        let current_generation = connection.generation.load(Ordering::SeqCst);

        // A captured generation older than the current one must not tear down
        // the live connection.
        connection
            .invalidate(Some(current_generation.saturating_sub(1)))
            .await;

        assert!(connection.is_connected().await);
    }

    #[tokio::test]
    async fn ensure_connected_state_replaces_finished_reader() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (release_second_tx, release_second_rx) = oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            // First connection: close immediately so the reader task finishes.
            let (first, _) = listener.accept().await.unwrap();
            drop(first);
            // Second connection: keep open until the assertion has observed it.
            let (_second, _) = listener.accept().await.unwrap();
            let _ = release_second_rx.await;
        });

        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
        connection.connect().await.unwrap();
        let first_generation = connection.generation.load(Ordering::SeqCst);

        // Wait for the reader task to observe EOF and finish.
        timeout(Duration::from_secs(1), async {
            while connection.is_connected().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        // Reconnect: the stale finished state is dropped and a new one created.
        connection.connect().await.unwrap();

        assert!(connection.is_connected().await);
        assert!(connection.generation.load(Ordering::SeqCst) > first_generation);
        drop(release_second_tx);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_message_propagates_reader_channel_error() {
        use crate::ModbusRequest;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (release_second_tx, release_second_rx) = oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            // First connection: read the request, then drop so the reader task
            // drains the in-flight waiter with a transport error.
            let (mut first, _) = listener.accept().await.unwrap();
            let mut header = [0u8; MBAP_HEADER_LEN];
            first.read_exact(&mut header).await.unwrap();
            let mut body = [0u8; 5];
            first.read_exact(&mut body).await.unwrap();
            drop(first);

            // Second connection (retry): respond successfully and stay open
            // until the client has consumed the response.
            let (mut second, _) = listener.accept().await.unwrap();
            let mut header = [0u8; MBAP_HEADER_LEN];
            second.read_exact(&mut header).await.unwrap();
            let mut body = [0u8; 5];
            second.read_exact(&mut body).await.unwrap();
            let unit_id = header[6];
            let response = [header[0], header[1], 0, 0, 0, 4, unit_id, 1, 1, 0b0000_0001];
            second.write_all(&response).await.unwrap();
            let _ = release_second_rx.await;
        });

        // Long read timeout ensures the reader's channel error wins over a
        // read timeout, exercising the Ok(Ok(Err(_))) request branch.
        let connection = ModbusTcpConnection::with_timeouts(
            addr.ip(),
            addr.port(),
            1,
            0,
            ModbusTcpTimeouts {
                connect_timeout: Duration::from_secs(1),
                write_timeout: Duration::from_secs(1),
                read_timeout: Duration::from_secs(5),
            },
        );

        let request = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 1,
        };

        let response = connection.send_message(&request).await.unwrap();
        assert_eq!(response, ModbusResponse::ReadCoils { coils: vec![true] });
        drop(release_second_tx);
        server_task.await.unwrap();
    }
}
