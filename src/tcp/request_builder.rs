// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Per-request options for [`ModbusTcpConnection`].

use std::time::Duration;

use crate::tcp::queue::Priority;
use crate::tcp::tcp_connection::{ModbusTcpConnection, SendOptions};
use crate::{ModbusRequest, ModbusResponse, error::ModbusError};

/// Builder for a single request with per-request options.
///
/// Created by [`ModbusTcpConnection::request`]. Every option starts at the
/// handle's default, so only what differs for this request has to be set. Nothing
/// is queued until [`Self::send`] is awaited.
#[derive(Debug)]
#[must_use = "a request builder does nothing until `send` is awaited"]
pub struct RequestBuilder<'a> {
    connection: &'a ModbusTcpConnection,
    pdu: &'a ModbusRequest,
    options: SendOptions,
}

impl<'a> RequestBuilder<'a> {
    pub(crate) fn new(
        connection: &'a ModbusTcpConnection,
        pdu: &'a ModbusRequest,
        options: SendOptions,
    ) -> Self {
        Self {
            connection,
            pdu,
            options,
        }
    }

    /// Sets the queue [`Priority`] of this request.
    ///
    /// Defaults to the handle's priority, see
    /// [`ModbusTcpConnection::with_priority`].
    pub fn priority(mut self, priority: Priority) -> Self {
        self.options.priority = priority;
        self
    }

    /// Sets the unit id this request is addressed to.
    ///
    /// Defaults to the handle's unit id.
    pub fn unit_id(mut self, unit_id: u8) -> Self {
        self.options.unit_id = unit_id;
        self
    }

    /// Sets the maximum time this request may wait before reaching the wire.
    ///
    /// The time covers both waiting for admission to a full queue and waiting in
    /// the queue. Defaults to the handle's
    /// [`queue_timeout`](crate::tcp::ModbusTcpFlowControl::queue_timeout). A short
    /// timeout suits control commands, where a stale write can be worse than a
    /// failed one. A zero timeout always fails with [`ModbusError::QueueTimeout`].
    pub fn queue_timeout(mut self, queue_timeout: Duration) -> Self {
        self.options.queue_timeout = queue_timeout;
        self
    }

    /// Queues the request and waits for its response.
    ///
    /// Behaves like [`ModbusTcpConnection::send_message`], including the handle's
    /// retry policy; every retry attempt is queued again with the same options.
    ///
    /// # Errors
    ///
    /// Returns transport, protocol, validation, exception-response, or
    /// request/response mismatch errors.
    pub async fn send(self) -> Result<ModbusResponse, ModbusError> {
        self.connection
            .send_with_options(self.options, self.pdu)
            .await
    }
}
