// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Public Modbus TCP connection handle and request orchestration.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU16, AtomicU64, Ordering},
};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

use crate::tcp::connected_state::ConnectedState;
use crate::tcp::frame::MBAP_HEADER_LEN;
use crate::tcp::pending::{self, PendingGuard};
use crate::tcp::retry::{ModbusTcpRetry, retry_transient};
use crate::tcp::timeouts::ModbusTcpTimeouts;
use crate::tcp::writer::{self, TearDown};
use crate::{ModbusRequest, ModbusResponse, error::ModbusError, tcp::adu::build_modbus_tcp_adu};

type SharedConnectedState = Arc<Mutex<Option<Arc<ConnectedState>>>>;

/// Reusable Modbus TCP client handle.
///
/// The handle owns shared connection state behind reference-counted locks, so
/// clones reuse the same TCP socket and transaction-id counter. Requests may be
/// in flight concurrently; the background reader routes each response to the
/// caller waiting on the matching MBAP transaction identifier. A generation
/// counter prevents stale read/write failures from tearing down a newer socket
/// after a reconnect.
///
/// # Retries
///
/// Transient connect, write, and read failures are retried within a send call
/// according to [`Self::with_retry`] and [`ModbusTcpRetry`]. Passing `None` to
/// [`Self::with_retry`] disables same-call retry and returns the first transient
/// error, but reconnect remains independent: the failed socket is invalidated
/// and the next call reconnects lazily.
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    state: SharedConnectedState,
    address: IpAddr,
    port: u16,
    unit_id: u8,
    transaction_id: Arc<AtomicU16>,
    timeouts: ModbusTcpTimeouts,
    retry: Option<ModbusTcpRetry>,
    generation: Arc<AtomicU64>,
}

impl ModbusTcpConnection {
    /// Creates a connection handle with default connect, write, and read timeouts.
    ///
    /// # Arguments
    ///
    /// * `address` - IP address of the Modbus TCP server or gateway.
    /// * `port` - TCP port used by the server, commonly `502`.
    /// * `unit_id` - Default Modbus unit identifier used by [`Self::send_message`].
    /// * `transaction_id` - Initial MBAP transaction identifier. The value is
    ///   incremented for each request and wraps around to `0` after `u16::MAX`.
    ///
    /// # Returns
    ///
    /// Returns a lazily connected handle. No socket is opened until [`Self::connect`],
    /// [`Self::send_message`], or [`Self::send_message_with_unit_id`] is called.
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
    ///
    /// # Arguments
    ///
    /// * `address` - IP address of the Modbus TCP server or gateway.
    /// * `port` - TCP port used by the server, commonly `502`.
    /// * `unit_id` - Default Modbus unit identifier used by [`Self::send_message`].
    /// * `transaction_id` - Initial MBAP transaction identifier. The value is
    ///   incremented for each request and wraps around to `0` after `u16::MAX`.
    /// * `timeouts` - Per-phase limits for connection establishment, frame writes,
    ///   and response reads.
    ///
    /// # Returns
    ///
    /// Returns a lazily connected handle using the provided timeout configuration.
    #[must_use]
    pub fn with_timeouts(
        address: IpAddr,
        port: u16,
        unit_id: u8,
        transaction_id: u16,
        timeouts: ModbusTcpTimeouts,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            address,
            port,
            unit_id,
            transaction_id: Arc::new(AtomicU16::new(transaction_id)),
            timeouts,
            retry: Some(ModbusTcpRetry::default()),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Configures the retry policy used for future send operations on this handle.
    ///
    /// Retry configuration is stored on the handle, while the underlying socket is
    /// shared between clones. Calling this method does not mutate existing clones;
    /// clone or derive `with_unit_id` handles after setting the policy when they
    /// should use the same retry behavior.
    ///
    /// The policy is validated when a send operation starts. Use
    /// [`ModbusTcpRetry::validate`] if you need to reject invalid configuration
    /// before storing it on the connection.
    ///
    /// # Arguments
    ///
    /// * `retry` - Retry policy to use. Pass `None` to disable same-call retry.
    ///
    /// # Returns
    ///
    /// Returns this handle with the updated retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: Option<ModbusTcpRetry>) -> Self {
        self.retry = retry;
        self
    }

