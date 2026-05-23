// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tiny-mb contributors

//! Issues concurrent Modbus TCP requests over one shared connection.
//!
//! Run with a reachable Modbus TCP server, for example:
//! `cargo run --example concurrent_clients`.

use std::error::Error;
use std::net::{IpAddr, Ipv4Addr};

use tiny_mb::ModbusRequest;
use tiny_mb::tcp::ModbusTcpConnection;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let connection = ModbusTcpConnection::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 502, 1, 0);
    let mut tasks = Vec::new();

    for offset in 0..4 {
        let client = connection.clone();
        tasks.push(tokio::spawn(async move {
            let request = ModbusRequest::ReadHoldingRegisters {
                starting_address: offset,
                quantity: 1,
            };

            let response = client.send_message(&request).await?;
            info!(offset, ?response, "received concurrent Modbus response");
            Ok::<_, tiny_mb::ModbusError>(())
        }));
    }

    for task in tasks {
        task.await??;
    }

    connection.disconnect().await;
    Ok(())
}
