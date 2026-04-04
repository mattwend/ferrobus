// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use backoff::{
    Error as BackoffError, ExponentialBackoff, ExponentialBackoffBuilder, future::retry,
};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

use crate::response::align_response_to_request;
use crate::{ModbusRequest, ModbusResponse, error::ModbusError, tcp::build_modbus_tcp_adu};

const MBAP_HEADER_LEN: usize = 7;
const MAX_MODBUS_TCP_FRAME: usize = 260;
const RETRY_MAX_ELAPSED_TIME: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    stream: Arc<Mutex<Option<Arc<Mutex<TcpStream>>>>>,
    address: IpAddr,
    port: u16,
    unit_id: u8,
    transaction_id: Arc<Mutex<u16>>,
}

impl ModbusTcpConnection {
    pub fn new(address: IpAddr, port: u16, unit_id: u8, transaction_id: u16) -> Self {
        Self {
            stream: Arc::new(Mutex::new(None)),
            address,
            port,
            unit_id,
            transaction_id: Arc::new(Mutex::new(transaction_id)),
        }
    }

    pub async fn connect(&self) -> Result<(), ModbusError> {
        let server_addr = format!("{}:{}", self.address, self.port);
        let stream = timeout(IO_TIMEOUT, TcpStream::connect(&server_addr))
            .await
            .map_err(|_| {
                ModbusError::ConnectionError(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connect timed out",
                ))
            })?
            .map_err(ModbusError::ConnectionError)?;
        debug!("Connected to Modbus TCP server at {}", &server_addr);
        let mut stream_guard = self.stream.lock().await;
        *stream_guard = Some(Arc::new(Mutex::new(stream)));
        Ok(())
    }

    fn response_body_len_from_header(header: &[u8; MBAP_HEADER_LEN]) -> Result<usize, ModbusError> {
        let pdu_length = u16::from_be_bytes([header[4], header[5]]) as usize;
        if pdu_length == 0 {
            return Err(ModbusError::ResponseError(
                "Invalid MBAP length: missing unit identifier and PDU".to_string(),
            ));
        }

        let body_len = pdu_length - 1;
        let total_length = MBAP_HEADER_LEN + body_len;
        if total_length > MAX_MODBUS_TCP_FRAME {
            return Err(ModbusError::ResponseError(format!(
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

    pub async fn send_message(&self, pdu: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        let backoff = Self::retry_backoff();
        let stream = Arc::clone(&self.stream);
        let transaction_id = Arc::clone(&self.transaction_id);
        let address = self.address;
        let port = self.port;
        let unit_id = self.unit_id;
        let pdu = pdu.clone();

        let tid = {
            let mut tid_guard = transaction_id.lock().await;
            let tid = *tid_guard;
            *tid_guard = tid.wrapping_add(1);
            tid
        };

        retry(backoff, || {
            let pdu = pdu.clone();
            let stream = Arc::clone(&stream);
            async move {
                let stream_arc = {
                    let mut stream_guard = stream.lock().await;
                    if stream_guard.is_none() {
                        let server_addr = format!("{}:{}", address, port);
                        let tcp_stream = timeout(IO_TIMEOUT, TcpStream::connect(&server_addr))
                            .await
                            .map_err(|_| {
                                BackoffError::transient(ModbusError::ConnectionError(
                                    std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        "connect timed out",
                                    ),
                                ))
                            })?
                            .map_err(ModbusError::ConnectionError)
                            .map_err(BackoffError::transient)?;
                        debug!("Connected to Modbus TCP server at {}", &server_addr);
                        *stream_guard = Some(Arc::new(Mutex::new(tcp_stream)));
                    }
                    Arc::clone(stream_guard.as_ref().unwrap())
                };

                let adu = build_modbus_tcp_adu(tid, unit_id, &pdu);
                debug!("Modbus TCP Frame: {:02X?}", adu);

                {
                    let mut socket = stream_arc.lock().await;
                    match timeout(IO_TIMEOUT, socket.write_all(&adu)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            let mut stream_guard = stream.lock().await;
                            *stream_guard = None;
                            return Err(BackoffError::transient(ModbusError::ConnectionError(
                                error,
                            )));
                        }
                        Err(_) => {
                            let mut stream_guard = stream.lock().await;
                            *stream_guard = None;
                            return Err(BackoffError::transient(ModbusError::ConnectionError(
                                std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "write timed out",
                                ),
                            )));
                        }
                    }
                    if let Err(e) = socket.flush().await.map_err(ModbusError::ConnectionError) {
                        let mut stream_guard = stream.lock().await;
                        *stream_guard = None;
                        return Err(BackoffError::transient(e));
                    }
                }

                let mut header_buffer = [0u8; MBAP_HEADER_LEN];
                {
                    let mut socket = stream_arc.lock().await;
                    match timeout(IO_TIMEOUT, socket.read_exact(&mut header_buffer)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            let mut stream_guard = stream.lock().await;
                            *stream_guard = None;
                            return Err(BackoffError::transient(ModbusError::ConnectionError(
                                error,
                            )));
                        }
                        Err(_) => {
                            let mut stream_guard = stream.lock().await;
                            *stream_guard = None;
                            return Err(BackoffError::transient(ModbusError::ConnectionError(
                                std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "read header timed out",
                                ),
                            )));
                        }
                    }
                }

                let protocol_id = u16::from_be_bytes([header_buffer[2], header_buffer[3]]);
                if protocol_id != 0 {
                    return Err(BackoffError::permanent(ModbusError::ProtocolIdMismatch {
                        actual: protocol_id,
                    }));
                }

                let received_unit_id = header_buffer[6];
                if received_unit_id != unit_id {
                    return Err(BackoffError::permanent(ModbusError::UnitIdMismatch {
                        expected: unit_id,
                        actual: received_unit_id,
                    }));
                }

                let body_len = Self::response_body_len_from_header(&header_buffer)
                    .map_err(BackoffError::permanent)?;

                let mut response_buffer = vec![0u8; MBAP_HEADER_LEN + body_len];
                response_buffer[..MBAP_HEADER_LEN].copy_from_slice(&header_buffer);
                {
                    let mut socket = stream_arc.lock().await;
                    match timeout(
                        IO_TIMEOUT,
                        socket.read_exact(&mut response_buffer[MBAP_HEADER_LEN..]),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            let mut stream_guard = stream.lock().await;
                            *stream_guard = None;
                            return Err(BackoffError::transient(ModbusError::ConnectionError(
                                error,
                            )));
                        }
                        Err(_) => {
                            let mut stream_guard = stream.lock().await;
                            *stream_guard = None;
                            return Err(BackoffError::transient(ModbusError::ConnectionError(
                                std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "read body timed out",
                                ),
                            )));
                        }
                    }
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
                align_response_to_request(&pdu, response).map_err(BackoffError::permanent)
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            ModbusError::ResponseError(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected ResponseError, got {other:?}"),
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
            ModbusError::ResponseError(message) => {
                assert!(message.contains("exceeds maximum frame size"));
            }
            other => panic!("Expected ResponseError, got {other:?}"),
        }
    }

    #[test]
    fn response_body_len_single_byte_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x11];
        let body_len = ModbusTcpConnection::response_body_len_from_header(&header).unwrap();
        assert_eq!(body_len, 2);
    }

    #[test]
    fn response_body_len_zero_length_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x11];
        let body_len = ModbusTcpConnection::response_body_len_from_header(&header).unwrap();
        assert_eq!(body_len, 0);
    }

    #[test]
    fn retry_backoff_has_bounded_elapsed_time() {
        let backoff = ModbusTcpConnection::retry_backoff();
        assert_eq!(backoff.max_elapsed_time, Some(RETRY_MAX_ELAPSED_TIME));
    }
}