    /// Returns a new handle that shares the same transport but overrides the default unit id.
    ///
    /// # Arguments
    ///
    /// * `unit_id` - Default unit identifier to use when the returned handle sends
    ///   requests through [`Self::send_message`].
    ///
    /// # Returns
    ///
    /// Returns a clone-like handle with the same socket, transaction-id counter,
    /// timeout configuration, retry policy, and generation state as `self`.
    #[must_use]
    pub fn with_unit_id(&self, unit_id: u8) -> Self {
        Self {
            state: Arc::clone(&self.state),
            address: self.address,
            port: self.port,
            unit_id,
            transaction_id: Arc::clone(&self.transaction_id),
            timeouts: self.timeouts,
            retry: self.retry,
            generation: Arc::clone(&self.generation),
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

    async fn create_connected_state(
        &self,
        generation: u64,
    ) -> Result<Arc<ConnectedState>, ModbusError> {
        let stream =
            Self::connect_stream(self.address, self.port, self.timeouts.connect_timeout).await?;
        let (read_half, write_half) = stream.into_split();
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_task = tokio::spawn(async move {
            ConnectedState::reader_loop(read_half, reader_pending).await;
        });

        Ok(Arc::new(ConnectedState {
            writer: Arc::new(Mutex::new(write_half)),
            pending,
            reader_task,
            generation,
        }))
    }

    async fn ensure_connected_state(&self) -> Result<Arc<ConnectedState>, ModbusError> {
        let mut state_guard = self.state.lock().await;
        if let Some(state) = state_guard.as_ref() {
            if !state.reader_task.is_finished() {
                return Ok(Arc::clone(state));
            }

            *state_guard = None;
        }

        let generation = self
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let connected_state = self.create_connected_state(generation).await?;
        *state_guard = Some(Arc::clone(&connected_state));
        Ok(connected_state)
    }

    /// Drops the active connected state and advances the generation counter.
    ///
    /// # Arguments
    ///
    /// * `captured` - When `Some(g)`, the state is dropped only if `g` is still
    ///   the current generation (guarded teardown from a request/writer path).
    ///   When `None`, the state is dropped unconditionally (explicit disconnect).
    pub(crate) async fn invalidate(&self, captured: Option<u64>) {
        if let Some(captured_generation) = captured {
            if self.generation.load(Ordering::SeqCst) != captured_generation {
                return;
            }
        }

        let mut state_guard = self.state.lock().await;
        let current_generation = self.generation.load(Ordering::SeqCst);
        if captured.is_some_and(|generation| generation != current_generation) {
            return;
        }

        *state_guard = None;
        self.generation
            .store(current_generation.saturating_add(1), Ordering::SeqCst);
    }

    /// Opens the TCP connection eagerly.
    ///
    /// Calling this is optional because [`Self::send_message`] and
    /// [`Self::send_message_with_unit_id`] connect lazily when needed.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ConnectError`] or [`ModbusError::ConnectTimeout`] if opening the socket fails.
    pub async fn connect(&self) -> Result<(), ModbusError> {
        let _ = self.ensure_connected_state().await?;
        Ok(())
    }

    /// Closes the current TCP session if one is open.
    ///
    /// This invalidates the shared connected state for all cloned handles. Any
    /// in-flight request waiters are completed with a read error when the state is
    /// dropped, and the next request reconnects lazily.
    pub async fn disconnect(&self) {
        self.invalidate(None).await;
    }

    /// Returns whether this handle currently owns an open TCP stream.
    ///
    /// # Returns
    ///
    /// Returns `true` when shared state exists and its reader task is still active.
    /// Returns `false` before the first connection, after [`Self::disconnect`], or
    /// after the reader task has terminated because the peer closed the socket or a
    /// transport failure occurred.
    pub async fn is_connected(&self) -> bool {
        self.state
            .lock()
            .await
            .as_ref()
            .is_some_and(|state| !state.reader_task.is_finished())
    }

    async fn send_message_attempt(
        &self,
        unit_id: u8,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        let state = self.ensure_connected_state().await?;
        let captured_generation = state.generation;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tid = pending::allocate_transaction_id(&self.transaction_id, &state.pending, tx)?;
        let mut pending_guard = PendingGuard {
            pending: Arc::clone(&state.pending),
            tid,
            armed: true,
        };

        let adu = build_modbus_tcp_adu(tid, unit_id, pdu)?;
        debug!(tid, "Modbus TCP Frame: {:02X?}", adu);

        writer::write_adu_cancellation_safe(
            Arc::clone(&state.writer),
            adu,
            self.timeouts.write_timeout,
            TearDown {
                connection: self.clone(),
                generation: captured_generation,
            },
        )
        .await?;

        let response_buffer = match timeout(self.timeouts.read_timeout, rx).await {
            Ok(Ok(Ok(frame))) => frame,
            Ok(Ok(Err(error))) => {
                self.invalidate(Some(captured_generation)).await;
                return Err(error);
            }
            Ok(Err(_)) => {
                self.invalidate(Some(captured_generation)).await;
                return Err(ModbusError::ReadError(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "reader task terminated",
                )));
            }
            Err(_) => {
                self.invalidate(Some(captured_generation)).await;
                return Err(ModbusError::ReadTimeout);
            }
        };

        pending_guard.disarm();

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

        let received_transaction_id = u16::from_be_bytes([response_buffer[0], response_buffer[1]]);
        if received_transaction_id != tid {
            return Err(ModbusError::TransactionIdMismatch {
                expected: tid,
                actual: received_transaction_id,
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
    /// The connection is opened on demand. Transient connect, read, and write
    /// failures invalidate the current socket and are retried according to this
    /// handle's [`ModbusTcpRetry`] policy. Protocol, validation, Modbus
    /// exception, and request/response mismatch failures are returned without
    /// retrying.
    ///
    /// # Arguments
    ///
    /// * `pdu` - Typed Modbus request to serialize and send.
    ///
    /// # Returns
    ///
    /// Returns the parsed response after validating the MBAP transaction id,
    /// protocol id, unit id, and request/response shape.
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
    /// This is useful when one TCP gateway fronts multiple logical Modbus devices.
    /// The socket and transaction-id counter are shared with the default-unit-id
    /// path, so calls for different unit ids may be in flight concurrently.
    /// Retry and reconnect semantics match [`Self::send_message`].
    ///
    /// # Arguments
    ///
    /// * `unit_id` - Modbus unit identifier to place in the MBAP header for this request.
    /// * `pdu` - Typed Modbus request to serialize and send.
    ///
    /// # Returns
    ///
    /// Returns the parsed response after validating that it matches this request's
    /// transaction id, unit id, and function-specific response shape.
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
            None => self.send_message_attempt(unit_id, pdu).await,
            Some(retry) => {
                let connection = self.clone();
                let pdu = pdu.clone();

                retry_transient(
                    || {
                        let connection = connection.clone();
                        let pdu = pdu.clone();
                        async move { connection.send_message_attempt(unit_id, &pdu).await }
                    },
                    retry,
                )
                .await
            }
        }
    }
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
