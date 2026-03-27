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

    pub async fn send_message(
        &mut self,
        pdu: &ModbusRequest,
    ) -> Result<ModbusResponse, ModbusError> {
        let backoff = ExponentialBackoff::default();

        let stream = Arc::new(Mutex::new(self.stream.take()));
        let unit_id = self.unit_id;
        let transaction_id = Arc::clone(&self.transaction_id);
        let address = self.address;
        let port = self.port;

        let result = retry(backoff, || {
            let pdu = pdu.clone();
            let stream = Arc::clone(&stream);
            let transaction_id = Arc::clone(&transaction_id);
            async move {
                {
                    let mut stream_guard = stream.lock().await;
                    if stream_guard.is_none() {
                        let server_addr = format!("{}:{}", address, port);
                        let tcp_stream = TcpStream::connect(&server_addr).await.map_err(|e| {
                            BackoffError::transient(ModbusError::ConnectionError(e))
                        })?;
                        *stream_guard = Some(Arc::new(Mutex::new(tcp_stream)));
                    }
                }

                let stream_mutex = {
                    let stream_guard = stream.lock().await;
                    Arc::clone(stream_guard.as_ref().unwrap())
                };

                let tid = {
                    let mut tid_guard = transaction_id.lock().await;
                    let tid = *tid_guard;
                    *tid_guard = tid.wrapping_add(1);
                    tid
                };

                let adu = build_modbus_tcp_adu(tid, unit_id, &pdu);
                debug!("Modbus TCP Frame: {:02X?}", adu);

                let mut stream = stream_mutex.lock().await;

                stream.write_all(&adu).await.map_err(|e| {
                    warn!("Write error: {}", e);
                    BackoffError::transient(ModbusError::ConnectionError(e))
                })?;

                let mut header_buffer = [0u8; MBAP_HEADER_LEN];
                stream.read_exact(&mut header_buffer).await.map_err(|e| {
                    warn!("Read header error: {}", e);
                    BackoffError::transient(ModbusError::ConnectionError(e))
                })?;

                let pdu_length = u16::from_be_bytes([header_buffer[4], header_buffer[5]]);
                let total_length = MBAP_HEADER_LEN + pdu_length as usize;

                if total_length > MAX_MODBUS_TCP_FRAME {
                    return Err(BackoffError::permanent(ModbusError::ResponseError(
                        format!(
                            "Response exceeds maximum frame size: {} > {}",
                            total_length, MAX_MODBUS_TCP_FRAME
                        ),
                    )));
                }

                let mut response_buffer = vec![0u8; total_length];
                response_buffer[..MBAP_HEADER_LEN].copy_from_slice(&header_buffer);
                stream
                    .read_exact(&mut response_buffer[MBAP_HEADER_LEN..])
                    .await
                    .map_err(|e| {
                        warn!("Read body error: {}", e);
                        BackoffError::transient(ModbusError::ConnectionError(e))
                    })?;

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
                let resp = ModbusResponse::try_from(pdu_bytes).map_err(BackoffError::permanent)?;

                let aligned =
                    align_response_to_request(&pdu, resp).map_err(BackoffError::permanent)?;

                Ok(aligned)
            }
        })
        .await;

        if result.is_ok() {
            let stream_guard = stream.lock().await;
            if let Some(ref s) = *stream_guard {
                self.stream = Some(Arc::clone(s));
            }
        }

        result
    }
}
