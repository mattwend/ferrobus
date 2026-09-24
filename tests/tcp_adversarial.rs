// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

#![allow(missing_docs, clippy::panic, clippy::unwrap_used)]

mod support;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use ferrobus::server::InMemoryStore;
use ferrobus::tcp::ModbusTcpServer;
use support::{
    build_tcp_response_frame, read_request_frame, spawn_tcp_server, spawn_tcp_server_with_task,
};

#[tokio::test]
async fn bad_client_does_not_kill_listener() {
    let store = InMemoryStore::new(8, 8, 8, 8).unwrap();
    let (addr, shutdown) = spawn_tcp_server(ModbusTcpServer::new(store)).await;

    let mut partial = TcpStream::connect(addr).await.unwrap();
    partial.write_all(&[0, 1, 0]).await.unwrap();
    drop(partial);

    let mut invalid_protocol = TcpStream::connect(addr).await.unwrap();
    invalid_protocol
        .write_all(&[0, 1, 0, 1, 0, 6, 1, 3, 0, 0, 0, 1])
        .await
        .unwrap();
    drop(invalid_protocol);

    let mut oversized = TcpStream::connect(addr).await.unwrap();
    oversized.write_all(&[0, 1, 0, 0, 1, 44, 1]).await.unwrap();
    drop(oversized);

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&build_tcp_response_frame(1, 1, &[0x07]))
        .await
        .unwrap();
    assert_eq!(
        read_request_frame(&mut stream).await.unwrap().pdu,
        vec![0x87, 0x01]
    );

    stream
        .write_all(&build_tcp_response_frame(2, 1, &[0x03, 0x00]))
        .await
        .unwrap();
    assert_eq!(
        read_request_frame(&mut stream).await.unwrap().pdu,
        vec![0x83, 0x03]
    );

    let first = build_tcp_response_frame(3, 1, &[0x03, 0, 0, 0, 1]);
    let second = build_tcp_response_frame(4, 1, &[0x03, 0, 0, 0, 1]);
    stream.write_all(&[first, second].concat()).await.unwrap();
    assert_eq!(
        read_request_frame(&mut stream).await.unwrap().pdu,
        vec![0x03, 0x02, 0x00, 0x00]
    );
    assert_eq!(
        read_request_frame(&mut stream).await.unwrap().pdu,
        vec![0x03, 0x02, 0x00, 0x00]
    );

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn shutdown_closes_idle_connections_without_waiting_for_the_drain_deadline() {
    let store = InMemoryStore::new(4, 4, 4, 4).unwrap();
    let (addr, shutdown, task) = spawn_tcp_server_with_task(ModbusTcpServer::new(store)).await;

    // An idle client keeps its socket open; shutdown must not wait for the read
    // timeout (30 s) or the drain deadline (10 s).
    let _idle = TcpStream::connect(addr).await.unwrap();
    let mut probe = TcpStream::connect(addr).await.unwrap();
    probe
        .write_all(&[0, 1, 0, 0, 0, 6, 1, 0x03, 0, 0, 0, 1])
        .await
        .unwrap();
    let mut response = [0u8; 11];
    probe.read_exact(&mut response).await.unwrap();

    let started = tokio::time::Instant::now();
    shutdown.send(()).unwrap();
    task.await.unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "shutdown took {elapsed:?}, expected idle connections to exit promptly"
    );
}
