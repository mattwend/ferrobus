// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use tiny_mb::tcp::ModbusTcpConnection;
use tiny_mb::{ModbusRequest, ModbusResponse};

async fn spawn_mock_server<F>(handler: F) -> SocketAddr
where
    F: FnOnce(TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        handler(stream);
    });

    addr
}

fn make_read_coils_response(tid: u16, unit_id: u8, coils: &[bool]) -> Vec<u8> {
    let byte_count = coils.len().div_ceil(8) as u8;
    let mut pdu = vec![1u8, byte_count];
    for i in (0..coils.len()).step_by(8) {
        let mut byte = 0u8;
        for bit in 0..8 {
            if i + bit < coils.len() && coils[i + bit] {
                byte |= 1 << bit;
            }
        }
        pdu.push(byte);
    }
    let pdu_len = pdu.len() as u16;
    let mut frame = Vec::with_capacity(7 + pdu.len());
    frame.extend_from_slice(&tid.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&(1 + pdu_len).to_be_bytes());
    frame.push(unit_id);
    frame.extend_from_slice(&pdu);
    frame
}

fn make_write_single_register_response(tid: u16, unit_id: u8, address: u16, value: u16) -> Vec<u8> {
    let pdu = vec![
        6u8,
        (address >> 8) as u8,
        address as u8,
        (value >> 8) as u8,
        value as u8,
    ];
    let pdu_len = pdu.len() as u16;
    let mut frame = Vec::with_capacity(7 + pdu.len());
    frame.extend_from_slice(&tid.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&(1 + pdu_len).to_be_bytes());
    frame.push(unit_id);
    frame.extend_from_slice(&pdu);
    frame
}

#[tokio::test]
async fn send_read_coils_success() {
    let addr = spawn_mock_server(|mut stream| {
        tokio::spawn(async move {
            let mut header = [0u8; 7];
            stream.read_exact(&mut header).await.unwrap();
            let tid = u16::from_be_bytes([header[0], header[1]]);
            let unit_id = header[6];

            let pdu_len = 5;
            let mut pdu = vec![0u8; pdu_len as usize];
            stream.read_exact(&mut pdu).await.unwrap();

            let response = make_read_coils_response(
                tid,
                unit_id,
                &[true, false, true, false, true, false, true, false],
            );
            stream.write_all(&response).await.unwrap();
        });
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 8,
    };
    let response = conn.send_message(&request).await.unwrap();

    assert_eq!(
        response,
        ModbusResponse::ReadCoils {
            coils: vec![true, false, true, false, true, false, true, false]
        }
    );
}

#[tokio::test]
async fn send_write_single_register_success() {
    let addr = spawn_mock_server(|mut stream| {
        tokio::spawn(async move {
            let mut header = [0u8; 7];
            stream.read_exact(&mut header).await.unwrap();
            let tid = u16::from_be_bytes([header[0], header[1]]);
            let unit_id = header[6];

            let pdu_len = 5;
            let mut pdu = vec![0u8; pdu_len as usize];
            stream.read_exact(&mut pdu).await.unwrap();

            let address = u16::from_be_bytes([pdu[1], pdu[2]]);
            let value = u16::from_be_bytes([pdu[3], pdu[4]]);

            let response = make_write_single_register_response(tid, unit_id, address, value);
            stream.write_all(&response).await.unwrap();
        });
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::WriteSingleRegister {
        address: 0x0010,
        value: 0x1234,
    };
    let response = conn.send_message(&request).await.unwrap();

    assert_eq!(
        response,
        ModbusResponse::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234
        }
    );
}

#[tokio::test]
async fn transaction_id_increments() {
    let addr = spawn_mock_server(|stream| {
        tokio::spawn(async move {
            let mut stream = stream;
            for _ in 0..2 {
                let mut header = [0u8; 7];
                stream.read_exact(&mut header).await.unwrap();
                let tid = u16::from_be_bytes([header[0], header[1]]);

                let pdu_len = 5;
                let mut pdu = vec![0u8; pdu_len as usize];
                stream.read_exact(&mut pdu).await.unwrap();

                let response = make_read_coils_response(tid, header[6], &[true, false]);
                stream.write_all(&response).await.unwrap();
            }
        });
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 100);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };
    let _ = conn.send_message(&request).await.unwrap();
    let _ = conn.send_message(&request).await.unwrap();
}

#[tokio::test]
async fn transaction_id_mismatch() {
    let addr = spawn_mock_server(|mut stream| {
        tokio::spawn(async move {
            let mut header = [0u8; 7];
            stream.read_exact(&mut header).await.unwrap();
            let wrong_tid = u16::from_be_bytes([header[0], header[1]]) + 1;

            let pdu_len = 5;
            let mut pdu = vec![0u8; pdu_len as usize];
            stream.read_exact(&mut pdu).await.unwrap();

            let response = make_read_coils_response(wrong_tid, header[6], &[true, false]);
            stream.write_all(&response).await.unwrap();
        });
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };
    let result = conn.send_message(&request).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        tiny_mb::ModbusError::TransactionIdMismatch { .. } => {}
        e => panic!("Expected TransactionIdMismatch, got {:?}", e),
    }
}

#[tokio::test]
async fn server_disconnects_on_write() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU8;
    use tokio::sync::Mutex as TokioMutex;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let attempt: Arc<AtomicU8> = Arc::new(AtomicU8::new(0));
    let listener = Arc::new(TokioMutex::new(listener));

    tokio::spawn({
        let listener = Arc::clone(&listener);
        let attempt = Arc::clone(&attempt);
        async move {
            for _ in 0..2 {
                let listener = listener.lock().await;
                let (stream, _) = listener.accept().await.unwrap();
                let attempt_val = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                tokio::spawn(async move {
                    let mut stream = stream;
                    if attempt_val == 0 {
                        let mut header = [0u8; 7];
                        let _ = stream.read_exact(&mut header).await;
                        let _ = stream.read_exact(&mut [0u8; 5]).await;
                        drop(stream);
                    } else {
                        let mut header = [0u8; 7];
                        stream.read_exact(&mut header).await.unwrap();
                        let tid = u16::from_be_bytes([header[0], header[1]]);
                        let unit_id = header[6];

                        let pdu_len = 5;
                        let mut pdu = vec![0u8; pdu_len as usize];
                        stream.read_exact(&mut pdu).await.unwrap();

                        let response = make_read_coils_response(tid, unit_id, &[true, false]);
                        stream.write_all(&response).await.unwrap();
                    }
                });
            }
        }
    });

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };
    let result = conn.send_message(&request).await;

    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
}

