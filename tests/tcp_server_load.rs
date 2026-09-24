// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Load and lifecycle tests for the Modbus TCP server: many concurrent clients,
//! reconnects, and the `max_connections` cap.

#![allow(missing_docs, clippy::panic, clippy::unwrap_used)]

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ferrobus::server::{InMemoryStore, ModbusServer};
use ferrobus::tcp::{ModbusTcpServer, ModbusTcpSocket};
use ferrobus::{ExceptionCode, ModbusError, ModbusRequest, ModbusResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

mod support;
use support::spawn_tcp_server;

const CLIENTS: u16 = 8;
const ITERATIONS: u16 = 200;

/// Eight clients hammering one server on disjoint register regions: every reply
/// must carry the value that client itself wrote, so a crossed transaction or a
/// shared-state race shows up as a mismatch rather than as flakiness.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_clients_never_observe_another_clients_data() {
    let store = InMemoryStore::new(64, 64, 64, 64).unwrap();
    let (addr, shutdown) = spawn_tcp_server(ModbusTcpServer::new(store)).await;

    let mut clients = Vec::new();
    for client_index in 0..CLIENTS {
        clients.push(tokio::spawn(async move {
            let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
                .connect()
                .await
                .unwrap();
            for iteration in 0..ITERATIONS {
                let value = client_index
                    .wrapping_mul(1000)
                    .wrapping_add(iteration)
                    .wrapping_add(1);
                assert_eq!(
                    connection
                        .send_message(&ModbusRequest::WriteSingleRegister {
                            address: client_index,
                            value,
                        })
                        .await
                        .unwrap(),
                    ModbusResponse::WriteSingleRegister {
                        address: client_index,
                        value,
                    }
                );
                assert_eq!(
                    connection
                        .send_message(&ModbusRequest::ReadHoldingRegisters {
                            starting_address: client_index,
                            quantity: 1,
                        })
                        .await
                        .unwrap(),
                    ModbusResponse::ReadHoldingRegisters {
                        registers: vec![value]
                    },
                    "client {client_index} iteration {iteration}"
                );
            }
            connection.disconnect().await;
        }));
    }

    for client in clients {
        client.await.unwrap();
    }
    shutdown.send(()).unwrap();
}

/// The client reconnects after an explicit disconnect, and the server keeps
/// accepting after a client goes away without closing cleanly.
#[tokio::test]
async fn server_survives_client_reconnects() {
    let store = InMemoryStore::new(16, 16, 16, 16).unwrap();
    store.set_holding(0, 7).unwrap();
    let (addr, shutdown) = spawn_tcp_server(ModbusTcpServer::new(store)).await;
    let read = ModbusRequest::ReadHoldingRegisters {
        starting_address: 0,
        quantity: 1,
    };

    let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
        .connect()
        .await
        .unwrap();
    for _round in 0..3 {
        assert_eq!(
            connection.send_message(&read).await.unwrap(),
            ModbusResponse::ReadHoldingRegisters { registers: vec![7] }
        );
        connection.disconnect().await;
    }

    // A client that vanishes mid-header must not disturb later clients.
    let mut abandoned = TcpStream::connect(addr).await.unwrap();
    abandoned.write_all(&[0x00, 0x01, 0x00]).await.unwrap();
    drop(abandoned);

    assert_eq!(
        connection.send_message(&read).await.unwrap(),
        ModbusResponse::ReadHoldingRegisters { registers: vec![7] }
    );
    connection.disconnect().await;
    shutdown.send(()).unwrap();
}

/// Records how many handler invocations are in flight at once.
#[derive(Clone)]
struct ConcurrencyProbe {
    inner: InMemoryStore,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
}

