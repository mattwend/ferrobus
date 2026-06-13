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
use crate::tcp::flow::ModbusTcpFlowControl;
use crate::tcp::tcp_connection::ModbusTcpConnection;
use crate::tcp::timeouts::ModbusTcpTimeouts;
use crate::{ModbusRequest, error::ModbusError};

/// Default bounded request capacity used to apply backpressure under burst load.
pub(crate) const COMMAND_CHANNEL_CAPACITY: usize = 128;
const CONTROL_CHANNEL_CAPACITY: usize = 8;
/// Maximum number of transaction-id candidates to inspect before reporting exhaustion.
const MAX_TID_PROBES: usize = u16::MAX as usize + 1;

/// Lifecycle command sent from connection handles to the owning actor.
#[derive(Debug)]
pub(crate) enum ControlCommand {
    /// Eagerly connect and acknowledge the result.
    Connect { ack: oneshot::Sender<Result<(), ModbusError>> },
    /// Drop the current socket and fail pending waiters.
    Disconnect { ack: oneshot::Sender<()> },
}

/// Request command sent from connection handles to the owning actor.
#[derive(Debug)]
pub(crate) struct RequestCommand {
    pub(crate) unit_id: u8,
    pub(crate) pdu: ModbusRequest,
    pub(crate) reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
    /// Absolute queue deadline anchored when the handle submits the request.
    pub(crate) queue_deadline: Instant,
}

/// Small control-channel capacity used by connection handles.
pub(crate) const fn control_channel_capacity() -> usize { CONTROL_CHANNEL_CAPACITY }

/// Pending response waiter and timeout metadata for one transaction id.
#[derive(Debug)]
struct PendingEntry {
    reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
    /// Absolute response deadline anchored after the frame is written to the socket.
    response_deadline: Instant,
}

