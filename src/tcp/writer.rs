// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Cancellation-safe Modbus TCP ADU writes.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::error::ModbusError;
use crate::tcp::tcp_connection::ModbusTcpConnection;

#[derive(Clone)]
pub(crate) struct TearDown {
    pub(crate) connection: ModbusTcpConnection,
    pub(crate) generation: u64,
}

/// Writes one complete Modbus TCP ADU on a background task.
///
/// The background task owns the writer mutex guard until the frame is fully
/// written and flushed, or until `write_timeout` expires. If the caller future
/// is cancelled while the write is in progress, the task continues to preserve
/// stream framing for subsequent requests.
///
/// # Arguments
///
/// * `writer` - Shared TCP write half for the connected state.
/// * `adu` - Complete Modbus TCP ADU bytes to send.
/// * `write_timeout` - Maximum duration for the write and flush operation.
/// * `teardown` - Connection state and generation used to invalidate stale state on failure.
///
/// # Errors
///
/// Returns [`ModbusError::WriteError`] for socket failures or writer task
/// failures, and [`ModbusError::WriteTimeout`] when the operation exceeds
/// `write_timeout`.
pub(crate) async fn write_adu_cancellation_safe(
    writer: Arc<Mutex<OwnedWriteHalf>>,
    adu: Vec<u8>,
    write_timeout: Duration,
    teardown: TearDown,
) -> Result<(), ModbusError> {
    // Keep this spawned instead of inlining the write in the caller future:
    // dropping a `JoinHandle` does not abort its task, so caller cancellation
    // cannot leave a partial ADU on the socket before the next writer runs.
    let writer_teardown = teardown.clone();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer.lock_owned().await;
        let result = timeout(write_timeout, async {
            writer.write_all(&adu).await?;
            writer.flush().await
        })
        .await
        .map_err(|_| ModbusError::WriteTimeout)?
        .map_err(ModbusError::WriteError);

        if result.is_err() {
            writer_teardown
                .connection
                .invalidate(Some(writer_teardown.generation))
                .await;
        }

        result
    });

    match writer_task.await {
        Ok(result) => result,
        Err(error) => {
            teardown
                .connection
                .invalidate(Some(teardown.generation))
                .await;
            Err(ModbusError::WriteError(io::Error::other(format!(
                "writer task failed: {error}"
            ))))
        }
    }
}
