// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Configurable Modbus TCP socket phase.

use tokio::sync::{mpsc, oneshot, watch};

use crate::error::ModbusError;
use crate::tcp::actor::{Actor, ControlCommand, RequestCommand, control_channel_capacity};
use crate::tcp::flow::ModbusTcpFlowControl;
use crate::tcp::retry::ModbusTcpRetry;
use crate::tcp::tcp_connection::{ModbusTcpConnection, actor_terminated_error};
use crate::tcp::timeouts::ModbusTcpTimeouts;

/// Configurable Modbus TCP connection phase.
///
/// A socket stores construction-time settings as plain configuration. Creating
/// or modifying a socket does not spawn the background actor, resolve `host`, or
/// open a TCP connection. Call [`ModbusTcpSocket::connect`] to validate the
/// configuration, spawn the actor, eagerly open the first TCP connection, and
/// receive a live [`ModbusTcpConnection`].
#[derive(Clone, Debug)]
pub struct ModbusTcpSocket {
    host: String,
    port: u16,
    unit_id: u8,
    transaction_id: u16,
    timeouts: ModbusTcpTimeouts,
    flow_control: ModbusTcpFlowControl,
    retry: Option<ModbusTcpRetry>,
}

impl ModbusTcpSocket {
    /// Creates a configurable socket with default construction settings.
    ///
    /// `host` may be a DNS name or numeric IP address and is not resolved until
    /// [`Self::connect`] is awaited. `port` is the TCP port used for every actor
    /// dial, and `unit_id` is the default Modbus unit id used by the live
    /// connection. The initial transaction-id seed defaults to `1`; timeouts,
    /// flow control, and retry use their documented defaults.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16, unit_id: u8) -> Self {
        Self {
            host: host.into(),
            port,
            unit_id,
            transaction_id: 1,
            timeouts: ModbusTcpTimeouts::default(),
            flow_control: ModbusTcpFlowControl::default(),
            retry: Some(ModbusTcpRetry::default()),
        }
    }

    /// Overrides the connect, write, and response timeouts used by the actor.
    ///
    /// `timeouts` is stored on the configurable socket and handed to the actor
    /// when [`Self::connect`] is awaited. Returns the updated socket.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: ModbusTcpTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Overrides the actor flow-control settings.
    ///
    /// `flow_control` configures queue depth, in-flight request capacity, queue
    /// timeout, and quarantine lifetime. It is validated when [`Self::connect`]
    /// is awaited. Returns the updated socket.
    #[must_use]
    pub fn with_flow_control(mut self, flow_control: ModbusTcpFlowControl) -> Self {
        self.flow_control = flow_control;
        self
    }

    /// Overrides the same-call retry policy used by the live connection.
    ///
    /// `retry` is copied into the live [`ModbusTcpConnection`]. Pass `None` to
    /// disable retry for sends through that handle. Returns the updated socket.
    #[must_use]
    pub fn with_retry(mut self, retry: Option<ModbusTcpRetry>) -> Self {
        self.retry = retry;
        self
    }

    /// Overrides the MBAP transaction-id counter seed.
    ///
    /// `transaction_id` is the id used by the first request after connection,
    /// with subsequent requests incrementing from it with wrap-around. Returns
    /// the updated socket.
    #[must_use]
    pub fn with_initial_transaction_id(mut self, transaction_id: u16) -> Self {
        self.transaction_id = transaction_id;
        self
    }

    /// Spawns the actor, eagerly opens the first TCP connection, and returns a live handle.
    ///
    /// Flow control is validated before channels are allocated. On success, the
    /// actor is spawned and the existing actor control path is used to perform
    /// an eager warm-up connect before the live [`ModbusTcpConnection`] is
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] if flow-control settings are
    /// invalid. Returns [`ModbusError::ConnectError`] or
    /// [`ModbusError::ConnectTimeout`] if the first TCP connection cannot be
    /// opened, or a transport error if the actor terminates before acknowledging
    /// the warm-up command.
    pub async fn connect(self) -> Result<ModbusTcpConnection, ModbusError> {
        self.flow_control.validate()?;
        let connection = self.spawn_actor();
        let (ack, reply) = oneshot::channel();
        connection
            .ctrl_tx
            .send(ControlCommand::Connect { ack })
            .await
            .map_err(|_| actor_terminated_error())?;
        reply.await.map_err(|_| actor_terminated_error())??;
        Ok(connection)
    }

    pub(crate) fn spawn_actor(self) -> ModbusTcpConnection {
        let (req_tx, req_rx) = mpsc::channel::<RequestCommand>(self.flow_control.max_queue_depth);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(control_channel_capacity());
        let (connected_tx, connected) = watch::channel(false);
        let actor = Actor::new(
            self.host,
            self.port,
            self.timeouts,
            self.flow_control,
            self.transaction_id,
            ctrl_rx,
            req_rx,
            connected_tx,
        );
        tokio::spawn(actor.run());
        ModbusTcpConnection::from_actor_parts(
            req_tx,
            ctrl_tx,
            self.unit_id,
            connected,
            self.flow_control.queue_timeout,
            self.retry,
        )
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn new_stores_defaults_without_spawning() {
        let socket = ModbusTcpSocket::new("localhost", 502, 7);

        assert_eq!(socket.host, "localhost");
        assert_eq!(socket.port, 502);
        assert_eq!(socket.unit_id, 7);
        assert_eq!(socket.transaction_id, 1);
        assert_eq!(socket.timeouts, ModbusTcpTimeouts::default());
        assert_eq!(socket.flow_control, ModbusTcpFlowControl::default());
        assert_eq!(socket.retry, Some(ModbusTcpRetry::default()));
    }

    #[test]
    fn modifiers_store_custom_settings() {
        let timeouts = ModbusTcpTimeouts {
            connect_timeout: Duration::from_millis(11),
            write_timeout: Duration::from_millis(12),
            response_timeout: Duration::from_millis(13),
        };
        let flow_control = ModbusTcpFlowControl {
            max_in_flight: 2,
            max_queue_depth: 3,
            queue_timeout: Duration::from_millis(14),
            quarantine_ttl: Duration::from_millis(15),
        };
        let retry = ModbusTcpRetry {
            initial_delay: Duration::from_millis(1),
            max_delay: Some(Duration::from_millis(2)),
            multiplier: 1.5,
            max_elapsed: Duration::from_millis(16),
            max_times: Some(2),
            jitter: false,
            retry_gateway_busy: false,
        };

        let socket = ModbusTcpSocket::new("127.0.0.1", 502, 1)
            .with_timeouts(timeouts)
            .with_flow_control(flow_control)
            .with_retry(Some(retry))
            .with_initial_transaction_id(42);

        assert_eq!(socket.timeouts, timeouts);
        assert_eq!(socket.flow_control, flow_control);
        assert_eq!(socket.retry, Some(retry));
        assert_eq!(socket.transaction_id, 42);

        let socket = socket.with_retry(None);
        assert_eq!(socket.retry, None);
    }

    #[tokio::test]
    async fn connect_success_returns_connected_handle() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let connection = ModbusTcpSocket::new(addr.ip().to_string(), addr.port(), 1)
            .connect()
            .await
            .unwrap();

        assert!(connection.is_connected().await);
    }

    #[tokio::test]
    async fn connect_validates_flow_control() {
        let flow_control = ModbusTcpFlowControl {
            max_in_flight: 0,
            ..ModbusTcpFlowControl::default()
        };

        let result = ModbusTcpSocket::new("127.0.0.1", 502, 1)
            .with_flow_control(flow_control)
            .connect()
            .await;

        assert!(matches!(result, Err(ModbusError::ValidationError(_))));
    }
}
