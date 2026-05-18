// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use backoff::{
    Error as BackoffError, ExponentialBackoff, ExponentialBackoffBuilder, future::retry,
};
use std::net::IpAddr;
use std::sync::{
    Arc,
    atomic::{AtomicU16, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

use crate::response::align_response_to_request;
use crate::{
    ModbusRequest, ModbusResponse, error::ModbusError, tcp::adu::build_modbus_tcp_adu_checked,
};

const MBAP_HEADER_LEN: usize = 7;
const MAX_MODBUS_TCP_FRAME: usize = 260;
const RETRY_MAX_ELAPSED_TIME: Duration = Duration::from_secs(2);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Reusable Modbus TCP client handle.
///
/// Cloned handles share the same underlying TCP stream and transaction counter.
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    stream: Arc<Mutex<Option<TcpStream>>>,
    address: IpAddr,
    port: u16,
    unit_id: u8,
    transaction_id: Arc<AtomicU16>,
    timeouts: ModbusTcpTimeouts,
}

impl ModbusTcpConnection {
    /// Creates a connection handle with default connect, write, and read timeouts.
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
    pub fn with_timeouts(
        address: IpAddr,
        port: u16,
        unit_id: u8,
        transaction_id: u16,
        timeouts: ModbusTcpTimeouts,
    ) -> Self {
        Self {
            stream: Arc::new(Mutex::new(None)),
            address,
            port,
            unit_id,
            transaction_id: Arc::new(AtomicU16::new(transaction_id)),
            timeouts,
        }
    }

    /// Returns the timeout configuration used for future operations.
    pub fn timeouts(&self) -> ModbusTcpTimeouts {
        self.timeouts
    }

    /// Returns the default unit identifier used by [`Self::send_message`].
    pub fn unit_id(&self) -> u8 {
        self.unit_id
    }

    /// Returns a new handle that shares the same transport but overrides the default unit id.
    pub fn with_unit_id(&self, unit_id: u8) -> Self {
        Self {
            stream: Arc::clone(&self.stream),
            address: self.address,
            port: self.port,
            unit_id,
            transaction_id: Arc::clone(&self.transaction_id),
            timeouts: self.timeouts,
        }
    }

    async fn connect_stream(
        address: IpAddr,
        port: u16,
        connect_timeout: Duration,
    ) -> Result<TcpStream, ModbusError> {
        let server_addr = format!("{}:{}", address, port);
        let stream = timeout(connect_timeout, TcpStream::connect(&server_addr))
            .await
            .map_err(|_| ModbusError::ConnectTimeout)?
            .map_err(ModbusError::ConnectError)?;
        debug!("Connected to Modbus TCP server at {}", &server_addr);
        Ok(stream)
    }

    /// Opens the TCP connection eagerly.
    ///
    /// Calling this is optional because [`Self::send_message`] and
    /// [`Self::send_message_with_unit_id`] connect lazily when needed.
    pub async fn connect(&self) -> Result<(), ModbusError> {
        let stream =
            Self::connect_stream(self.address, self.port, self.timeouts.connect_timeout).await?;
        let mut stream_guard = self.stream.lock().await;
        *stream_guard = Some(stream);
        Ok(())
    }

    /// Closes the current TCP session if one is open.
    pub async fn disconnect(&self) {
        let mut stream_guard = self.stream.lock().await;
        *stream_guard = None;
    }

    /// Returns whether this handle currently owns an open TCP stream.
    pub async fn is_connected(&self) -> bool {
        self.stream.lock().await.is_some()
    }

    fn response_body_len_from_header(header: &[u8; MBAP_HEADER_LEN]) -> Result<usize, ModbusError> {
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
                "Response exceeds maximum frame size: {} > {}",
                total_length, MAX_MODBUS_TCP_FRAME
            )));
        }

        Ok(body_len)
    }

    fn retry_backoff() -> ExponentialBackoff {
        ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(RETRY_MAX_ELAPSED_TIME))
            .build()
    }

    async fn ensure_connected_stream(
        stream: &mut Option<TcpStream>,
        address: IpAddr,
        port: u16,
        timeouts: ModbusTcpTimeouts,
    ) -> Result<&mut TcpStream, ModbusError> {
        if stream.is_none() {
            let tcp_stream = Self::connect_stream(address, port, timeouts.connect_timeout).await?;
            *stream = Some(tcp_stream);
        }

        stream.as_mut().ok_or_else(|| {
            ModbusError::ConnectError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "stream missing after successful connect",
            ))
        })
    }

    async fn exchange_frame(
        socket: &mut TcpStream,
        adu: &[u8],
        timeouts: ModbusTcpTimeouts,
    ) -> Result<Vec<u8>, ModbusError> {
        match timeout(timeouts.write_timeout, socket.write_all(adu)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(ModbusError::WriteError(error)),
            Err(_) => return Err(ModbusError::WriteTimeout),
        }

        socket.flush().await.map_err(ModbusError::WriteError)?;

        let mut header_buffer = [0u8; MBAP_HEADER_LEN];
        match timeout(timeouts.read_timeout, socket.read_exact(&mut header_buffer)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(ModbusError::ReadError(error)),
            Err(_) => return Err(ModbusError::ReadTimeout),
        }

        let body_len = Self::response_body_len_from_header(&header_buffer).map_err(|error| {
            ModbusError::MalformedResponse(format!("invalid MBAP header: {error}"))
        })?;
        let mut response_buffer = vec![0u8; MBAP_HEADER_LEN + body_len];
        response_buffer[..MBAP_HEADER_LEN].copy_from_slice(&header_buffer);

        match timeout(
            timeouts.read_timeout,
            socket.read_exact(&mut response_buffer[MBAP_HEADER_LEN..]),
        )
        .await
        {
            Ok(Ok(_)) => Ok(response_buffer),
            Ok(Err(error)) => Err(ModbusError::ReadError(error)),
            Err(_) => Err(ModbusError::ReadTimeout),
        }
    }

    /// Sends one request using this connection's default unit id.
    ///
    /// The connection is opened on demand, and transient I/O failures are retried
    /// with a short exponential backoff.
    pub async fn send_message(&self, pdu: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        self.send_message_with_unit_id(self.unit_id, pdu).await
    }

    /// Sends one request using an explicit unit id.
    ///
    /// This is useful when one TCP gateway fronts multiple logical Modbus devices.
    pub async fn send_message_with_unit_id(
        &self,
        unit_id: u8,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        let backoff = Self::retry_backoff();
        let stream = Arc::clone(&self.stream);
        let transaction_id = Arc::clone(&self.transaction_id);
        let address = self.address;
        let port = self.port;
        let timeouts = self.timeouts;
        let pdu = pdu.clone();

        let tid = transaction_id.fetch_add(1, Ordering::Relaxed);

        retry(backoff, || {
            let pdu = pdu.clone();
            let stream = Arc::clone(&stream);
            async move {
                let adu = build_modbus_tcp_adu_checked(tid, unit_id, &pdu)
                    .map_err(BackoffError::permanent)?;
                debug!("Modbus TCP Frame: {:02X?}", adu);

                let mut stream_guard = stream.lock().await;
                let socket =
                    Self::ensure_connected_stream(&mut stream_guard, address, port, timeouts)
                        .await
                        .map_err(BackoffError::transient)?;

                let response_buffer = match Self::exchange_frame(socket, &adu, timeouts).await {
                    Ok(response_buffer) => response_buffer,
                    Err(
                        error @ (ModbusError::ConnectError(_)
                        | ModbusError::ConnectTimeout
                        | ModbusError::WriteError(_)
                        | ModbusError::WriteTimeout
                        | ModbusError::ReadError(_)
                        | ModbusError::ReadTimeout),
                    ) => {
                        *stream_guard = None;
                        return Err(BackoffError::transient(error));
                    }
                    Err(error) => return Err(BackoffError::permanent(error)),
                };

                let protocol_id = u16::from_be_bytes([response_buffer[2], response_buffer[3]]);
                if protocol_id != 0 {
                    return Err(BackoffError::permanent(ModbusError::ProtocolIdMismatch {
                        actual: protocol_id,
                    }));
                }

                let received_unit_id = response_buffer[6];
                if received_unit_id != unit_id {
                    return Err(BackoffError::permanent(ModbusError::UnitIdMismatch {
                        expected: unit_id,
                        actual: received_unit_id,
                    }));
                }

                let received_transaction_id =
                    u16::from_be_bytes([response_buffer[0], response_buffer[1]]);
                if received_transaction_id != tid {
                    return Err(BackoffError::permanent(
                        ModbusError::TransactionIdMismatch {
                            expected: tid,
                            actual: received_transaction_id,
                        },
                    ));
                }

                let pdu_bytes = &response_buffer[MBAP_HEADER_LEN..];
                let response =
                    ModbusResponse::try_from(pdu_bytes).map_err(BackoffError::permanent)?;
                if let ModbusResponse::Exception { function, code } = response {
                    return Err(BackoffError::permanent(ModbusError::ExceptionResponse {
                        function,
                        code,
                    }));
                }
                align_response_to_request(&pdu, response).map_err(BackoffError::permanent)
            }
        })
        .await
    }
}

