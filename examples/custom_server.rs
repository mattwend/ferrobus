// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

#![allow(missing_docs)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

use ferrobus::server::ModbusServer;
use ferrobus::tcp::ModbusTcpServer;
use ferrobus::{ExceptionCode, ModbusRequest, ModbusResponse};

struct HashMapServer {
    holding: Mutex<HashMap<u16, u16>>,
}

impl HashMapServer {
    fn new(values: impl IntoIterator<Item = (u16, u16)>) -> Self {
        Self {
            holding: Mutex::new(values.into_iter().collect()),
        }
    }
}

impl ModbusServer for HashMapServer {
    fn handle(
        &self,
        _unit_id: u8,
        request: ModbusRequest,
    ) -> impl Future<Output = Result<ModbusResponse, ExceptionCode>> + Send {
        let result = match request {
            ModbusRequest::ReadHoldingRegisters {
                starting_address,
                quantity,
            } => match self.holding.lock() {
                Ok(guard) => (0..quantity)
                    .map(|offset| {
                        let address = starting_address
                            .checked_add(offset)
                            .ok_or(ExceptionCode::IllegalDataAddress)?;
                        guard
                            .get(&address)
                            .copied()
                            .ok_or(ExceptionCode::IllegalDataAddress)
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|registers| ModbusResponse::ReadHoldingRegisters { registers }),
                Err(_) => Err(ExceptionCode::ServerDeviceFailure),
            },
            ModbusRequest::WriteSingleRegister { address, value } => match self.holding.lock() {
                Ok(mut guard) => {
                    guard.insert(address, value);
                    Ok(ModbusResponse::WriteSingleRegister { address, value })
                }
                Err(_) => Err(ExceptionCode::ServerDeviceFailure),
            },
            _ => Err(ExceptionCode::IllegalFunction),
        };
        async move { result }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = HashMapServer::new([(0, 10), (1, 20), (2, 30)]);
    ModbusTcpServer::new(server)
        .bind("127.0.0.1:5503".parse()?)
        .await?
        .serve()
        .await?;
    Ok(())
}
