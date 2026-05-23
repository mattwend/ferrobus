// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::panic,
    clippy::uninlined_format_args,
    clippy::unwrap_used
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

mod support;

use tiny_mb::ModbusError;
use tiny_mb::tcp::{ModbusTcpConnection, ModbusTcpTimeouts};
use tiny_mb::{ModbusRequest, ModbusResponse};

use support::{
    build_exception_response_frame, build_protocol_mismatch_frame, build_tcp_response_frame,
    read_request_frame, spawn_mock_server as spawn_test_server, spawn_slow_server,
};

async fn spawn_mock_server<F, Fut>(handler: F) -> SocketAddr
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    spawn_test_server(handler).await.unwrap()
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
    build_tcp_response_frame(tid, unit_id, &pdu)
}

fn make_write_single_register_response(tid: u16, unit_id: u8, address: u16, value: u16) -> Vec<u8> {
    let pdu = vec![
        6u8,
        (address >> 8) as u8,
        address as u8,
        (value >> 8) as u8,
        value as u8,
    ];
    build_tcp_response_frame(tid, unit_id, &pdu)
}

#[tokio::test]
async fn send_read_coils_success() {
    let addr = spawn_mock_server(|mut stream| async move {
        let request = read_request_frame(&mut stream).await.unwrap();

        let response = make_read_coils_response(
            request.transaction_id,
            request.unit_id,
            &[true, false, true, false, true, false, true, false],
        );
        stream.write_all(&response).await.unwrap();
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
    let addr = spawn_mock_server(|mut stream| async move {
        let request = read_request_frame(&mut stream).await.unwrap();

        let address = u16::from_be_bytes([request.pdu[1], request.pdu[2]]);
        let value = u16::from_be_bytes([request.pdu[3], request.pdu[4]]);

        let response = make_write_single_register_response(
            request.transaction_id,
            request.unit_id,
            address,
            value,
        );
        stream.write_all(&response).await.unwrap();
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
    let addr = spawn_mock_server(|mut stream| async move {
        for _ in 0..2 {
            let request = read_request_frame(&mut stream).await.unwrap();
            let response =
                make_read_coils_response(request.transaction_id, request.unit_id, &[true, false]);
            stream.write_all(&response).await.unwrap();
        }
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
async fn concurrent_in_flight_requests_are_matched_out_of_order() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let first = read_request_frame(&mut stream).await.unwrap();
        let second = read_request_frame(&mut stream).await.unwrap();

        let second_response =
            make_read_coils_response(second.transaction_id, second.unit_id, &[false, true]);
        stream.write_all(&second_response).await.unwrap();

        let first_response =
            make_read_coils_response(first.transaction_id, first.unit_id, &[true, false]);
        stream.write_all(&first_response).await.unwrap();
    });

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };

    let first_conn = conn.clone();
    let second_conn = conn.with_unit_id(2);

    let (first_response, second_response) = tokio::join!(
        first_conn.send_message(&request),
        second_conn.send_message(&request)
    );

    assert_eq!(
        first_response.unwrap(),
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
    assert_eq!(
        second_response.unwrap(),
        ModbusResponse::ReadCoils {
            coils: vec![false, true]
        }
    );
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut first_stream, _) = listener.accept().await.unwrap();
        let mut first_header = [0u8; 7];
        first_stream.read_exact(&mut first_header).await.unwrap();
        first_header[4] = 0;
        first_header[5] = 0;
        first_stream.write_all(&first_header).await.unwrap();

        let (mut second_stream, _) = listener.accept().await.unwrap();
        let mut second_header = [0u8; 7];
        second_stream.read_exact(&mut second_header).await.unwrap();
        let tid = u16::from_be_bytes([second_header[0], second_header[1]]);
        let unit_id = second_header[6];

        let pdu_len = 5;
        let mut pdu = vec![0u8; pdu_len as usize];
        second_stream.read_exact(&mut pdu).await.unwrap();

        let response = make_read_coils_response(tid, unit_id, &[true, false]);
        second_stream.write_all(&response).await.unwrap();
    });

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };
    let response = conn.send_message(&request).await.unwrap();
    assert_eq!(
        response,
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
}

#[tokio::test]
async fn send_messages_across_multiple_unit_ids_on_one_connection() {
    let addr = spawn_mock_server(|mut stream| async move {
        for expected_unit_id in [1u8, 2u8] {
            let request = read_request_frame(&mut stream).await.unwrap();
            assert_eq!(request.unit_id, expected_unit_id);

            let response =
                make_read_coils_response(request.transaction_id, request.unit_id, &[true, false]);
            stream.write_all(&response).await.unwrap();
        }
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };

    let default_response = conn.send_message(&request).await.unwrap();
    let alternate_response = conn.send_message_with_unit_id(2, &request).await.unwrap();

    assert_eq!(
        default_response,
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
    assert_eq!(
        alternate_response,
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
}

#[tokio::test]
async fn send_message_returns_exception_response_as_typed_error() {
    let addr = spawn_mock_server(|mut stream| async move {
        let request = read_request_frame(&mut stream).await.unwrap();
        let response =
            build_exception_response_frame(request.transaction_id, request.unit_id, 0x01, 0x02);
        stream.write_all(&response).await.unwrap();
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };

    let error = conn.send_message(&request).await.unwrap_err();
    match error {
        ModbusError::ExceptionResponse { function, code } => {
            assert_eq!(function, 0x81);
            assert_eq!(code, 0x02);
        }
        other => panic!("Expected ExceptionResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn send_message_returns_protocol_id_mismatch_as_typed_error() {
    let addr = spawn_mock_server(|mut stream| async move {
        let request = read_request_frame(&mut stream).await.unwrap();
        let response = build_protocol_mismatch_frame(
            request.transaction_id,
            1,
            request.unit_id,
            &[0x01, 0x01, 0x01],
        );
        stream.write_all(&response).await.unwrap();
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 1,
    };

    let error = conn.send_message(&request).await.unwrap_err();
    match error {
        ModbusError::ProtocolIdMismatch { actual } => assert_eq!(actual, 1),
        other => panic!("Expected ProtocolIdMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn send_message_returns_unit_id_mismatch_as_typed_error() {
    let addr = spawn_mock_server(|mut stream| async move {
        let request = read_request_frame(&mut stream).await.unwrap();
        // Reply with a different unit id than was requested to exercise the
        // `UnitIdMismatch` validation path in `send_message_with_unit_id`.
        let response = make_read_coils_response(
            request.transaction_id,
            request.unit_id.wrapping_add(1),
            &[true, false],
        );
        stream.write_all(&response).await.unwrap();
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };

    let error = conn.send_message(&request).await.unwrap_err();
    match error {
        ModbusError::UnitIdMismatch { expected, actual } => {
            assert_eq!(expected, 1);
            assert_eq!(actual, 2);
        }
        other => panic!("Expected UnitIdMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn send_message_returns_read_timeout_from_slow_server() {
    let addr = spawn_slow_server(Duration::from_millis(200)).await.unwrap();

    let conn = ModbusTcpConnection::with_timeouts(
        addr.ip(),
        addr.port(),
        1,
        0,
        ModbusTcpTimeouts {
            connect_timeout: Duration::from_millis(50),
            write_timeout: Duration::from_millis(50),
            read_timeout: Duration::from_millis(25),
        },
    );
    conn.connect().await.unwrap();

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 1,
    };

    let error = conn.send_message(&request).await.unwrap_err();
    match error {
        ModbusError::ReadTimeout => {}
        other => panic!("Expected ReadTimeout, got {other:?}"),
    }
}

#[tokio::test]
async fn stray_response_with_unknown_tid_is_ignored() {
    let addr = spawn_mock_server(|mut stream| async move {
        let request = read_request_frame(&mut stream).await.unwrap();
        let stray = make_read_coils_response(
            request.transaction_id.wrapping_add(100),
            request.unit_id,
            &[true],
        );
        stream.write_all(&stray).await.unwrap();

        let response =
            make_read_coils_response(request.transaction_id, request.unit_id, &[true, false]);
        stream.write_all(&response).await.unwrap();
    })
    .await;

    let conn = ModbusTcpConnection::new(addr.ip(), addr.port(), 1, 0);
    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };

    let response = conn.send_message(&request).await.unwrap();
    assert_eq!(
        response,
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
}

#[tokio::test]
async fn caller_future_cancellation_does_not_break_following_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));

    tokio::spawn({
        let release_rx = Arc::clone(&release_rx);
        async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first = read_request_frame(&mut stream).await.unwrap();
            let _ = first;
            let _ = started_tx.send(());
            if let Some(rx) = release_rx.lock().await.take() {
                let _ = rx.await;
            }

            let second = read_request_frame(&mut stream).await.unwrap();
            let response =
                make_read_coils_response(second.transaction_id, second.unit_id, &[true, false]);
            stream.write_all(&response).await.unwrap();
        }
    });

    let conn = ModbusTcpConnection::with_timeouts(
        addr.ip(),
        addr.port(),
        1,
        0,
        ModbusTcpTimeouts {
            connect_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(1),
            read_timeout: Duration::from_secs(1),
        },
    );

    let request = ModbusRequest::ReadCoils {
        starting_address: 0x0000,
        quantity: 2,
    };

    let task = tokio::spawn({
        let conn = conn.clone();
        let request = request.clone();
        async move { conn.send_message(&request).await }
    });
    started_rx.await.unwrap();
    task.abort();
    let _ = release_tx.send(());

    let response = conn.send_message(&request).await.unwrap();
    assert_eq!(
        response,
        ModbusResponse::ReadCoils {
            coils: vec![true, false]
        }
    );
}