#[tokio::test]
async fn server_disconnects_on_header_read() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU8;
    use tokio::sync::Mutex as TokioMutex;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let attempt: Arc<AtomicU8> = Arc::new(AtomicU8::new(0));
    let listener = Arc::new(TokioMutex::new(listener));

    tokio::spawn({
        let listener = Arc::clone(&listener);
        let attempt = Arc::clone(&attempt);
        async move {
            for _ in 0..2 {
                let listener = listener.lock().await;
                let (stream, _) = listener.accept().await.unwrap();
                let attempt_val = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                tokio::spawn(async move {
                    let mut stream = stream;
                    if attempt_val == 0 {
                        let mut header = [0u8; 4];
                        let _ = stream.read_exact(&mut header).await;
                        drop(stream);
                    } else {
                        let mut header = [0u8; 7];
                        stream.read_exact(&mut header).await.unwrap();
                        let tid = u16::from_be_bytes([header[0], header[1]]);
                        let unit_id = header[6];

                        let pdu_len = 5;
                        let mut pdu = vec![0u8; pdu_len as usize];
                        stream.read_exact(&mut pdu).await.unwrap();

                        let response = make_read_coils_response(tid, unit_id, &[true, false]);
                        stream.write_all(&response).await.unwrap();
                    }
                });
            }
        }
    });

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };
    let result = conn.send_message(&request).await;

    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
}

#[tokio::test]
async fn server_sends_invalid_mbap_length() {
    let addr = spawn_mock_server(|mut stream| {
        tokio::spawn(async move {
            let mut header = [0u8; 7];
            stream.read_exact(&mut header).await.unwrap();

            header[4] = 0;
            header[5] = 0;

            stream.write_all(&header).await.unwrap();
        });
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };
    let result = conn.send_message(&request).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        tiny_mb::ModbusError::ResponseError(msg) => {
            assert!(msg.contains("Invalid MBAP length"));
        }
        e => panic!("Expected ResponseError, got {:?}", e),
    }
}
