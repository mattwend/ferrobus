// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use backoff::{Error as BackoffError, ExponentialBackoff, future::retry};
use std::net::IpAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::response::align_response_to_request;
use crate::{ModbusRequest, ModbusResponse, error::ModbusError, tcp::build_modbus_tcp_adu};

const MBAP_HEADER_LEN: usize = 7;
const MAX_MODBUS_TCP_FRAME: usize = 260;

#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    stream: Option<Arc<Mutex<TcpStream>>>,
    address: IpAddr,
    port: u16,
    unit_id: u8,
    transaction_id: Arc<Mutex<u16>>,
}

impl ModbusTcpConnection {
    pub fn new(address: IpAddr, port: u16, unit_id: u8, transaction_id: u16) -> Self {
        Self {
            stream: None,
            address,
            port,
            unit_id,
            transaction_id: Arc::new(Mutex::new(transaction_id)),
        }
    }

    pub async fn connect(&mut self) -> Result<(), ModbusError> {
        let server_addr = format!("{}:{}", self.address, self.port);
        let stream = TcpStream::connect(&server_addr).await?;
        debug!("Connected to Modbus TCP server at {}", &server_addr);
        self.stream = Some(Arc::new(Mutex::new(stream)));
        Ok(())
    }

    async fn ensure_connected(
        stream: &Arc<Mutex<Option<Arc<Mutex<TcpStream>>>>>,
        address: IpAddr,
        port: u16,
    ) -> Result<(), ModbusError> {
        let needs_connect = {
            let stream_guard = stream.lock().await;
            stream_guard.is_none()
        };

        if needs_connect {
            let server_addr = format!("{}:{}", address, port);
            let tcp_stream = TcpStream::connect(&server_addr).await?;
            debug!("Connected to Modbus TCP server at {}", &server_addr);
            let mut stream_guard = stream.lock().await;
            *stream_guard = Some(Arc::new(Mutex::new(tcp_stream)));
        }

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

    pub async fn send_message(
        &mut self,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        let backoff = ExponentialBackoff::default();
        let stream = Arc::new(Mutex::new(self.stream.clone()));
        let transaction_id = Arc::clone(&self.transaction_id);
        let address = self.address;
        let port = self.port;
        let unit_id = self.unit_id;

        let result = retry(backoff, || {
            let pdu = pdu.clone();
            let stream = Arc::clone(&stream);
            let transaction_id = Arc::clone(&transaction_id);
            async move {
                Self::ensure_connected(&stream, address, port)
                    .await
                    .map_err(BackoffError::transient)?;

                let tid = {
                    let mut tid_guard = transaction_id.lock().await;
                    let tid = *tid_guard;
                    *tid_guard = tid.wrapping_add(1);
                    tid
                };

                let adu = build_modbus_tcp_adu(tid, unit_id, &pdu);
                debug!("Modbus TCP Frame: {:02X?}", adu);

                let stream_mutex = {
                    let stream_guard = stream.lock().await;
                    Arc::clone(stream_guard.as_ref().expect("connection ensured above"))
                };
                let mut socket = stream_mutex.lock().await;

                if let Err(error) = socket.write_all(&adu).await {
                    warn!("Write error: {}", error);
                    drop(socket);
                    let mut stream_guard = stream.lock().await;
                    *stream_guard = None;
                    return Err(BackoffError::transient(ModbusError::ConnectionError(error)));
                }

                let mut header_buffer = [0u8; MBAP_HEADER_LEN];
                if let Err(error) = socket.read_exact(&mut header_buffer).await {
                    warn!("Read header error: {}", error);
                    drop(socket);
                    let mut stream_guard = stream.lock().await;
                    *stream_guard = None;
                    return Err(BackoffError::transient(ModbusError::ConnectionError(error)));
                }

                let body_len = Self::response_body_len_from_header(&header_buffer)
                    .map_err(BackoffError::permanent)?;

                let mut response_buffer = vec![0u8; MBAP_HEADER_LEN + body_len];
                response_buffer[..MBAP_HEADER_LEN].copy_from_slice(&header_buffer);
                if let Err(error) = socket
                    .read_exact(&mut response_buffer[MBAP_HEADER_LEN..])
                    .await
                {
                    warn!("Read body error: {}", error);
                    drop(socket);
                    let mut stream_guard = stream.lock().await;
                    *stream_guard = None;
                    return Err(BackoffError::transient(ModbusError::ConnectionError(error)));
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
        .await;

        self.stream = stream.lock().await.clone();
        result
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
}
