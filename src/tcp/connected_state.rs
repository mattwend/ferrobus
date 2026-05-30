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

pub(crate) struct ConnectedState {
    // `lock_owned` requires an `Arc<Mutex<_>>`; the spawned writer task uses it to
    // keep a full ADU write cancellation-safe after the caller future is dropped.
    pub(crate) writer: Arc<Mutex<OwnedWriteHalf>>,
    pub(crate) pending: Arc<Pending>,
    pub(crate) reader_task: JoinHandle<()>,
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
