// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Single-owner Modbus TCP connection actor.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::codec::Framed;
use tracing::{debug, warn};

use crate::tcp::adu::build_modbus_tcp_adu;
use crate::tcp::codec::{MbapCodec, MbapCodecError};
use crate::tcp::tcp_connection::ModbusTcpConnection;
use crate::tcp::timeouts::ModbusTcpTimeouts;
use crate::{ModbusRequest, error::ModbusError};

/// Bounded command capacity used to apply backpressure under burst load.
pub(crate) const COMMAND_CHANNEL_CAPACITY: usize = 128;
/// Maximum number of transaction-id candidates to inspect before reporting exhaustion.
const MAX_TID_PROBES: usize = u16::MAX as usize + 1;

/// Command sent from connection handles to the owning actor.
#[derive(Debug)]
pub(crate) enum Command {
    /// Serialize and write a request, then route the response to `reply`.
    Request {
        /// Unit identifier to place in the MBAP header.
        unit_id: u8,
        /// Typed Modbus request PDU.
        pdu: ModbusRequest,
        /// Response channel receiving a raw ADU frame or transport error.
        reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
    },
    /// Eagerly connect and acknowledge the result.
    Connect {
        /// Acknowledgement channel completed after dialing.
        ack: oneshot::Sender<Result<(), ModbusError>>,
    },
    /// Drop the current socket and fail pending waiters.
    Disconnect {
        /// Acknowledgement channel completed after teardown is processed.
        ack: oneshot::Sender<()>,
    },
}

/// Pending response waiter and timeout metadata for one transaction id.
#[derive(Debug)]
struct PendingEntry {
    /// Response channel waiting for this transaction's frame or terminal error.
    reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
    /// Absolute time after which the response waiter receives a read timeout.
    deadline: Instant,
}

/// Actor that owns the TCP socket, transaction ids, and pending response map.
#[derive(Debug)]
pub(crate) struct Actor {
    /// Remote Modbus TCP server address.
    address: IpAddr,
    /// Remote Modbus TCP server port.
    port: u16,
    /// Connection, read, and write timeout configuration.
    timeouts: ModbusTcpTimeouts,
    /// Command receiver for handle requests and lifecycle operations.
    rx: mpsc::Receiver<Command>,
    /// Watch channel notifying handles whether a socket is currently open.
    connected_tx: watch::Sender<bool>,
    /// Framed socket owned exclusively by the actor when connected.
    framed: Option<Framed<TcpStream, MbapCodec>>,
    /// Response waiters keyed by Modbus transaction identifier.
    pending: HashMap<u16, PendingEntry>,
    /// Candidate transaction identifier used by the next request.
    next_tid: u16,
}

impl Actor {
    /// Creates an actor seeded with no open socket.
    ///
    /// # Arguments
    ///
    /// * `address` - Remote Modbus TCP server address.
    /// * `port` - Remote Modbus TCP server port.
    /// * `timeouts` - Timeout configuration for connect, write, and read operations.
    /// * `transaction_id` - Initial transaction identifier candidate.
    /// * `rx` - Command receiver owned by the actor.
    /// * `connected_tx` - Watch sender used to publish connection state.
    ///
    /// # Returns
    ///
    /// A disconnected actor ready to run its command loop.
    pub(crate) fn new(
        address: IpAddr,
        port: u16,
        timeouts: ModbusTcpTimeouts,
        transaction_id: u16,
        rx: mpsc::Receiver<Command>,
        connected_tx: watch::Sender<bool>,
    ) -> Self {
        Self {
            address,
            port,
            timeouts,
            rx,
            connected_tx,
            framed: None,
            pending: HashMap::new(),
            next_tid: transaction_id,
        }
    }