/// Actor that owns the TCP socket, transaction ids, and pending response map.
#[derive(Debug)]
pub(crate) struct Actor {
    address: IpAddr,
    port: u16,
    timeouts: ModbusTcpTimeouts,
    flow: ModbusTcpFlowControl,
    ctrl_rx: mpsc::Receiver<ControlCommand>,
    req_rx: mpsc::Receiver<RequestCommand>,
    connected_tx: watch::Sender<bool>,
    framed: Option<Framed<TcpStream, MbapCodec>>,
    pending: HashMap<u16, PendingEntry>,
    quarantined: HashMap<u16, Instant>,
    next_tid: u16,
    ctrl_closed: bool,
    req_closed: bool,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        address: IpAddr,
        port: u16,
        timeouts: ModbusTcpTimeouts,
        flow: ModbusTcpFlowControl,
        transaction_id: u16,
        ctrl_rx: mpsc::Receiver<ControlCommand>,
        req_rx: mpsc::Receiver<RequestCommand>,
        connected_tx: watch::Sender<bool>,
    ) -> Self {
        Self {
            address,
            port,
            timeouts,
            flow,
            ctrl_rx,
            req_rx,
            connected_tx,
            framed: None,
            pending: HashMap::new(),
            quarantined: HashMap::new(),
            next_tid: transaction_id,
            ctrl_closed: false,
            req_closed: false,
        }
    }

    /// Runs the actor until both command channels are closed and no requests are pending.
    pub(crate) async fn run(mut self) {
        loop {
            if self.ctrl_closed && self.req_closed && self.pending.is_empty() { return; }

            if let Some(tid) = self.cancelled_tid() {
                self.cancel_in_flight(tid);
                continue;
            }

            let next_wake = self.earliest_wake();
            let window_has_room = self.pending.len() < self.flow.max_in_flight && !self.req_closed;

            tokio::select! {
                ctrl = self.ctrl_rx.recv(), if !self.ctrl_closed => match ctrl {
                    None => self.ctrl_closed = true,
                    Some(ControlCommand::Connect { ack }) => {
                        let result = self.ensure_connected().await;
                        notify_waiter(ack, result);
                    }
                    Some(ControlCommand::Disconnect { ack }) => {
                        self.teardown(Some(default_teardown_error()));
                        notify_waiter(ack, ());
                    }
                },
                req = self.req_rx.recv(), if window_has_room => match req {
                    None => self.req_closed = true,
                    Some(cmd) => self.dispatch(cmd).await,
                },
                item = Self::next_frame(self.framed.as_mut()), if self.framed.is_some() => match item {
                    Some(Ok(frame)) => self.route_frame(frame),
                    Some(Err(error)) => self.teardown(Some(read_error_from_stream(error))),
                    None => self.teardown(Some(ModbusError::ReadError(Arc::new(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed connection"))))),
                },
                () = async { if let Some(deadline) = next_wake { sleep_until(deadline).await; } }, if next_wake.is_some() => self.handle_deadlines(),
            }
        }
    }

    async fn next_frame(
        framed: Option<&mut Framed<TcpStream, MbapCodec>>,
    ) -> Option<Result<Vec<u8>, MbapCodecError>> {
        match framed { Some(framed) => framed.next().await, None => None }
    }

    async fn ensure_connected(&mut self) -> Result<(), ModbusError> {
        if self.framed.is_some() { return Ok(()); }
        let stream = ModbusTcpConnection::connect_stream(self.address, self.port, self.timeouts.connect_timeout).await?;
        self.framed = Some(Framed::new(stream, MbapCodec));
        if self.connected_tx.send(true).is_err() { trace!("no connection-state subscribers to notify of connect"); }
        Ok(())
    }

    async fn dispatch(&mut self, cmd: RequestCommand) {
        if cmd.queue_deadline <= Instant::now() {
            notify_waiter(cmd.reply, Err(ModbusError::QueueTimeout));
            return;
        }
        if self.framed.is_none() {
            if let Err(error) = self.ensure_connected().await {
                notify_waiter(cmd.reply, Err(error.clone()));
                self.fail_backlog(&error);
                return;
            }
            if cmd.queue_deadline <= Instant::now() {
                notify_waiter(cmd.reply, Err(ModbusError::QueueTimeout));
                return;
            }
        }
        self.write_on_wire(cmd).await;
    }


    async fn write_on_wire(&mut self, cmd: RequestCommand) {
        let tid = match self.allocate_tid() {
            Ok(tid) => tid,
            Err(error) => { notify_waiter(cmd.reply, Err(error)); return; }
        };
        let adu = match build_modbus_tcp_adu(tid, cmd.unit_id, &cmd.pdu) {
            Ok(adu) => adu,
            Err(error) => { notify_waiter(cmd.reply, Err(error)); return; }
        };
        debug!(tid, "Modbus TCP Frame: {:02X?}", adu);
        let Some(framed) = self.framed.as_mut() else {
            warn!(tid, "request reached actor without an open connection");
            notify_waiter(cmd.reply, Err(ModbusError::ReadError(Arc::new(io::Error::new(io::ErrorKind::NotConnected, "connection not open")))));
            return;
        };
        match timeout(self.timeouts.write_timeout, framed.send(adu)).await {
            Ok(Ok(())) => {
                let response_deadline = Instant::now() + self.timeouts.response_timeout;
                self.pending.insert(tid, PendingEntry { reply: cmd.reply, response_deadline });
            }
            Ok(Err(error)) => {
                notify_waiter(cmd.reply, Err(write_error_from_sink(error)));
                self.teardown(Some(default_teardown_error()));
            }
            Err(_) => {
                notify_waiter(cmd.reply, Err(ModbusError::WriteTimeout));
                self.teardown(Some(default_teardown_error()));
            }
        }
    }

    /// Fails every request currently buffered in the command channel with `error`.
    ///
    /// A single `try_recv` pass is deliberate. A sender that was parked on a full
    /// channel at failure time deposits its request once this drain frees capacity, and
    /// the `run` loop re-dispatches it on the next iteration — where it is either failed
    /// by a fresh (still-failing) connect attempt or short-circuited by its elapsed
    /// queue deadline. Looping here until producers quiesce would block the actor from
    /// servicing control commands and pending-response deadlines, and could not fail
    /// parked senders deterministically anyway (their wake ordering is runtime-defined).
    fn fail_backlog(&mut self, error: &ModbusError) {
        while let Ok(cmd) = self.req_rx.try_recv() {
            notify_waiter(cmd.reply, Err(error.clone()));
        }
    }

    fn allocate_tid(&mut self) -> Result<u16, ModbusError> {
        self.reclaim_quarantine();
        for _ in 0..MAX_TID_PROBES {
            let tid = self.next_tid;
            self.next_tid = self.next_tid.wrapping_add(1);
            if !self.pending.contains_key(&tid) && !self.quarantined.contains_key(&tid) {
                return Ok(tid);
            }
        }
        Err(ModbusError::NoFreeTransactionId)
    }

    fn route_frame(&mut self, frame: Vec<u8>) {
        let tid = u16::from_be_bytes([frame[0], frame[1]]);
        if let Some(entry) = self.pending.remove(&tid) {
            notify_waiter(entry.reply, Ok(frame));
        } else if self.quarantined.remove(&tid).is_some() {
            debug!(tid, "discarding late response for quarantined transaction id");
        } else {
            warn!(tid, "ignoring response for unknown transaction id");
        }
    }

    fn earliest_wake(&self) -> Option<Instant> {
        let deadline = self.pending.values().map(|entry| entry.response_deadline)
            .chain(self.quarantined.values().copied())
            .min();
        let cancellation_poll = (!self.pending.is_empty()).then(|| Instant::now() + std::time::Duration::from_millis(10));
        deadline.into_iter().chain(cancellation_poll).min()
    }

    fn handle_deadlines(&mut self) {
        let now = Instant::now();
        let cancelled: Vec<u16> = self.pending.iter()
            .filter_map(|(tid, entry)| entry.reply.is_closed().then_some(*tid))
            .collect();
        for tid in cancelled {
            self.cancel_in_flight(tid);
        }
        let expired: Vec<u16> = self.pending.iter()
            .filter_map(|(tid, entry)| (entry.response_deadline <= now).then_some(*tid))
            .collect();
        for tid in expired {
            if let Some(entry) = self.pending.remove(&tid) {
                notify_waiter(entry.reply, Err(ModbusError::ReadTimeout));
                self.quarantined.insert(tid, now + self.flow.quarantine_ttl);
            }
        }
        self.reclaim_quarantine();
    }

    fn reclaim_quarantine(&mut self) {
        let now = Instant::now();
        self.quarantined.retain(|_, reclaim_after| *reclaim_after > now);
    }

    fn cancelled_tid(&self) -> Option<u16> {
        self.pending.iter().find_map(|(tid, entry)| entry.reply.is_closed().then_some(*tid))
    }

    fn cancel_in_flight(&mut self, tid: u16) {
        if self.pending.remove(&tid).is_some() {
            self.quarantined.insert(tid, Instant::now() + self.flow.quarantine_ttl);
        }
    }

    fn teardown(&mut self, reason: Option<ModbusError>) {
        self.framed = None;
        if self.connected_tx.send(false).is_err() { trace!("no connection-state subscribers to notify of teardown"); }
        let error = reason.unwrap_or_else(default_teardown_error);
        for (_, entry) in self.pending.drain() {
            notify_waiter(entry.reply, Err(error.clone()));
        }
    }
}

fn default_teardown_error() -> ModbusError {
    ModbusError::ReadError(Arc::new(io::Error::new(io::ErrorKind::ConnectionAborted, "connection closed")))
}

fn notify_waiter<T>(reply: oneshot::Sender<T>, value: T) {
    if reply.send(value).is_err() { trace!("waiter receiver dropped before delivery; discarding response"); }
}

fn write_error_from_sink(error: MbapCodecError) -> ModbusError {
    match error {
        MbapCodecError::Io(error) => ModbusError::WriteError(Arc::new(error)),
        MbapCodecError::Modbus(ModbusError::WriteError(error)) => ModbusError::WriteError(error),
        MbapCodecError::Modbus(other) => ModbusError::WriteError(Arc::new(io::Error::other(other.to_string()))),
    }
}

fn read_error_from_stream(error: MbapCodecError) -> ModbusError {
    match error { MbapCodecError::Io(error) => ModbusError::ReadError(Arc::new(error)), MbapCodecError::Modbus(error) => error }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn actor_with_seed(seed: u16) -> Actor {
        let (_ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let (_req_tx, req_rx) = mpsc::channel(1);
        let (connected_tx, _connected_rx) = watch::channel(false);
        Actor::new(
            "127.0.0.1".parse().unwrap(),
            502,
            ModbusTcpTimeouts::default(),
            ModbusTcpFlowControl::default(),
            seed,
            ctrl_rx,
            req_rx,
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
                response_deadline: Instant::now() + Duration::from_secs(1),
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

    #[test]
    fn allocate_tid_skips_quarantined_and_reclaims_aged_entries() {
        let mut actor = actor_with_seed(5);
        actor
            .quarantined
            .insert(5, Instant::now() + Duration::from_secs(1));

        assert_eq!(actor.allocate_tid().unwrap(), 6);

        actor.next_tid = 7;
        actor
            .quarantined
            .insert(7, Instant::now() - Duration::from_millis(1));

        assert_eq!(actor.allocate_tid().unwrap(), 7);
        assert!(!actor.quarantined.contains_key(&7));
    }

    #[test]
    fn route_frame_discards_late_quarantined_response() {
        let mut actor = actor_with_seed(0);
        actor
            .quarantined
            .insert(42, Instant::now() + Duration::from_secs(1));
        let frame =
            crate::tcp::adu::build_modbus_tcp_adu_from_pdu_bytes(42, 1, &[0x03, 0x00]).unwrap();

        actor.route_frame(frame);

        assert!(!actor.quarantined.contains_key(&42));
        assert!(actor.pending.is_empty());
    }

    #[tokio::test]
    async fn handle_deadlines_quarantines_timeout_without_tearing_down_socket() {
        let (client, _server) = loopback_stream_pair().await;
        let (mut actor, _connected_rx) = connected_actor_with_stream(client);
        let (expired_reply, expired_response) = oneshot::channel();
        actor.pending.insert(
            1,
            PendingEntry {
                reply: expired_reply,
                response_deadline: Instant::now() - Duration::from_millis(1),
            },
        );
        let (sibling, _sibling_response) = pending_entry();
        actor.pending.insert(2, sibling);

        actor.handle_deadlines();

        assert!(actor.framed.is_some());
        assert!(actor.pending.contains_key(&2));
        assert!(!actor.pending.contains_key(&1));
        assert!(actor.quarantined.contains_key(&1));
        assert!(matches!(
            expired_response.await.unwrap(),
            Err(ModbusError::ReadTimeout)
        ));
    }

    #[tokio::test]
    async fn next_cancelled_tid_frees_slot_and_quarantines_tid() {
        let mut actor = actor_with_seed(0);
        actor.flow = ModbusTcpFlowControl::serial_gateway();
        let (entry, response) = pending_entry();
        actor.pending.insert(10, entry);
        drop(response);

        let tid = tokio::time::timeout(
            Duration::from_secs(1),
            Actor::next_cancelled_tid(&mut actor.pending),
        )
        .await
        .unwrap();
        actor.cancel_in_flight(tid);

        assert_eq!(tid, 10);
        assert!(actor.pending.is_empty());
        assert!(actor.quarantined.contains_key(&10));
        assert!(actor.pending.len() < actor.flow.max_in_flight);
    }

    #[tokio::test]
    async fn fail_backlog_drains_queued_requests_after_connect_failure() {
        let (_ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let (req_tx, req_rx) = mpsc::channel(2);
        let (connected_tx, _connected_rx) = watch::channel(false);
        let mut actor = Actor::new(
            "127.0.0.1".parse().unwrap(),
            502,
            ModbusTcpTimeouts::default(),
            ModbusTcpFlowControl {
                max_queue_depth: 2,
                ..ModbusTcpFlowControl::default()
            },
            0,
            ctrl_rx,
            req_rx,
            connected_tx,
        );
        let (first_reply, first_response) = oneshot::channel();
        let (second_reply, second_response) = oneshot::channel();
        let error = ModbusError::ConnectTimeout;
        req_tx
            .send(RequestCommand {
                unit_id: 1,
                pdu: read_holding_registers(1),
                reply: first_reply,
                queue_deadline: Instant::now() + Duration::from_secs(1),
            })
            .await
            .unwrap();
        req_tx
            .send(RequestCommand {
                unit_id: 1,
                pdu: read_holding_registers(1),
                reply: second_reply,
                queue_deadline: Instant::now() + Duration::from_secs(1),
            })
            .await
            .unwrap();

        actor.fail_backlog(&error);

        assert!(matches!(
            first_response.await.unwrap(),
            Err(ModbusError::ConnectTimeout)
        ));
        assert!(matches!(
            second_response.await.unwrap(),
            Err(ModbusError::ConnectTimeout)
        ));
    }

    /// A sender parked on a full command channel at connect-failure time is still
    /// failed: the single-pass drain frees capacity, the parked send completes, and the
    /// `run` loop re-dispatches it into another (still-failing) connect attempt. This
    /// exercises the path through `run` rather than `fail_backlog` in isolation, because
    /// the re-dispatch — not the drain — is what covers the straggler.
    #[tokio::test]
    async fn parked_sender_is_failed_via_redispatch_after_connect_failure() {
        // Reserve a port and drop the listener so connects are refused promptly.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);

        let (ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let (req_tx, req_rx) = mpsc::channel(2);
        let (connected_tx, _connected_rx) = watch::channel(false);
        let actor = Actor::new(
            dead_addr.ip(),
            dead_addr.port(),
            ModbusTcpTimeouts {
                connect_timeout: Duration::from_millis(200),
                ..ModbusTcpTimeouts::default()
            },
            ModbusTcpFlowControl {
                max_queue_depth: 2,
                ..ModbusTcpFlowControl::default()
            },
            0,
            ctrl_rx,
            req_rx,
            connected_tx,
        );
        let run = tokio::spawn(actor.run());

        let mut responses = Vec::new();
        // Two requests fill the depth-2 channel; the third parks until capacity frees.
        for _ in 0..3 {
            let (reply, response) = oneshot::channel();
            req_tx
                .send(RequestCommand {
                    unit_id: 1,
                    pdu: read_holding_registers(1),
                    reply,
                    queue_deadline: Instant::now() + Duration::from_secs(5),
                })
                .await
                .unwrap();
            responses.push(response);
        }
        drop(req_tx);
        drop(ctrl_tx);

        for response in responses {
            assert!(response.await.unwrap().is_err());
        }
        run.await.unwrap();
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

    fn request_command(
        quantity: u16,
        reply: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
        queue_deadline: Instant,
    ) -> RequestCommand {
        RequestCommand {
            unit_id: 1,
            pdu: read_holding_registers(quantity),
            reply,
            queue_deadline,
        }
    }

    /// A request whose deadline has already passed must short-circuit with a queue
    /// timeout instead of allocating a transaction id or touching the socket.
    #[tokio::test]
    async fn dispatch_with_expired_deadline_replies_queue_timeout() {
        let mut actor = actor_with_seed(0);
        let (reply, response) = oneshot::channel();

        actor
            .dispatch(request_command(1, reply, Instant::now()))
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::QueueTimeout)
        ));
        assert!(actor.pending.is_empty());
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
        let (_ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let (_req_tx, req_rx) = mpsc::channel(1);
        let (connected_tx, connected_rx) = watch::channel(false);
        drop(connected_rx);
        let mut actor = Actor::new(
            addr.ip(),
            addr.port(),
            ModbusTcpTimeouts::default(),
            ModbusTcpFlowControl::default(),
            0,
            ctrl_rx,
            req_rx,
            connected_tx,
        );

        actor.ensure_connected().await.unwrap();

        assert!(actor.framed.is_some());
    }

    /// When every transaction id is already pending, a new request must be rejected
    /// with `NoFreeTransactionId` rather than overwriting an in-flight waiter.
    #[tokio::test]
    async fn dispatch_without_free_tid_replies_no_free_transaction_id() {
        let (client, _server) = loopback_stream_pair().await;
        let (mut actor, _connected_rx) = connected_actor_with_stream(client);
        for tid in 0..=u16::MAX {
            let (entry, _response) = pending_entry();
            actor.pending.insert(tid, entry);
        }
        let (reply, response) = oneshot::channel();

        actor
            .dispatch(request_command(
                1,
                reply,
                Instant::now() + Duration::from_secs(1),
            ))
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::NoFreeTransactionId)
        ));
    }

    /// A request that fails PDU validation must surface the validation error to the
    /// caller without consuming the allocated transaction id permanently.
    #[tokio::test]
    async fn dispatch_with_invalid_pdu_replies_validation_error() {
        let (client, _server) = loopback_stream_pair().await;
        let (mut actor, _connected_rx) = connected_actor_with_stream(client);
        let (reply, response) = oneshot::channel();

        actor
            .dispatch(request_command(
                0,
                reply,
                Instant::now() + Duration::from_secs(1),
            ))
            .await;

        assert!(matches!(
            response.await.unwrap(),
            Err(ModbusError::ValidationError(_))
        ));
        assert!(actor.pending.is_empty());
    }

    /// The defensive guard for a request that reaches `write_on_wire` without an
    /// open socket must reply with a `NotConnected` read error.
    #[tokio::test]
    async fn write_on_wire_without_socket_replies_not_connected() {
        let mut actor = actor_with_seed(0);
        let (reply, response) = oneshot::channel();

        actor
            .write_on_wire(request_command(
                1,
                reply,
                Instant::now() + Duration::from_secs(1),
            ))
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
        let (_ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let (_req_tx, req_rx) = mpsc::channel(1);
        let (connected_tx, connected_rx) = watch::channel(true);
        let mut actor = Actor::new(
            "127.0.0.1".parse().unwrap(),
            502,
            ModbusTcpTimeouts {
                connect_timeout: Duration::from_secs(1),
                write_timeout: Duration::from_millis(10),
                response_timeout: Duration::from_secs(1),
            },
            ModbusTcpFlowControl::default(),
            0,
            ctrl_rx,
            req_rx,
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
            .dispatch(request_command(
                1,
                reply,
                Instant::now() + Duration::from_secs(1),
            ))
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
    async fn dispatch_write_error_replies_and_tears_down() {
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
            .dispatch(request_command(
                1,
                reply,
                Instant::now() + Duration::from_secs(1),
            ))
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
    async fn dispatch_write_timeout_replies_and_tears_down() {
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
            .dispatch(request_command(
                1,
                reply,
                Instant::now() + Duration::from_secs(1),
            ))
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
