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
