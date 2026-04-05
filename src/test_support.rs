// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::ModbusRequest;
use crate::request::serialize_modbus_request;
use crate::tcp::build_modbus_tcp_adu;

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

pub fn build_request_frame(transaction_id: u16, unit_id: u8, request: &ModbusRequest) -> Vec<u8> {
    build_modbus_tcp_adu(transaction_id, unit_id, request)
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

pub async fn write_frame(stream: &mut TcpStream, frame: &[u8]) -> std::io::Result<()> {
    stream.write_all(frame).await
}

pub fn serialized_request_pdu(request: &ModbusRequest) -> Vec<u8> {
    serialize_modbus_request(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exception_frame_sets_exception_bit() {
        let frame = build_exception_response_frame(1, 2, 3, 4);

        assert_eq!(&frame[..7], &[0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x02]);
        assert_eq!(&frame[7..], &[0x83, 0x04]);
    }

    #[test]
    fn request_frame_matches_public_builder() {
        let request = ModbusRequest::ReadHoldingRegisters {
            starting_address: 0x0010,
            quantity: 2,
        };

        let frame = build_request_frame(0x1234, 0x11, &request);

        assert_eq!(frame, build_modbus_tcp_adu(0x1234, 0x11, &request));
    }

    #[test]
    fn serialized_request_pdu_matches_request_serializer() {
        let request = ModbusRequest::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };

        assert_eq!(
            serialized_request_pdu(&request),
            serialize_modbus_request(&request)
        );
    }
}
