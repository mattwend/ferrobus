// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use std::collections::{HashMap, hash_map::Entry};
use std::io;
use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU16, AtomicU64, Ordering},
};
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use futures::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, error};

use crate::{ModbusRequest, ModbusResponse, error::ModbusError, tcp::adu::build_modbus_tcp_adu};

const MBAP_HEADER_LEN: usize = 7;
const MAX_MODBUS_TCP_FRAME: usize = 260;
const RETRY_MAX_ELAPSED_TIME: Duration = Duration::from_secs(2);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TID_PROBES: usize = 256;

type Pending = StdMutex<HashMap<u16, oneshot::Sender<Result<Vec<u8>, ModbusError>>>>;

/// Per-operation time limits used by [`ModbusTcpConnection`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModbusTcpTimeouts {
    /// Maximum time allowed to establish a TCP connection.
    pub connect_timeout: Duration,
    /// Maximum time allowed to write one Modbus TCP frame.
    pub write_timeout: Duration,
    /// Maximum time allowed to read one Modbus TCP response.
    pub read_timeout: Duration,
}

impl Default for ModbusTcpTimeouts {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }
}

struct ConnectedState {
    // `lock_owned` requires an `Arc<Mutex<_>>`; the spawned writer task uses it to
    // keep a full ADU write cancellation-safe after the caller future is dropped.
    writer: Arc<Mutex<OwnedWriteHalf>>,
    pending: Arc<Pending>,
    reader_task: JoinHandle<()>,
    generation: u64,
}

#[derive(Clone)]
struct TearDown {
    connection: ModbusTcpConnection,
    generation: u64,
}

impl std::fmt::Debug for ConnectedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectedState")
            .field("generation", &self.generation)
            .field("reader_finished", &self.reader_task.is_finished())
            .finish_non_exhaustive()
    }
}

impl Drop for ConnectedState {
    fn drop(&mut self) {
        self.reader_task.abort();
        Self::drain_pending(
            &self.pending,
            io::ErrorKind::ConnectionAborted,
            "connection state replaced or dropped",
        );
    }
}

impl ConnectedState {
    /// Drains all in-flight request waiters with a transport error.
    fn drain_pending(pending: &Pending, kind: io::ErrorKind, reason: &'static str) {
        match pending.lock() {
            Ok(mut map) => {
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err(ModbusError::ReadError(io::Error::new(kind, reason))));
                }
            }
            Err(_) => {
                // A poisoned pending map cannot be drained safely; log the degradation so
                // callers blocked on removed senders are not silently abandoned.
                error!(
                    reason,
                    "pending map poisoned while draining in-flight requests"
                );
            }
        }
    }

    async fn reader_loop(mut read_half: OwnedReadHalf, pending: Arc<Pending>) {
        let reader_pending = Arc::clone(&pending);
        let reader = async move {
            loop {
                let mut header_buffer = [0u8; MBAP_HEADER_LEN];
                read_half
                    .read_exact(&mut header_buffer)
                    .await
                    .map_err(ModbusError::ReadError)?;

                let body_len = ModbusTcpConnection::response_body_len_from_header(header_buffer)
                    .map_err(|error| {
                        ModbusError::MalformedResponse(format!("invalid MBAP header: {error}"))
                    })?;

                let mut response_buffer = vec![0u8; MBAP_HEADER_LEN + body_len];
                response_buffer[..MBAP_HEADER_LEN].copy_from_slice(&header_buffer);
                read_half
                    .read_exact(&mut response_buffer[MBAP_HEADER_LEN..])
                    .await
                    .map_err(ModbusError::ReadError)?;

                let tid = u16::from_be_bytes([header_buffer[0], header_buffer[1]]);
                let sender = {
                    let mut map = reader_pending.lock().map_err(|_| {
                        ModbusError::ReadError(io::Error::other("pending map poisoned"))
                    })?;
                    match map.remove(&tid) {
                        Some(sender) => {
                            debug!(tid, in_flight = map.len(), "pending response matched");
                            Some(sender)
                        }
                        None => None,
                    }
                };

                match sender {
                    Some(tx) => {
                        let _ = tx.send(Ok(response_buffer));
                    }
                    None => {
                        debug!(tid, "stray response, ignored");
                    }
                }
            }
        };

        let result = AssertUnwindSafe(reader)
            .catch_unwind()
            .await
            .map_err(|panic| {
                let message = if let Some(message) = panic.downcast_ref::<&str>() {
                    *message
                } else if let Some(message) = panic.downcast_ref::<String>() {
                    message.as_str()
                } else {
                    "unknown panic payload"
                };
                error!(message, "reader task panicked");
                ModbusError::ReadError(io::Error::other("reader task panicked"))
            })
            .and_then(|inner| inner);

        match result {
            Ok(()) => {}
            Err(ModbusError::ReadError(error)) => {
                let kind = error.kind();
                Self::drain_pending(&pending, kind, "reader task terminated");
            }
            Err(ModbusError::MalformedResponse(_)) => {
                Self::drain_pending(
                    &pending,
                    io::ErrorKind::InvalidData,
                    "reader task received malformed response",
                );
            }
            Err(_) => {
                Self::drain_pending(&pending, io::ErrorKind::Other, "reader task terminated");
            }
        }
    }
}