/// Decrements the live counter even if the handler future is dropped, so an
/// aborted connection cannot leave the probe reading high.
struct LiveGuard(Arc<AtomicUsize>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ConcurrencyProbe {
    fn new(inner: InMemoryStore) -> Self {
        Self {
            inner,
            live: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            served: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ModbusServer for ConcurrencyProbe {
    fn handle(
        &self,
        unit_id: u8,
        request: ModbusRequest,
    ) -> impl Future<Output = Result<ModbusResponse, ExceptionCode>> + Send {
        let inner = self.inner.clone();
        let live = self.live.clone();
        let peak = self.peak.clone();
        let served = self.served.clone();
        async move {
            let in_flight = live.fetch_add(1, Ordering::SeqCst) + 1;
            let _guard = LiveGuard(live);
            peak.fetch_max(in_flight, Ordering::SeqCst);
            // Hold the connection open long enough that every admitted client
            // overlaps with the others.
            tokio::time::sleep(Duration::from_millis(200)).await;
            served.fetch_add(1, Ordering::SeqCst);
            inner.handle(unit_id, request).await
        }
    }
}

/// `max_connections` is a hard cap: with 8 clients arriving at once against a
/// server capped at 2, at most 2 requests are ever in flight and the surplus
/// connections are closed instead of queued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_connections_caps_concurrent_clients() {
    const LIMIT: usize = 2;

    let probe = ConcurrencyProbe::new(InMemoryStore::new(16, 16, 16, 16).unwrap());
    let peak = probe.peak.clone();
    let served = probe.served.clone();
    let (addr, shutdown) = spawn_tcp_server(
        ModbusTcpServer::new(probe).with_max_connections(NonZeroUsize::new(LIMIT).unwrap()),
    )
    .await;

    let mut clients = Vec::new();
    for _client in 0..CLIENTS {
        clients.push(tokio::spawn(async move {
            // Raw sockets: the pooled client would reconnect on refusal and hide
            // the cap being enforced.
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(&[0, 1, 0, 0, 0, 6, 1, 0x03, 0, 0, 0, 1])
                .await
                .unwrap();
            let mut response = [0u8; 11];
            stream.read_exact(&mut response).await.is_ok()
        }));
    }

    let mut answered = 0;
    for client in clients {
        if client.await.unwrap() {
            answered += 1;
        }
    }

    let observed = peak.load(Ordering::SeqCst);
    assert!(
        observed <= LIMIT,
        "observed {observed} concurrent handlers, limit is {LIMIT}"
    );
    // The cap must be a cap, not an accident of scheduling: with 8 clients
    // arriving at once and a 200 ms handler, the server has to admit the full
    // allowance.
    assert_eq!(observed, LIMIT, "server never used its full allowance");
    assert!(answered > 0, "no client was served");
    assert!(
        answered < usize::from(CLIENTS),
        "every client was served, so the cap was never exercised"
    );
    assert_eq!(answered, served.load(Ordering::SeqCst));
    shutdown.send(()).unwrap();
}

/// A handler that panics takes down its own connection task only; the listener
/// and every other client keep working.
struct PanickingServer;

impl ModbusServer for PanickingServer {
    async fn handle(
        &self,
        _unit_id: u8,
        request: ModbusRequest,
    ) -> Result<ModbusResponse, ExceptionCode> {
        if matches!(request, ModbusRequest::ReadHoldingRegisters { .. }) {
            panic!("handler panicked on purpose");
        }
        Ok(ModbusResponse::WriteSingleRegister {
            address: 0,
            value: 1,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_handler_does_not_kill_the_listener() {
    let (addr, shutdown) = spawn_tcp_server(ModbusTcpServer::new(PanickingServer)).await;

    let victim = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
        .connect()
        .await
        .unwrap();
    let error = victim
        .send_message(&ModbusRequest::ReadHoldingRegisters {
            starting_address: 0,
            quantity: 1,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ModbusError::ReadError(_)
                | ModbusError::WriteError(_)
                | ModbusError::ReadTimeout
                | ModbusError::MalformedResponse(_)
        ),
        "unexpected error after handler panic: {error:?}"
    );
    victim.disconnect().await;

    // The listener is still up and still serving.
    let survivor = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
        .connect()
        .await
        .unwrap();
    assert_eq!(
        survivor
            .send_message(&ModbusRequest::WriteSingleRegister {
                address: 0,
                value: 1,
            })
            .await
            .unwrap(),
        ModbusResponse::WriteSingleRegister {
            address: 0,
            value: 1,
        }
    );
    survivor.disconnect().await;
    shutdown.send(()).unwrap();
}
