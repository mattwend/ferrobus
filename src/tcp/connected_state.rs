// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Connected socket state and spawned response reader lifecycle.

use std::convert::Infallible;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tokio::io::AsyncReadExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error};

use crate::error::ModbusError;
use crate::tcp::frame::{MBAP_HEADER_LEN, response_body_len_from_header};
use crate::tcp::pending::Pending;

/// Shared state for one established Modbus TCP socket.
///
/// A [`ConnectedState`] owns the split TCP halves for a single connection
/// generation. The writer half is shared by request tasks, while the reader task
/// continuously receives MBAP frames and completes the matching pending waiter.
/// Dropping the state aborts the reader and fails all still-pending requests.
pub(crate) struct ConnectedState {
    /// Shared TCP write half used by cancellation-safe writer tasks.
    ///
    /// `lock_owned` requires an `Arc<Mutex<_>>`; the spawned writer task uses it to
    /// keep a full ADU write cancellation-safe after the caller future is dropped.
    pub(crate) writer: Arc<Mutex<OwnedWriteHalf>>,
    /// Transaction-id keyed response waiters for requests in flight on this socket.
    pub(crate) pending: Arc<Pending>,
    /// Background task that reads response frames and routes them to `pending`.
    pub(crate) reader_task: JoinHandle<()>,
    /// Monotonic connection generation used to ignore stale teardown attempts.
    pub(crate) generation: u64,
}

impl std::fmt::Debug for ConnectedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectedState")
            .field("generation", &self.generation)
            .field("reader_finished", &self.reader_task.is_finished())
            .finish_non_exhaustive()
    }
}

impl Drop for ConnectedState {
    fn drop(&mut self) {
        self.reader_task.abort();
        Self::drain_pending(
            &self.pending,
            io::ErrorKind::ConnectionAborted,
            "connection state replaced or dropped",
        );
    }
}