    /// Runs the actor until all command senders are dropped.
    ///
    /// The loop owns the socket, routes responses to pending waiters, handles request deadlines,
    /// and processes lifecycle commands serially.
    pub(crate) async fn run(mut self) {
        loop {
            if self.framed.is_none() {
                match self.rx.recv().await {
                    None => return,
                    Some(Command::Disconnect { ack }) => {
                        self.teardown(None);
                        let _ = ack.send(());
                    }
                    Some(Command::Connect { ack }) => {
                        let _ = ack.send(self.ensure_connected().await);
                    }
                    Some(Command::Request {
                        unit_id,
                        pdu,
                        reply,
                    }) => {
                        self.connect_then_request(unit_id, pdu, reply).await;
                    }
                }
            } else {
                let deadline = self.earliest_deadline();
                tokio::select! {
                    maybe_cmd = self.rx.recv() => match maybe_cmd {
                        None => return,
                        Some(Command::Disconnect { ack }) => {
                            self.teardown(None);
                            let _ = ack.send(());
                        }
                        Some(Command::Connect { ack }) => { let _ = ack.send(Ok(())); }
                        Some(Command::Request { unit_id, pdu, reply }) => self.handle_request(unit_id, pdu, reply).await,
                    },
                    item = Self::next_frame(self.framed.as_mut()) => match item {
                        Some(Ok(frame)) => self.route_frame(frame),
                        Some(Err(error)) => self.teardown(Some(read_error_from_stream(error))),
                        None => self.teardown(Some(ModbusError::ReadError(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed connection")))),
                    },
                    () = async {
                        if let Some(deadline) = deadline { sleep_until(deadline).await; }
                    }, if deadline.is_some() => self.handle_deadline(),
                }
            }
        }
    }

    /// Awaits the next decoded ADU frame from an optional framed socket.
    ///
    /// # Arguments
    ///
    /// * `framed` - Mutable framed socket reference when connected.
    ///
    /// # Returns
    ///
    /// The next decoded frame result, or `None` when no socket is available or the stream ends.
    async fn next_frame(
        framed: Option<&mut Framed<TcpStream, MbapCodec>>,
    ) -> Option<Result<Vec<u8>, MbapCodecError>> {
        match framed {
            Some(framed) => framed.next().await,
            None => None,
        }
    }

    /// Opens the TCP connection if the actor is currently disconnected.
    ///
    /// # Returns
    ///
    /// `Ok(())` when a socket is available, otherwise the connect error or timeout.
    async fn ensure_connected(&mut self) -> Result<(), ModbusError> {
        if self.framed.is_some() {
            return Ok(());
        }
        let stream = ModbusTcpConnection::connect_stream(
            self.address,
            self.port,
            self.timeouts.connect_timeout,
        )
        .await?;
        self.framed = Some(Framed::new(stream, MbapCodec));
        let _ = self.connected_tx.send(true);
        Ok(())
    }

    /// Ensures a connection exists before dispatching a request.
    ///
    /// # Arguments
    ///
    /// * `unit_id` - Unit identifier to encode in the MBAP header.
    /// * `pdu` - Request PDU to serialize and send.
    /// * `reply` - Response channel completed with a frame or transport error.
    async fn connect_then_request(
        &mut self,
        unit_id: u8,
        pdu: ModbusRequest,
        reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
    ) {
        if let Err(error) = self.ensure_connected().await {
            let _ = reply.send(Err(error));
            return;
        }
        self.handle_request(unit_id, pdu, reply).await;
    }

    /// Serializes, writes, and tracks a request on the current connection.
    ///
    /// # Arguments
    ///
    /// * `unit_id` - Unit identifier to encode in the MBAP header.
    /// * `pdu` - Request PDU to serialize and send.
    /// * `reply` - Response channel stored until the matching response arrives or times out.
    async fn handle_request(
        &mut self,
        unit_id: u8,
        pdu: ModbusRequest,
        reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
    ) {
        let tid = match self.allocate_tid() {
            Ok(tid) => tid,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let adu = match build_modbus_tcp_adu(tid, unit_id, &pdu) {
            Ok(adu) => adu,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        debug!(tid, "Modbus TCP Frame: {:02X?}", adu);
        let Some(framed) = self.framed.as_mut() else {
            let _ = reply.send(Err(ModbusError::ReadError(io::Error::new(
                io::ErrorKind::NotConnected,
                "connection not open",
            ))));
            return;
        };
        match timeout(self.timeouts.write_timeout, framed.send(adu)).await {
            Ok(Ok(())) => {
                self.pending.insert(
                    tid,
                    PendingEntry {
                        reply,
                        deadline: Instant::now() + self.timeouts.read_timeout,
                    },
                );
            }
            Ok(Err(error)) => {
                let _ = reply.send(Err(write_error_from_sink(error)));
                self.teardown(None);
            }
            Err(_) => {
                let _ = reply.send(Err(ModbusError::WriteTimeout));
                self.teardown(None);
            }
        }
    }

    /// Allocates an unused transaction identifier for a new request.
    ///
    /// # Returns
    ///
    /// An available transaction identifier, or `NoFreeTransactionId` when all identifiers are pending.
    fn allocate_tid(&mut self) -> Result<u16, ModbusError> {
        for _ in 0..MAX_TID_PROBES {
            let tid = self.next_tid;
            self.next_tid = self.next_tid.wrapping_add(1);
            if !self.pending.contains_key(&tid) {
                return Ok(tid);
            }
        }
        Err(ModbusError::NoFreeTransactionId)
    }

    /// Routes a decoded response frame to the matching pending waiter.
    ///
    /// # Arguments
    ///
    /// * `frame` - Complete Modbus TCP ADU whose first two bytes contain the transaction id.
    fn route_frame(&mut self, frame: Vec<u8>) {
        let tid = u16::from_be_bytes([frame[0], frame[1]]);
        if let Some(entry) = self.pending.remove(&tid) {
            let _ = entry.reply.send(Ok(frame));
        } else {
            warn!(tid, "ignoring response for unknown transaction id");
        }
    }

    /// Finds the nearest pending response deadline.
    ///
    /// # Returns
    ///
    /// The earliest deadline among pending requests, or `None` when no requests are pending.
    fn earliest_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|entry| entry.deadline).min()
    }

    /// Completes expired waiters with read timeouts and closes the socket.
    fn handle_deadline(&mut self) {
        let now = Instant::now();
        let expired: Vec<u16> = self
            .pending
            .iter()
            .filter_map(|(tid, entry)| (entry.deadline <= now).then_some(*tid))
            .collect();
        for tid in expired {
            if let Some(entry) = self.pending.remove(&tid) {
                let _ = entry.reply.send(Err(ModbusError::ReadTimeout));
            }
        }
        self.teardown(None);
    }

    /// Drops the socket, marks the actor disconnected, and fails pending waiters.
    ///
    /// # Arguments
    ///
    /// * `reason` - Error to clone for pending waiters, or a default connection-closed error.
    fn teardown(&mut self, reason: Option<ModbusError>) {
        self.framed = None;
        let _ = self.connected_tx.send(false);
        let error = reason.unwrap_or_else(default_teardown_error);
        for (_, entry) in self.pending.drain() {
            let _ = entry.reply.send(Err(clone_error_for_waiter(&error)));
        }
    }
}

/// Builds the fallback error used when teardown has no explicit reason.
///
/// # Returns
///
/// A connection-aborted read error describing the closed connection.
fn default_teardown_error() -> ModbusError {
    ModbusError::ReadError(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "connection closed",
    ))
}

/// Converts a sink failure into a write-side transport error.
///
/// # Arguments
///
/// * `error` - Error returned while sending through the framed sink.
///
/// # Returns
///
/// A `WriteError` preserving I/O details when possible.
fn write_error_from_sink(error: MbapCodecError) -> ModbusError {
    match error {
        MbapCodecError::Io(error) | MbapCodecError::Modbus(ModbusError::WriteError(error)) => {
            ModbusError::WriteError(error)
        }
        MbapCodecError::Modbus(other) => {
            ModbusError::WriteError(io::Error::other(other.to_string()))
        }
    }
}

/// Converts a stream failure into a read-side transport or protocol error.
///
/// # Arguments
///
/// * `error` - Error returned while receiving through the framed stream.
///
/// # Returns
///
/// A read error for socket I/O failures, or the original Modbus protocol error.
fn read_error_from_stream(error: MbapCodecError) -> ModbusError {
    match error {
        MbapCodecError::Io(error) => ModbusError::ReadError(error),
        MbapCodecError::Modbus(error) => error,
    }
}

/// Clones a transport error for delivery to an independent pending waiter.
///
/// # Arguments
///
/// * `error` - Source error that cannot be cloned directly because it may contain `io::Error`.
///
/// # Returns
///
/// A semantically equivalent `ModbusError` with copied I/O error information.
fn clone_error_for_waiter(error: &ModbusError) -> ModbusError {
    match error {
        ModbusError::ConnectError(error) => ModbusError::ConnectError(copy_io_error(error)),
        ModbusError::ConnectTimeout => ModbusError::ConnectTimeout,
        ModbusError::WriteError(error) => ModbusError::WriteError(copy_io_error(error)),
        ModbusError::WriteTimeout => ModbusError::WriteTimeout,
        ModbusError::ReadError(error) => ModbusError::ReadError(copy_io_error(error)),
        ModbusError::ReadTimeout => ModbusError::ReadTimeout,
        ModbusError::MalformedResponse(message) => ModbusError::MalformedResponse(message.clone()),
        ModbusError::DeserializationError(message) => {
            ModbusError::DeserializationError(message.clone())
        }
        ModbusError::ExceptionResponse { function, code } => ModbusError::ExceptionResponse {
            function: *function,
            code: *code,
        },
        ModbusError::TransactionIdMismatch { expected, actual } => {
            ModbusError::TransactionIdMismatch {
                expected: *expected,
                actual: *actual,
            }
        }
        ModbusError::ProtocolIdMismatch { actual } => {
            ModbusError::ProtocolIdMismatch { actual: *actual }
        }
        ModbusError::NoFreeTransactionId => ModbusError::NoFreeTransactionId,
        ModbusError::UnitIdMismatch { expected, actual } => ModbusError::UnitIdMismatch {
            expected: *expected,
            actual: *actual,
        },
        ModbusError::RequestResponseMismatch(message) => {
            ModbusError::RequestResponseMismatch(message.clone())
        }
        ModbusError::ValidationError(message) => ModbusError::ValidationError(message.clone()),
    }
}

/// Copies an `io::Error` kind and message into a new error value.
///
/// # Arguments
///
/// * `error` - I/O error to duplicate for another owner.
///
/// # Returns
///
/// A new `io::Error` with the same kind and string representation.
fn copy_io_error(error: &io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::Other => io::Error::other(error.to_string()),
        kind => io::Error::new(kind, error.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn actor_with_seed(seed: u16) -> Actor {
        let (_tx, rx) = mpsc::channel(1);
        let (connected_tx, _connected_rx) = watch::channel(false);
        Actor::new(
            "127.0.0.1".parse().unwrap(),
            502,
            ModbusTcpTimeouts::default(),
            seed,
            rx,
            connected_tx,
        )
    }

    fn pending_entry() -> (
        PendingEntry,
        oneshot::Receiver<Result<Vec<u8>, ModbusError>>,
    ) {
        let (reply, response) = oneshot::channel();
        (
            PendingEntry {
                reply,
                deadline: Instant::now() + Duration::from_secs(1),
            },
            response,
        )
    }

    #[test]
    fn allocate_tid_wraps_from_max_to_zero() {
        let mut actor = actor_with_seed(u16::MAX);

        assert_eq!(actor.allocate_tid().unwrap(), u16::MAX);
        assert_eq!(actor.allocate_tid().unwrap(), 0);
    }

    #[test]
    fn allocate_tid_reports_no_free_transaction_id_after_probe_limit() {
        let mut actor = actor_with_seed(0);

        for tid in 0..=u16::MAX {
            let (entry, _response) = pending_entry();
            actor.pending.insert(tid, entry);
        }

        assert!(matches!(
            actor.allocate_tid(),
            Err(ModbusError::NoFreeTransactionId)
        ));
    }

    #[tokio::test]
    async fn teardown_preserves_malformed_response_for_pending_waiters() {
        let mut actor = actor_with_seed(0);
        let (entry, response) = pending_entry();
        actor.pending.insert(1, entry);

        actor.teardown(Some(ModbusError::MalformedResponse(
            "invalid MBAP length".to_string(),
        )));

        let result = response.await.unwrap();
        assert!(matches!(
            result,
            Err(ModbusError::MalformedResponse(message)) if message == "invalid MBAP length"
        ));
    }

    #[test]
    fn sink_io_errors_are_classified_as_write_errors() {
        let error = write_error_from_sink(MbapCodecError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "broken pipe",
        )));

        assert!(matches!(
            error,
            ModbusError::WriteError(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }
}
