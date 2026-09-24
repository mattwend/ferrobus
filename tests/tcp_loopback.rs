// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

#![allow(missing_docs, clippy::panic, clippy::unwrap_used)]

mod support;

use ferrobus::server::InMemoryStore;
use ferrobus::tcp::{ModbusTcpServer, ModbusTcpSocket};
use ferrobus::{ModbusRequest, ModbusResponse};
use support::spawn_tcp_server;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn client_exercises_all_supported_functions_against_server() {
    let store = InMemoryStore::new(32, 32, 32, 32).unwrap();
    let retained = store.clone();
    retained.set_coil(0, true).unwrap();
    retained.set_discrete(1, true).unwrap();
    retained.set_holding(0, 0x1111).unwrap();
    retained.set_input(0, 0x2222).unwrap();

    let (addr, shutdown) = spawn_tcp_server(ModbusTcpServer::new(store)).await;
    let client = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
        .connect()
        .await
        .unwrap();

    assert_eq!(
        client
            .send_message(&ModbusRequest::ReadCoils {
                starting_address: 0,
                quantity: 2,
            })
            .await
            .unwrap(),
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
    assert_eq!(
        client
            .send_message(&ModbusRequest::ReadDiscreteInputs {
                starting_address: 0,
                quantity: 2,
            })
            .await
            .unwrap(),
        ModbusResponse::ReadDiscreteInputs {
            inputs: vec![false, true]
        }
    );
    assert_eq!(
        client
            .send_message(&ModbusRequest::ReadHoldingRegisters {
                starting_address: 0,
                quantity: 1,
            })
            .await
            .unwrap(),
        ModbusResponse::ReadHoldingRegisters {
            registers: vec![0x1111]
        }
    );
    assert_eq!(
        client
            .send_message(&ModbusRequest::ReadInputRegisters {
                starting_address: 0,
                quantity: 1,
            })
            .await
            .unwrap(),
        ModbusResponse::ReadInputRegisters {
            registers: vec![0x2222]
        }
    );
    client
        .send_message(&ModbusRequest::WriteSingleCoil {
            address: 2,
            value: true,
        })
        .await
        .unwrap();
    client
        .send_message(&ModbusRequest::WriteSingleRegister {
            address: 2,
            value: 0x3333,
        })
        .await
        .unwrap();
    client
        .send_message(&ModbusRequest::WriteMultipleCoils {
            starting_address: 3,
            values: vec![true, false, true],
        })
        .await
        .unwrap();
    client
        .send_message(&ModbusRequest::WriteMultipleRegisters {
            starting_address: 3,
            values: vec![0x4444, 0x5555],
        })
        .await
        .unwrap();

    assert!(retained.get_coil(2).unwrap());
    assert_eq!(retained.get_holding(2).unwrap(), 0x3333);
    // Every written coil and register must land, not just the first one: a
    // packing or offset bug would leave the later values untouched.
    assert_eq!(
        [
            retained.get_coil(3).unwrap(),
            retained.get_coil(4).unwrap(),
            retained.get_coil(5).unwrap(),
            retained.get_coil(6).unwrap(),
        ],
        [true, false, true, false]
    );
    assert_eq!(
        [
            retained.get_holding(3).unwrap(),
            retained.get_holding(4).unwrap(),
            retained.get_holding(5).unwrap(),
        ],
        [0x4444, 0x5555, 0x0000]
    );
    shutdown.send(()).unwrap();
}
