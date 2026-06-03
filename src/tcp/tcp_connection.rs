// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Public Modbus TCP connection handle and request orchestration.

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};
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
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Request {
                unit_id,
                pdu: pdu.clone(),
                reply,
                deadline: Instant::now() + self.read_timeout,
            })
            .await
            .map_err(|_| actor_terminated_error())?;

        let response_buffer = match response.await {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(actor_terminated_error()),
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
    ModbusError::ReadError(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "reader task terminated",
    ))
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn echo_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut header = [0_u8; 7];
                    while stream.read_exact(&mut header).await.is_ok() {
                        let len = u16::from_be_bytes([header[4], header[5]]) as usize;
                        let mut rest = vec![0_u8; len.saturating_sub(1)];
                        if stream.read_exact(&mut rest).await.is_err() {
                            break;
                        }
                        let mut response = header.to_vec();
                        response.extend(rest);
                        if stream.write_all(&response).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn connection_starts_disconnected_and_connect_sets_connected() {
        let addr = echo_server().await;
        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
        assert!(!connection.is_connected().await);
        connection.connect().await.unwrap();
        assert!(connection.is_connected().await);
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_and_clears_connected() {
        let addr = echo_server().await;
        let connection = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
        connection.connect().await.unwrap();
        connection.disconnect().await;
        connection.disconnect().await;
        assert!(!connection.is_connected().await);
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
