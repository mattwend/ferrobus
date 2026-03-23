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

/// A Modbus TCP connection that sends and receives Modbus TCP frames.
#[derive(Clone, Debug)]
pub struct ModbusTcpConnection {
    /// The TCP stream used to communicate with the server.
    stream: Option<Arc<Mutex<TcpStream>>>,
    /// The address of the Modbus TCP server.
    address: IpAddr,
    /// The port of the Modbus TCP server.
    port: u16,
    /// The unit identifier of the remote slave device.
    unit_id: u8,
    /// The transaction identifier for matching requests/replies.
    transaction_id: u16,
}

impl ModbusTcpConnection {
    /// Creates a new Modbus TCP connection with the specified server address, port, unit ID, and
    /// transaction ID.
    ///
    /// # Arguments
    ///
    /// * `address` - The address of the Modbus TCP server.
    /// * `port` - The port of the Modbus TCP server.
    /// * `unit_id` - The unit identifier of the remote slave device.
    /// * `transaction_id` - The transaction identifier for matching requests/replies.
    ///
    /// # Returns
    ///
    /// A new `ModbusTcpConnection` instance.
    pub fn new(address: IpAddr, port: u16, unit_id: u8, transaction_id: u16) -> Self {
        Self {
            stream: None,
            address,
            port,
            unit_id,
            transaction_id,
        }
    }

    /// Connects to the Modbus TCP server.
    pub async fn connect(&mut self) -> Result<(), ModbusError> {
        let server_addr = format!("{}:{}", self.address, self.port);
        let stream = TcpStream::connect(&server_addr).await?;
        debug!("Connected to Modbus TCP server at {}", &server_addr);
        self.stream = Some(Arc::new(Mutex::new(stream)));
        Ok(())
    }

    /// Sends a Modbus TCP frame to the server and returns the response.
    /// The connection is established if it does not exist.
    ///
    /// # Arguments
    ///
    /// * `pdu` - The ModbusRequest PDU to send.
    ///
    /// # Returns
    ///
    /// A ModbusResponse containing the response PDU.
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
            let mut response_buffer = [0u8; 256];
            let mut stream = stream_mutex_clone.lock().await;

            let adu = build_modbus_tcp_adu(transaction_id, unit_id, pdu)
                .map_err(|e| BackoffError::permanent(ModbusError::AduBuildError(e.to_string())))?;

            debug!("Modbus TCP Frame: {:02X?}", adu);

            stream.write_all(&adu).await.map_err(|e| {
                // self.stream = None;
                BackoffError::transient(ModbusError::ConnectionError(e))
            })?;

            let size = stream.read(&mut response_buffer).await.map_err(|e| {
                // self.stream = None;
                BackoffError::transient(ModbusError::ConnectionError(e))
            })?;

            if size < 7 {
                return Err(BackoffError::permanent(ModbusError::ResponseTooShort {
                    expected: 7,
                    actual: size,
                }));
            }

            let pdu_bytes = &response_buffer[7..size];
            let resp = ModbusResponse::try_from(pdu_bytes).map_err(|e| {
                BackoffError::permanent(ModbusError::DeserializationError(e.to_string()))
            })?;

            Ok(resp)
        };

        retry(backoff, op).await
    }
}
