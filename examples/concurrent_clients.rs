// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Issues concurrent Modbus TCP requests over one shared connection.
//!
//! Run with a reachable Modbus TCP server, for example:
//! `cargo run --example concurrent_clients`.

use std::error::Error;

use ferrobus::ModbusRequest;
use ferrobus::tcp::ModbusTcpConnection;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let connection = ModbusTcpConnection::open("127.0.0.1", 502, 1).await?;
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
            Ok::<_, ferrobus::ModbusError>(())
        }));
    }

    for task in tasks {
        task.await??;
    }

    connection.disconnect().await;
    Ok(())
}
