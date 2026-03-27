// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use backoff::{Error as BackoffError, ExponentialBackoff, future::retry};
use std::net::IpAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::debug;

use crate::{ModbusRequest, ModbusResponse, error::ModbusError, tcp::build_modbus_tcp_adu};

const MBAP_HEADER_LEN: usize = 7;
const MAX_MODBUS_TCP_FRAME: usize = 260;

#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    stream: Option<Arc<Mutex<TcpStream>>>,
    address: IpAddr,
    port: u16,
    unit_id: u8,
    transaction_id: u16,
}

impl ModbusTcpConnection {
    pub fn new(address: IpAddr, port: u16, unit_id: u8, transaction_id: u16) -> Self {
        Self {
            stream: None,
            address,
            port,
            unit_id,
            transaction_id,
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

        if self.stream.is_none() {
            self.connect().await?;
        }
        let stream_mutex = self.stream.as_ref().ok_or_else(|| {
            ModbusError::ConnectionError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "No TCP stream available",
            ))
        })?;
        let stream_mutex_clone = Arc::clone(stream_mutex);

        let transaction_id = self.transaction_id;
        let unit_id = self.unit_id;

        let op = || async {
            let mut stream = stream_mutex_clone.lock().await;

            let adu = build_modbus_tcp_adu(transaction_id, unit_id, pdu)
                .map_err(BackoffError::permanent)?;

            debug!("Modbus TCP Frame: {:02X?}", adu);

            stream
                .write_all(&adu)
                .await
                .map_err(|e| BackoffError::permanent(ModbusError::ConnectionError(e)))?;

            let mut header_buffer = [0u8; MBAP_HEADER_LEN];
            stream
                .read_exact(&mut header_buffer)
                .await
                .map_err(|e| BackoffError::permanent(ModbusError::ConnectionError(e)))?;

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
                .map_err(|e| BackoffError::permanent(ModbusError::ConnectionError(e)))?;

            let received_transaction_id =
                u16::from_be_bytes([response_buffer[0], response_buffer[1]]);
            if received_transaction_id != transaction_id {
                return Err(BackoffError::permanent(
                    ModbusError::TransactionIdMismatch {
                        expected: transaction_id,
                        actual: received_transaction_id,
                    },
                ));
            }

            let pdu_bytes = &response_buffer[MBAP_HEADER_LEN..];
            let resp = ModbusResponse::try_from(pdu_bytes).map_err(BackoffError::permanent)?;

            Ok(resp)
        };

        retry(backoff, op).await
    }
}