#[cfg(test)]
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
        let body_len = ModbusTcpConnection::response_body_len_from_header(&header).unwrap();
        assert_eq!(body_len, 5);
    }

    #[test]
    fn response_body_len_rejects_zero_length() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x11];
        let error = ModbusTcpConnection::response_body_len_from_header(&header).unwrap_err();
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
        let body_len = ModbusTcpConnection::response_body_len_from_header(&header).unwrap();
        assert_eq!(body_len, 1);
    }

    #[test]
    fn response_body_len_maximum_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0xFE, 0x11];
        let body_len = ModbusTcpConnection::response_body_len_from_header(&header).unwrap();
        assert_eq!(body_len, 253);
    }

    #[test]
    fn response_body_len_rejects_oversized_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x11];
        let error = ModbusTcpConnection::response_body_len_from_header(&header).unwrap_err();
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
        let body_len = ModbusTcpConnection::response_body_len_from_header(&header).unwrap();
        assert_eq!(body_len, 2);
    }

    #[test]
    fn response_body_len_rejects_empty_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x11];
        let error = ModbusTcpConnection::response_body_len_from_header(&header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    #[test]
    fn retry_backoff_has_bounded_elapsed_time() {
        let backoff = ModbusTcpConnection::retry_backoff();
        assert_eq!(backoff.max_elapsed_time, Some(RETRY_MAX_ELAPSED_TIME));
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
        assert!(Arc::ptr_eq(&connection.stream, &child.stream));
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