struct PendingGuard {
    pending: Arc<Pending>,
    tid: u16,
    armed: bool,
}

impl PendingGuard {
    /// Disables automatic pending-map removal on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            match self.pending.lock() {
                Ok(mut map) => {
                    let _ = map.remove(&self.tid);
                    debug!(
                        tid = self.tid,
                        in_flight = map.len(),
                        "pending request removed"
                    );
                }
                Err(_) => {
                    error!(
                        tid = self.tid,
                        "pending map poisoned while removing request"
                    );
                }
            }
        }
    }
}

/// Reusable Modbus TCP client handle.
///
/// Cloned handles may issue requests concurrently over one shared TCP socket.
/// Responses are matched back to callers by MBAP transaction identifier.
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    state: Arc<Mutex<Option<Arc<ConnectedState>>>>,
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

    fn allocate_pending_transaction_id(
        &self,
        pending: &Pending,
        sender: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
    ) -> Result<u16, ModbusError> {
        let mut map = pending
            .lock()
            .map_err(|_| ModbusError::ReadError(io::Error::other("pending map poisoned")))?;

        for _ in 0..MAX_TID_PROBES {
            let tid = self.transaction_id.fetch_add(1, Ordering::Relaxed);
            if let Entry::Vacant(entry) = map.entry(tid) {
                entry.insert(sender);
                debug!(tid, in_flight = map.len(), "pending request inserted");
                return Ok(tid);
            }
        }

        Err(ModbusError::NoFreeTransactionId)
    }

    async fn tear_down(&self, captured_generation: u64) {
        let current_generation = self.generation.load(Ordering::SeqCst);
        if captured_generation != current_generation {
            return;
        }

        let mut state_guard = self.state.lock().await;
        if self.generation.load(Ordering::SeqCst) == captured_generation {
            *state_guard = None;
            self.generation
                .store(captured_generation.saturating_add(1), Ordering::SeqCst);
        }
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
        let mut state_guard = self.state.lock().await;
        *state_guard = None;
        let current_generation = self.generation.load(Ordering::SeqCst);
        self.generation
            .store(current_generation.saturating_add(1), Ordering::SeqCst);
    }

    /// Returns whether this handle currently owns an open TCP stream.
    pub async fn is_connected(&self) -> bool {
        self.state
            .lock()
            .await
            .as_ref()
            .is_some_and(|state| !state.reader_task.is_finished())
    }

    fn response_body_len_from_header(header: [u8; MBAP_HEADER_LEN]) -> Result<usize, ModbusError> {
        let pdu_length = u16::from_be_bytes([header[4], header[5]]) as usize;
        if pdu_length < 2 {
            return Err(ModbusError::MalformedResponse(
                "Invalid MBAP length: missing unit identifier or PDU".to_string(),
            ));
        }

        let body_len = pdu_length - 1;
        let total_length = MBAP_HEADER_LEN + body_len;
        if total_length > MAX_MODBUS_TCP_FRAME {
            return Err(ModbusError::MalformedResponse(format!(
                "Response exceeds maximum frame size: {total_length} > {MAX_MODBUS_TCP_FRAME}"
            )));
        }

        Ok(body_len)
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

    /// Writes one complete Modbus TCP ADU on a background task.
    ///
    /// The background task owns the writer mutex guard until the frame is fully
    /// written and flushed, or until `write_timeout` expires. If the caller future
    /// is cancelled while the write is in progress, the task continues to preserve
    /// stream framing for subsequent requests.
    ///
    /// # Arguments
    ///
    /// * `writer` - Shared TCP write half for the connected state.
    /// * `adu` - Complete Modbus TCP ADU bytes to send.
    /// * `write_timeout` - Maximum duration for the write and flush operation.
    /// * `teardown` - Connection state and generation used to tear down stale state on failure.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::WriteError`] for socket failures or writer task
    /// failures, and [`ModbusError::WriteTimeout`] when the operation exceeds
    /// `write_timeout`.
    async fn write_adu_cancellation_safe(
        writer: Arc<Mutex<OwnedWriteHalf>>,
        adu: Vec<u8>,
        write_timeout: Duration,
        teardown: TearDown,
    ) -> Result<(), ModbusError> {
        // Keep this spawned instead of inlining the write in the caller future:
        // dropping a `JoinHandle` does not abort its task, so caller cancellation
        // cannot leave a partial ADU on the socket before the next writer runs.
        let writer_teardown = teardown.clone();
        let writer_task = tokio::spawn(async move {
            let mut writer = writer.lock_owned().await;
            let result = timeout(write_timeout, async {
                writer.write_all(&adu).await?;
                writer.flush().await
            })
            .await
            .map_err(|_| ModbusError::WriteTimeout)?
            .map_err(ModbusError::WriteError);

            if result.is_err() {
                writer_teardown
                    .connection
                    .tear_down(writer_teardown.generation)
                    .await;
            }

            result
        });

        match writer_task.await {
            Ok(result) => result,
            Err(error) => {
                teardown.connection.tear_down(teardown.generation).await;
                Err(ModbusError::WriteError(io::Error::other(format!(
                    "writer task failed: {error}"
                ))))
            }
        }
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
                let (tx, rx) = oneshot::channel();
                let tid = connection.allocate_pending_transaction_id(&state.pending, tx)?;
                let mut pending_guard = PendingGuard {
                    pending: Arc::clone(&state.pending),
                    tid,
                    armed: true,
                };

                let adu = build_modbus_tcp_adu(tid, unit_id, &pdu)?;
                debug!(tid, "Modbus TCP Frame: {:02X?}", adu);

                Self::write_adu_cancellation_safe(
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
                        connection.tear_down(captured_generation).await;
                        return Err(error);
                    }
                    Ok(Err(_)) => {
                        connection.tear_down(captured_generation).await;
                        return Err(ModbusError::ReadError(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "reader task terminated",
                        )));
                    }
                    Err(_) => {
                        connection.tear_down(captured_generation).await;
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
    fn response_body_len_excludes_unit_id() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x11];
        let body_len = ModbusTcpConnection::response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 5);
    }

    #[test]
    fn response_body_len_rejects_zero_length() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x11];
        let error = ModbusTcpConnection::response_body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    #[test]
    fn response_body_len_minimum_valid() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x11];
        let body_len = ModbusTcpConnection::response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 1);
    }

    #[test]
    fn response_body_len_maximum_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0xFE, 0x11];
        let body_len = ModbusTcpConnection::response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 253);
    }

    #[test]
    fn response_body_len_rejects_oversized_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x11];
        let error = ModbusTcpConnection::response_body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("exceeds maximum frame size"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    #[test]
    fn response_body_len_single_byte_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x11];
        let body_len = ModbusTcpConnection::response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 2);
    }

    #[test]
    fn response_body_len_rejects_empty_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x11];
        let error = ModbusTcpConnection::response_body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
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
    fn default_timeouts_match_previous_behavior() {
        let timeouts = ModbusTcpTimeouts::default();

        assert_eq!(timeouts.connect_timeout, Duration::from_secs(5));
        assert_eq!(timeouts.write_timeout, Duration::from_secs(5));
        assert_eq!(timeouts.read_timeout, Duration::from_secs(5));
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

    #[test]
    fn allocate_pending_transaction_id_inserts_atomically() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0);
        let pending = StdMutex::new(HashMap::new());
        let (tx, _rx) = oneshot::channel();

        let tid = connection
            .allocate_pending_transaction_id(&pending, tx)
            .unwrap();

        let map = pending.lock().unwrap();
        assert_eq!(tid, 0);
        assert!(map.contains_key(&0));
    }

    #[test]
    fn allocate_pending_transaction_id_reports_no_free_tid_after_probe_limit() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, 0);
        let pending = StdMutex::new(HashMap::new());
        {
            let mut map = pending.lock().unwrap();
            for tid in 0..u16::try_from(MAX_TID_PROBES).unwrap() {
                let (tx, _rx) = oneshot::channel();
                map.insert(tid, tx);
            }
        }
        let (tx, _rx) = oneshot::channel();

        let error = connection
            .allocate_pending_transaction_id(&pending, tx)
            .unwrap_err();

        assert!(matches!(error, ModbusError::NoFreeTransactionId));
    }

    #[test]
    fn allocate_pending_transaction_id_wraps_around_u16_max() {
        let connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 502, 1, u16::MAX);
        let pending = StdMutex::new(HashMap::new());
        let (first_tx, _first_rx) = oneshot::channel();
        let (second_tx, _second_rx) = oneshot::channel();

        let first_tid = connection
            .allocate_pending_transaction_id(&pending, first_tx)
            .unwrap();
        let second_tid = connection
            .allocate_pending_transaction_id(&pending, second_tx)
            .unwrap();

        let map = pending.lock().unwrap();
        assert_eq!(first_tid, u16::MAX);
        assert_eq!(second_tid, 0);
        assert!(map.contains_key(&u16::MAX));
        assert!(map.contains_key(&0));
    }

    #[test]
    fn pending_guard_removes_cancelled_request_entry() {
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx);

        let guard = PendingGuard {
            pending: Arc::clone(&pending),
            tid: 7,
            armed: true,
        };
        drop(guard);

        assert!(pending.lock().unwrap().is_empty());
    }
}
