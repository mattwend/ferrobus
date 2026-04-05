// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::sleep;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRequest {
    pub transaction_id: u16,
    pub protocol_id: u16,
    pub unit_id: u8,
    pub pdu: Vec<u8>,
}

pub fn build_tcp_response_frame(transaction_id: u16, unit_id: u8, pdu: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(7 + pdu.len());
    frame.extend_from_slice(&transaction_id.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&((1 + pdu.len()) as u16).to_be_bytes());
    frame.push(unit_id);
    frame.extend_from_slice(pdu);
    frame
}

pub fn build_exception_response_frame(
    transaction_id: u16,
    unit_id: u8,
    function: u8,
    code: u8,
) -> Vec<u8> {
    build_tcp_response_frame(transaction_id, unit_id, &[function | 0x80, code])
}

pub fn build_protocol_mismatch_frame(
    transaction_id: u16,
    protocol_id: u16,
    unit_id: u8,
    pdu: &[u8],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(7 + pdu.len());
    frame.extend_from_slice(&transaction_id.to_be_bytes());
    frame.extend_from_slice(&protocol_id.to_be_bytes());
    frame.extend_from_slice(&((1 + pdu.len()) as u16).to_be_bytes());
    frame.push(unit_id);
    frame.extend_from_slice(pdu);
    frame
}

pub async fn read_request_frame(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
    let mut header = [0u8; 7];
    stream.read_exact(&mut header).await?;

    let transaction_id = u16::from_be_bytes([header[0], header[1]]);
    let protocol_id = u16::from_be_bytes([header[2], header[3]]);
    let remaining_len = u16::from_be_bytes([header[4], header[5]]) as usize;
    let pdu_len = remaining_len.saturating_sub(1);

    let mut pdu = vec![0u8; pdu_len];
    stream.read_exact(&mut pdu).await?;

    Ok(CapturedRequest {
        transaction_id,
        protocol_id,
        unit_id: header[6],
        pdu,
    })
}

pub async fn spawn_mock_server<F, Fut>(handler: F) -> std::io::Result<SocketAddr>
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        handler(stream).await;
    });

    Ok(addr)
}

pub async fn spawn_slow_server(delay: Duration) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _stream = stream;
                sleep(delay).await;
            });
        }
    });

    Ok(addr)
}
