// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Single-owner Modbus TCP connection actor.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::codec::Framed;
use tracing::{debug, trace, warn};

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
        /// Absolute request deadline anchored when the handle submits the command.
        deadline: Instant,
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
    /// Absolute request deadline anchored when the handle submits the command.
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
                        notify_waiter(ack, ());
                    }
                    Some(Command::Connect { ack }) => {
                        notify_waiter(ack, self.ensure_connected().await);
                    }
                    Some(Command::Request {
                        unit_id,
                        pdu,
                        reply,
                        deadline,
                    }) => {
                        self.connect_then_request(unit_id, pdu, reply, deadline)
                            .await;
                    }
                }
            } else {
                let deadline = self.earliest_deadline();
                tokio::select! {
                    maybe_cmd = self.rx.recv() => match maybe_cmd {
                        None => return,
                        Some(Command::Disconnect { ack }) => {
                            self.teardown(None);
                            notify_waiter(ack, ());
                        }
                        Some(Command::Connect { ack }) => { notify_waiter(ack, Ok(())); }
                        Some(Command::Request { unit_id, pdu, reply, deadline }) => self.handle_request(unit_id, pdu, reply, deadline).await,
                    },
                    item = Self::next_frame(self.framed.as_mut()) => match item {
                        Some(Ok(frame)) => self.route_frame(frame),
                        Some(Err(error)) => self.teardown(Some(read_error_from_stream(error))),
                        None => self.teardown(Some(ModbusError::ReadError(Arc::new(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed connection"))))),
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
            // defensive: connected-state callers short-circuit before reaching this path.
            return Ok(());
        }
        let stream = ModbusTcpConnection::connect_stream(
            self.address,
            self.port,
            self.timeouts.connect_timeout,
        )
        .await?;
        self.framed = Some(Framed::new(stream, MbapCodec));
        if self.connected_tx.send(true).is_err() {
            trace!("no connection-state subscribers to notify of connect");
        }
        Ok(())
    }

    /// Ensures a connection exists before dispatching a request.
    ///
    /// # Arguments
    ///
    /// * `unit_id` - Unit identifier to encode in the MBAP header.
    /// * `pdu` - Request PDU to serialize and send.
    /// * `reply` - Response channel completed with a frame or transport error.
    /// * `deadline` - Absolute request deadline anchored at command submission.
    async fn connect_then_request(
        &mut self,
        unit_id: u8,
        pdu: ModbusRequest,
        reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
        deadline: Instant,
    ) {
        if deadline <= Instant::now() {
            notify_waiter(reply, Err(ModbusError::ReadTimeout));
            return;
        }
        if let Err(error) = self.ensure_connected().await {
            notify_waiter(reply, Err(error));
            return;
        }
        self.handle_request(unit_id, pdu, reply, deadline).await;
    }

    /// Serializes, writes, and tracks a request on the current connection.
    ///
    /// # Arguments
    ///
    /// * `unit_id` - Unit identifier to encode in the MBAP header.
    /// * `pdu` - Request PDU to serialize and send.
    /// * `reply` - Response channel stored until the matching response arrives or times out.
    /// * `deadline` - Absolute request deadline anchored at command submission.
    async fn handle_request(
        &mut self,
        unit_id: u8,
        pdu: ModbusRequest,
        reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
        deadline: Instant,
    ) {
        if deadline <= Instant::now() {
            notify_waiter(reply, Err(ModbusError::ReadTimeout));
            return;
        }
        let tid = match self.allocate_tid() {
            Ok(tid) => tid,
            Err(error) => {
                notify_waiter(reply, Err(error));
                return;
            }
        };
        let adu = match build_modbus_tcp_adu(tid, unit_id, &pdu) {
            Ok(adu) => adu,
            Err(error) => {
                notify_waiter(reply, Err(error));
                return;
            }
        };
        debug!(tid, "Modbus TCP Frame: {:02X?}", adu);
        let Some(framed) = self.framed.as_mut() else {
            warn!(tid, "request reached actor without an open connection");
            notify_waiter(
                reply,
                Err(ModbusError::ReadError(Arc::new(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "connection not open",
                )))),
            );
            return;
        };
        match timeout(self.timeouts.write_timeout, framed.send(adu)).await {
            Ok(Ok(())) => {
                self.pending.insert(tid, PendingEntry { reply, deadline });
            }
            Ok(Err(error)) => {
                notify_waiter(reply, Err(write_error_from_sink(error)));
                self.teardown(None);
            }
            Err(_) => {
                notify_waiter(reply, Err(ModbusError::WriteTimeout));
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
        // The MBAP codec only yields complete frames with a full header and at least one PDU byte.
        let tid = u16::from_be_bytes([frame[0], frame[1]]);
        if let Some(entry) = self.pending.remove(&tid) {
            notify_waiter(entry.reply, Ok(frame));
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
    ///
    /// Closing the socket is intentionally connection-wide: a timed-out request may still
    /// leave an unread response on the stream, so remaining waiters receive a transient
    /// connection-aborted error and can retry on a fresh connection.
    fn handle_deadline(&mut self) {
        let now = Instant::now();
        let expired: Vec<u16> = self
            .pending
            .iter()
            .filter_map(|(tid, entry)| (entry.deadline <= now).then_some(*tid))
            .collect();
        for tid in expired {
            if let Some(entry) = self.pending.remove(&tid) {
                notify_waiter(entry.reply, Err(ModbusError::ReadTimeout));
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
        if self.connected_tx.send(false).is_err() {
            trace!("no connection-state subscribers to notify of teardown");
        }
        let error = reason.unwrap_or_else(default_teardown_error);
        for (_, entry) in self.pending.drain() {
            notify_waiter(entry.reply, Err(error.clone()));
        }
    }
}

/// Builds the fallback error used when teardown has no explicit reason.
///
/// # Returns
///
/// A connection-aborted read error describing the closed connection.
fn default_teardown_error() -> ModbusError {
    ModbusError::ReadError(Arc::new(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "connection closed",
    )))
}

/// Delivers a value to a oneshot waiter, tracing when the receiver has already been dropped.
///
/// A dropped receiver is the expected outcome when the originating caller timed out or was
/// cancelled before the response arrived; there is no error to propagate, so the value is
/// discarded and the occurrence is traced.
///
/// # Arguments
///
/// * `reply` - Oneshot sender whose receiver may have been dropped by a cancelled caller.
/// * `value` - Value to deliver to the waiter.
fn notify_waiter<T>(reply: oneshot::Sender<T>, value: T) {
    if reply.send(value).is_err() {
        trace!("waiter receiver dropped before delivery; discarding response");
    }
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
        MbapCodecError::Io(error) => ModbusError::WriteError(Arc::new(error)),
        MbapCodecError::Modbus(ModbusError::WriteError(error)) => ModbusError::WriteError(error),
        MbapCodecError::Modbus(other) => {
            ModbusError::WriteError(Arc::new(io::Error::other(other.to_string())))
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
        MbapCodecError::Io(error) => ModbusError::ReadError(Arc::new(error)),
        MbapCodecError::Modbus(error) => error,
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    fn read_holding_registers(quantity: u16) -> ModbusRequest {
        ModbusRequest::ReadHoldingRegisters {
            starting_address: 0,
            quantity,
        }
    }

    /// A request whose deadline has already passed must short-circuit with a read
    /// timeout instead of allocating a transaction id or touching the socket.
    #[tokio::test]
    async fn handle_request_with_expired_deadline_replies_read_timeout() {
        let mut actor = actor_with_seed(0);
        let (reply, response) = oneshot::channel();

        actor
            .handle_request(1, read_holding_registers(1), reply, Instant::now())
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::ReadTimeout)
        ));
        assert!(actor.pending.is_empty());
    }

    /// The lazy-connect path must also honor an already-expired deadline before it
    /// attempts to dial the server.
    #[tokio::test]
    async fn connect_then_request_with_expired_deadline_replies_read_timeout() {
        let mut actor = actor_with_seed(0);
        let (reply, response) = oneshot::channel();

        actor
            .connect_then_request(1, read_holding_registers(1), reply, Instant::now())
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::ReadTimeout)
        ));
        assert!(actor.framed.is_none());
    }

    /// Calling `ensure_connected` when a framed socket is already present must be a no-op.
    #[tokio::test]
    async fn ensure_connected_with_existing_socket_is_ok() {
        let (client, _server) = loopback_stream_pair().await;
        let (mut actor, _connected_rx) = connected_actor_with_stream(client);

        // The actor address points at an unbound port, so reaching the dial path
        // would fail the unwrap; succeeding proves the early-return guard was taken.
        actor.ensure_connected().await.unwrap();
    }

    /// Connecting with no watch receivers must still succeed and trace the missing subscriber.
    #[tokio::test]
    async fn ensure_connected_without_state_subscribers_succeeds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let (_tx, rx) = mpsc::channel(1);
        let (connected_tx, connected_rx) = watch::channel(false);
        drop(connected_rx);
        let mut actor = Actor::new(
            addr.ip(),
            addr.port(),
            ModbusTcpTimeouts::default(),
            0,
            rx,
            connected_tx,
        );

        actor.ensure_connected().await.unwrap();

        assert!(actor.framed.is_some());
    }

    /// When every transaction id is already pending, a new request must be rejected
    /// with `NoFreeTransactionId` rather than overwriting an in-flight waiter.
    #[tokio::test]
    async fn handle_request_without_free_tid_replies_no_free_transaction_id() {
        let mut actor = actor_with_seed(0);
        for tid in 0..=u16::MAX {
            let (entry, _response) = pending_entry();
            actor.pending.insert(tid, entry);
        }
        let (reply, response) = oneshot::channel();

        actor
            .handle_request(
                1,
                read_holding_registers(1),
                reply,
                Instant::now() + Duration::from_secs(1),
            )
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::NoFreeTransactionId)
        ));
    }

    /// A request that fails PDU validation must surface the validation error to the
    /// caller without consuming the allocated transaction id permanently.
    #[tokio::test]
    async fn handle_request_with_invalid_pdu_replies_validation_error() {
        let mut actor = actor_with_seed(0);
        let (reply, response) = oneshot::channel();

        actor
            .handle_request(
                1,
                read_holding_registers(0),
                reply,
                Instant::now() + Duration::from_secs(1),
            )
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::ValidationError(_))
        ));
        assert!(actor.pending.is_empty());
    }

    /// The defensive guard for a request that reaches `handle_request` without an
    /// open socket must reply with a `NotConnected` read error.
    #[tokio::test]
    async fn handle_request_without_socket_replies_not_connected() {
        let mut actor = actor_with_seed(0);
        let (reply, response) = oneshot::channel();

        actor
            .handle_request(
                1,
                read_holding_registers(1),
                reply,
                Instant::now() + Duration::from_secs(1),
            )
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::ReadError(error)) if error.kind() == io::ErrorKind::NotConnected
        ));
    }

    /// Without a socket, `next_frame` must yield `None` so the select arm stays idle.
    #[tokio::test]
    async fn next_frame_without_socket_yields_none() {
        assert!(Actor::next_frame(None).await.is_none());
    }

    /// A Modbus write error carried by the codec must be preserved as a write error.
    #[test]
    fn sink_modbus_write_error_is_preserved_as_write_error() {
        let error = write_error_from_sink(MbapCodecError::Modbus(ModbusError::WriteError(
            Arc::new(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe")),
        )));

        assert!(matches!(
            error,
            ModbusError::WriteError(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }

    /// A non-write Modbus error from the codec sink must be wrapped as a write error.
    #[test]
    fn sink_other_modbus_error_is_wrapped_as_write_error() {
        let error = write_error_from_sink(MbapCodecError::Modbus(ModbusError::ValidationError(
            "bad frame".to_string(),
        )));

        assert!(matches!(error, ModbusError::WriteError(_)));
    }

    fn connected_actor_with_stream(stream: TcpStream) -> (Actor, watch::Receiver<bool>) {
        let (_tx, rx) = mpsc::channel(1);
        let (connected_tx, connected_rx) = watch::channel(true);
        let mut actor = Actor::new(
            "127.0.0.1".parse().unwrap(),
            502,
            ModbusTcpTimeouts {
                connect_timeout: Duration::from_secs(1),
                write_timeout: Duration::from_millis(10),
                read_timeout: Duration::from_secs(1),
            },
            0,
            rx,
            connected_tx,
        );
        actor.framed = Some(Framed::new(stream, MbapCodec));
        (actor, connected_rx)
    }

    async fn loopback_stream_pair() -> (TcpStream, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    async fn spawn_one_response_server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0u8; 7];
            stream.read_exact(&mut header).await.unwrap();
            let tid = u16::from_be_bytes([header[0], header[1]]);
            let unit_id = header[6];
            let pdu_len = usize::from(u16::from_be_bytes([header[4], header[5]])) - 1;
            let mut pdu = vec![0u8; pdu_len];
            stream.read_exact(&mut pdu).await.unwrap();
            let response_pdu = [0x03, 0x02, 0x00, 0x01];
            let response =
                crate::tcp::adu::build_modbus_tcp_adu_from_pdu_bytes(tid, unit_id, &response_pdu)
                    .unwrap();
            stream.write_all(&response).await.unwrap();
        });
        addr
    }

    async fn assert_next_request_reconnects(actor: &mut Actor) {
        let reconnect_addr = spawn_one_response_server().await;
        actor.address = reconnect_addr.ip();
        actor.port = reconnect_addr.port();
        let (reply, response) = oneshot::channel();

        actor
            .connect_then_request(
                1,
                read_holding_registers(1),
                reply,
                Instant::now() + Duration::from_secs(1),
            )
            .await;
        let frame = Actor::next_frame(actor.framed.as_mut())
            .await
            .unwrap()
            .unwrap();
        actor.route_frame(frame);

        assert!(response.await.unwrap().is_ok());
        assert!(actor.framed.is_some());
    }

    /// A sink I/O error must be reported to the current waiter and tear down the socket.
    ///
    /// This relies on the local OS honoring `SO_LINGER(0)` as an immediate reset for a
    /// loopback socket pair before the client write below.
    #[tokio::test]
    async fn handle_request_write_error_replies_and_tears_down() {
        let (client, server) = loopback_stream_pair().await;
        let server = server.into_std().unwrap();
        socket2::SockRef::from(&server)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        drop(server);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (mut actor, connected_rx) = connected_actor_with_stream(client);
        let (pending, pending_response) = pending_entry();
        actor.pending.insert(99, pending);
        let (reply, response) = oneshot::channel();

        actor
            .handle_request(
                1,
                read_holding_registers(1),
                reply,
                Instant::now() + Duration::from_secs(1),
            )
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::WriteError(_))
        ));
        assert!(matches!(
            pending_response.await.unwrap(),
            Err(ModbusError::ReadError(error)) if error.kind() == io::ErrorKind::ConnectionAborted
        ));
        assert!(actor.pending.is_empty());
        assert!(actor.framed.is_none());
        assert!(!*connected_rx.borrow());

        assert_next_request_reconnects(&mut actor).await;
    }

    /// A send that remains blocked past the write timeout must fail and close the socket.
    ///
    /// This assumes the local OS eventually reports `WouldBlock` after the test fills a
    /// loopback socket send buffer while the peer remains open and unread.
    #[tokio::test]
    async fn handle_request_write_timeout_replies_and_tears_down() {
        let (client, _server) = loopback_stream_pair().await;
        let filler = vec![0xA5; 64 * 1024];
        loop {
            match client.try_write(&filler) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("failed to fill client send buffer: {error}"),
            }
        }
        let (mut actor, connected_rx) = connected_actor_with_stream(client);
        let (pending, pending_response) = pending_entry();
        actor.pending.insert(99, pending);
        let (reply, response) = oneshot::channel();

        actor
            .handle_request(
                1,
                read_holding_registers(1),
                reply,
                Instant::now() + Duration::from_secs(1),
            )
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::WriteTimeout)
        ));
        assert!(matches!(
            pending_response.await.unwrap(),
            Err(ModbusError::ReadError(error)) if error.kind() == io::ErrorKind::ConnectionAborted
        ));
        assert!(actor.pending.is_empty());
        assert!(actor.framed.is_none());
        assert!(!*connected_rx.borrow());

        assert_next_request_reconnects(&mut actor).await;
    }
}
