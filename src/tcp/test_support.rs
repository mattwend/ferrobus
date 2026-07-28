// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Shared test helpers for the TCP submodules.

#![allow(clippy::unwrap_used)]

use tokio::net::TcpListener;

/// Accepts every incoming connection and holds each one open.
///
/// Holding the accepted streams keeps the peer side alive, so a handle observes
/// `connected: true` until it tears the socket down itself, and repeated dials
/// (reconnects) are all served by the same server task.
pub(crate) async fn accept_and_hold_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    addr
}

/// Returns an address with no listener bound, so a dial to it fails fast.
pub(crate) async fn dead_server_addr() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}
