// tinymb - A simple Modbus library for Rust
// Copyright (C) 2025 tinymb authors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::error::Error;
use tracing::info;
use tracing_subscriber::{filter::EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use mb_rs::tcp::ModbusTcpConnection;
use mb_rs::ModbusRequest;

// TODO
// * error types
// * retry logic
// * cli tool
// * tcp_connection module tests

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::registry()
        .with(EnvFilter::from_env("LOG_LEVEL"))
        .with(fmt::layer().with_target(false))
        .init();

    let mut connection = ModbusTcpConnection::new("127.0.0.1".parse().unwrap(), 8502, 1, 1);

    let response = connection
        .send_message(&ModbusRequest::WriteSingleRegister {
            address: 40004,
            value: 500,
        })
        .await?;
    info!("Received response: {:?}", response);

    Ok(())
}