impl ConnectedState {
    /// Drains all in-flight request waiters with a transport error.
    pub(crate) fn drain_pending(pending: &Pending, kind: io::ErrorKind, reason: &'static str) {
        match pending.lock() {
            Ok(mut map) => {
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err(ModbusError::ReadError(io::Error::new(kind, reason))));
                }
            }
            Err(_) => {
                error!(
                    reason,
                    "pending map poisoned while draining in-flight requests"
                );
            }
        }
    }

    /// Runs the response reader task and drains pending callers when it exits.
    ///
    /// # Arguments
    ///
    /// * `read_half` - TCP read half for the connected state.
    /// * `pending` - Shared pending-response map for the same socket generation.
    ///
    /// # Returns
    ///
    /// Returns when the reader loop encounters a read or frame error, or when a
    /// panic is caught by the safety net. All pending waiters receive an error
    /// before the task exits.
    pub(crate) async fn reader_loop(mut read_half: OwnedReadHalf, pending: Arc<Pending>) {
        let reader = Self::run_reader_loop(&mut read_half, Arc::clone(&pending));

        // Safety net: the reader loop should never panic (all fallible ops use `?`),
        // but if a bug or dependency causes one, catch_unwind ensures pending callers
        // get an error instead of silently deadlocking on their oneshot receivers.
        // AssertUnwindSafe is required because the captured state (Arc<StdMutex<…>>,
        // OwnedReadHalf) is !UnwindSafe; the drain_pending call below either restores
        // state or logs the degradation.
        let result = AssertUnwindSafe(reader)
            .catch_unwind()
            .await
            .map_err(|panic| {
                let message = if let Some(message) = panic.downcast_ref::<&str>() {
                    *message
                } else if let Some(message) = panic.downcast_ref::<String>() {
                    message.as_str()
                } else {
                    "unknown panic payload"
                };
                error!(message, "reader task panicked");
                ModbusError::ReadError(io::Error::other("reader task panicked"))
            })
            .and_then(|inner| inner);

        match result {
            Err(ModbusError::ReadError(error)) => {
                let kind = error.kind();
                Self::drain_pending(&pending, kind, "reader task terminated");
            }
            Err(ModbusError::MalformedResponse(_)) => {
                Self::drain_pending(
                    &pending,
                    io::ErrorKind::InvalidData,
                    "reader task received malformed response",
                );
            }
            Err(_) => {
                Self::drain_pending(&pending, io::ErrorKind::Other, "reader task terminated");
            }
        }
    }

    /// Continuously reads Modbus TCP responses and routes them to waiting callers.
    ///
    /// # Arguments
    /// * `read_half` - The TCP read half used to receive MBAP-framed responses.
    /// * `pending` - Map of in-flight transaction IDs to waiting response channels.
    ///
    /// # Returns
    /// Returns `Err(ModbusError)` when reading, frame validation, or pending-map access fails.
    /// On success this function never returns.
    pub(crate) async fn run_reader_loop(
        read_half: &mut OwnedReadHalf,
        pending: Arc<Pending>,
    ) -> Result<Infallible, ModbusError> {
        loop {
            let mut header_buffer = [0u8; MBAP_HEADER_LEN];
            read_half
                .read_exact(&mut header_buffer)
                .await
                .map_err(ModbusError::ReadError)?;

            let body_len = response_body_len_from_header(header_buffer).map_err(|error| {
                ModbusError::MalformedResponse(format!("invalid MBAP header: {error}"))
            })?;

            let mut response_buffer = vec![0u8; MBAP_HEADER_LEN + body_len];
            response_buffer[..MBAP_HEADER_LEN].copy_from_slice(&header_buffer);
            read_half
                .read_exact(&mut response_buffer[MBAP_HEADER_LEN..])
                .await
                .map_err(ModbusError::ReadError)?;

            let tid = u16::from_be_bytes([header_buffer[0], header_buffer[1]]);
            let sender = {
                let mut map = pending.lock().map_err(|_| {
                    ModbusError::ReadError(io::Error::other("pending map poisoned"))
                })?;
                match map.remove(&tid) {
                    Some(sender) => {
                        debug!(tid, in_flight = map.len(), "pending response matched");
                        Some(sender)
                    }
                    None => None,
                }
            };

            match sender {
                Some(tx) => {
                    let _ = tx.send(Ok(response_buffer));
                }
                None => {
                    debug!(tid, "stray response, ignored");
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    /// Establishes a loopback TCP connection and returns the client read/write
    /// halves alongside the accepted server stream.
    async fn connected_halves() -> (OwnedReadHalf, OwnedWriteHalf, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (read_half, write_half) = client.into_split();
        (read_half, write_half, server)
    }

    #[tokio::test]
    async fn debug_reports_generation_and_reader_state() {
        let (read_half, write_half, _server) = connected_halves().await;
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let reader_task = tokio::spawn(async {});
        let state = ConnectedState {
            writer: Arc::new(Mutex::new(write_half)),
            pending,
            reader_task,
            generation: 42,
        };

        let rendered = format!("{state:?}");

        assert!(rendered.contains("generation: 42"));
        assert!(rendered.contains("reader_finished"));
        drop(read_half);
    }

    #[tokio::test]
    async fn drain_pending_sends_error_to_waiters() {
        let pending = StdMutex::new(HashMap::new());
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(1u16, tx);

        ConnectedState::drain_pending(&pending, io::ErrorKind::ConnectionReset, "boom");

        let error = rx.await.unwrap().unwrap_err();
        match error {
            ModbusError::ReadError(io_error) => {
                assert_eq!(io_error.kind(), io::ErrorKind::ConnectionReset);
            }
            other => panic!("expected ReadError, got {other:?}"),
        }
    }

    #[test]
    fn drain_pending_tolerates_poisoned_map() {
        let pending: Arc<Pending> = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().insert(7u16, tx);
        let poisoner = Arc::clone(&pending);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the pending map");
        })
        .join();

        // Must not panic even though the mutex is poisoned; because the lock
        // cannot be acquired, the existing waiter remains in the poisoned map.
        ConnectedState::drain_pending(&pending, io::ErrorKind::InvalidData, "poisoned");

        let lock_error = pending.lock().unwrap_err();
        assert!(lock_error.into_inner().contains_key(&7));
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn run_reader_loop_rejects_malformed_header() {
        let (mut read_half, _write_half, mut server) = connected_halves().await;
        // MBAP length field of 0 is below the minimum because it omits the
        // required unit identifier byte.
        server.write_all(&[0, 1, 0, 0, 0, 0, 1]).await.unwrap();

        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let error = ConnectedState::run_reader_loop(&mut read_half, pending)
            .await
            .unwrap_err();

        assert!(matches!(error, ModbusError::MalformedResponse(_)));
    }

    #[tokio::test]
    async fn reader_loop_drains_pending_on_malformed_header() {
        let (read_half, _write_half, mut server) = connected_halves().await;
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(7u16, tx);

        server.write_all(&[0, 7, 0, 0, 0, 0, 1]).await.unwrap();
        ConnectedState::reader_loop(read_half, Arc::clone(&pending)).await;

        let error = rx.await.unwrap().unwrap_err();
        match error {
            ModbusError::ReadError(io_error) => {
                assert_eq!(io_error.kind(), io::ErrorKind::InvalidData);
            }
            other => panic!("expected ReadError(InvalidData), got {other:?}"),
        }
    }
}
