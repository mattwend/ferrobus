// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

#![allow(missing_docs)]

use ferrobus::server::InMemoryStore;
use ferrobus::tcp::ModbusTcpServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = InMemoryStore::new(100, 100, 100, 100)?;
    for address in 0..100u16 {
        store.set_holding(address, address)?;
    }

    // Try it from another shell with:
    // cargo run --example modbus_cli --features cli -- read holding --address 0 --quantity 4
    ModbusTcpServer::new(store)
        .bind("127.0.0.1:5502".parse()?)
        .await?
        .serve()
        .await?;

    Ok(())
}
