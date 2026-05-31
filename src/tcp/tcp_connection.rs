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

use backon::{ExponentialBuilder, Retryable};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

use crate::tcp::connected_state::ConnectedState;
use crate::tcp::frame::MBAP_HEADER_LEN;
use crate::tcp::pending::{self, PendingGuard};
use crate::tcp::timeouts::ModbusTcpTimeouts;
use crate::tcp::writer::{self, TearDown};
use crate::{ModbusRequest, ModbusResponse, error::ModbusError, tcp::adu::build_modbus_tcp_adu};

const RETRY_MAX_ELAPSED_TIME: Duration = Duration::from_secs(2);
type SharedConnectedState = Arc<Mutex<Option<Arc<ConnectedState>>>>;

/// Reusable Modbus TCP client handle.
///
/// Cloned handles may issue requests concurrently over one shared TCP socket.
/// Responses are matched back to callers by MBAP transaction identifier.
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    state: SharedConnectedState,
    address: IpAddr,
    port: u16,
    unit_id: u8,
    transaction_id: Arc<AtomicU16>,
    timeouts: ModbusTcpTimeouts,
    generation: Arc<AtomicU64>,
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
        Self {
            state: Arc::new(Mutex::new(None)),
            address,
            port,
            unit_id,
            transaction_id: Arc::new(AtomicU16::new(transaction_id)),
            timeouts,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns the timeout configuration used for future operations.
    #[must_use]
    pub fn timeouts(&self) -> ModbusTcpTimeouts {
        self.timeouts
    }

    /// Returns the default unit identifier used by [`Self::send_message`].
    #[must_use]
    pub fn unit_id(&self) -> u8 {
        self.unit_id
    }

    /// Returns a new handle that shares the same transport but overrides the default unit id.
    #[must_use]
    pub fn with_unit_id(&self, unit_id: u8) -> Self {
        Self {
            state: Arc::clone(&self.state),
            address: self.address,
            port: self.port,
            unit_id,
            transaction_id: Arc::clone(&self.transaction_id),
            timeouts: self.timeouts,
            generation: Arc::clone(&self.generation),
        }
    }

    async fn connect_stream(
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
    pub async fn disconnect(&self) {
        self.invalidate(None).await;
    }

    /// Returns whether this handle currently owns an open TCP stream.
    pub async fn is_connected(&self) -> bool {
        self.state
            .lock()
            .await
            .as_ref()
            .is_some_and(|state| !state.reader_task.is_finished())
    }

    /// Returns the exponential backoff policy used to retry transient I/O failures.
    ///
    /// The total time spent retrying is capped by [`RETRY_MAX_ELAPSED_TIME`].
    fn retry_backoff() -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(500))
            .with_factor(1.5)
            .with_jitter()
            .with_total_delay(Some(RETRY_MAX_ELAPSED_TIME))
    }

    /// Sends one request using this connection's default unit id.
    ///
    /// The connection is opened on demand, and transient I/O failures are retried
    /// with a short exponential backoff.
    ///
    /// # Errors
    ///
    /// Returns transport, protocol, validation, or request/response mismatch errors.
    pub async fn send_message(&self, pdu: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        self.send_message_with_unit_id(self.unit_id, pdu).await
    }

    /// Sends one request using an explicit unit id.
    ///
    /// This is useful when one TCP gateway fronts multiple logical Modbus devices.
    ///
    /// # Errors
    ///
    /// Returns transport, protocol, validation, or request/response mismatch errors.
    pub async fn send_message_with_unit_id(
        &self,
        unit_id: u8,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        let backoff = Self::retry_backoff();
        let connection = self.clone();
        let pdu = pdu.clone();

        (|| {
            let connection = connection.clone();
            let pdu = pdu.clone();
            async move {
                let state = connection.ensure_connected_state().await?;
                let captured_generation = state.generation;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let tid = pending::allocate_transaction_id(
                    &connection.transaction_id,
                    &state.pending,
                    tx,
                )?;
                let mut pending_guard = PendingGuard {
                    pending: Arc::clone(&state.pending),
                    tid,
                    armed: true,
                };

                let adu = build_modbus_tcp_adu(tid, unit_id, &pdu)?;
                debug!(tid, "Modbus TCP Frame: {:02X?}", adu);

                writer::write_adu_cancellation_safe(
                    Arc::clone(&state.writer),
                    adu,
                    connection.timeouts.write_timeout,
                    TearDown {
                        connection: connection.clone(),
                        generation: captured_generation,
                    },
                )
                .await?;

                let response_buffer = match timeout(connection.timeouts.read_timeout, rx).await {
                    Ok(Ok(Ok(frame))) => frame,
                    Ok(Ok(Err(error))) => {
                        connection.invalidate(Some(captured_generation)).await;
                        return Err(error);
                    }
                    Ok(Err(_)) => {
                        connection.invalidate(Some(captured_generation)).await;
                        return Err(ModbusError::ReadError(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "reader task terminated",
                        )));
                    }
                    Err(_) => {
                        connection.invalidate(Some(captured_generation)).await;
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

                let received_transaction_id =
                    u16::from_be_bytes([response_buffer[0], response_buffer[1]]);
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
                response.align_to_request(&pdu)
            }
        })
        .retry(backoff)
        .when(ModbusError::is_transient)
        .await
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn spawn_accept_once_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        addr
    }

    #[test]
    fn retry_backoff_builds_without_panicking() {
        let _ = ModbusTcpConnection::retry_backoff();
    }

    #[test]
    fn retry_max_elapsed_time_is_two_seconds() {
        assert_eq!(RETRY_MAX_ELAPSED_TIME, Duration::from_secs(2));
    }

    #[test]
    fn new_connection_uses_default_timeouts() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0);

        assert_eq!(connection.timeouts(), ModbusTcpTimeouts::default());
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

        assert_eq!(connection.timeouts(), timeouts);
    }

    #[test]
    fn with_unit_id_returns_child_handle_with_shared_state() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 7);

        let child = connection.with_unit_id(42);

        assert_eq!(connection.unit_id(), 1);
        assert_eq!(child.unit_id(), 42);
        assert_eq!(child.timeouts(), connection.timeouts());
        assert!(Arc::ptr_eq(&connection.state, &child.state));
        assert!(Arc::ptr_eq(
            &connection.transaction_id,
            &child.transaction_id,
        ));
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
}
