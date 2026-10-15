// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Bounded, priority-ordered request queue between connection handles and the actor.
//!
//! The queue has one FIFO lane per [`Priority`] level. A fair per-lane admission
//! mutex serializes senders through enqueue, so new arrivals cannot overtake parked
//! senders of the same level. Higher-priority waiters get capacity first.
//!
//! The lanes use unbounded Tokio channels and receiver-local buffers. A shared atomic
//! counter bounds their combined size by `max_queue_depth`; moving a request into a
//! receiver buffer does not release its slot. Statistics remain lock-free.
//!
//! Before selecting the highest non-empty lane, the receiver scans all buffered
//! requests for expiration. A deadline timer also runs when dispatch is disabled by
//! the in-flight limit, so neither urgent traffic nor a full window suppresses expiry.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::time::{Instant, Sleep, sleep_until, timeout_at};

use crate::error::ModbusError;
use crate::tcp::actor::RequestCommand;
use crate::tcp::tcp_connection::actor_terminated_error;

/// Number of priority levels, and therefore of queue lanes.
const LEVELS: usize = 3;
/// Weight of the newest sample in the queue-wait moving average, as `1 / EWMA_DIVISOR`.
const EWMA_DIVISOR: u128 = 8;

/// Scheduling priority of a request waiting in a connection's request queue.
///
/// The actor always dispatches the oldest request of the highest non-empty level, so
/// `Urgent` requests overtake `High` ones, which overtake `Normal` ones. Requests of
/// the same level keep their submission order. When the queue is full, a freed slot
/// goes to a parked sender of the highest waiting level.
///
/// Priority only reorders requests that have not been written to the socket yet. A
/// request already on the wire, or already queued inside a gateway, cannot be
/// overtaken, so priorities are most effective with a small
/// [`max_in_flight`](crate::tcp::ModbusTcpFlowControl::max_in_flight), such as
/// [`ModbusTcpFlowControl::serial_gateway`](crate::tcp::ModbusTcpFlowControl::serial_gateway).
///
/// Ordering is strict: as long as higher-priority requests keep arriving, lower ones
/// wait, bounded by their queue timeout. Priorities are a latency tool, not a safety
/// function: an emergency stop must not depend on Modbus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Regular traffic such as measurement polling. This is the default.
    #[default]
    Normal,
    /// Traffic that should overtake regular polling, such as control set-points.
    High,
    /// Traffic that should overtake everything else, such as a curtailment command.
    Urgent,
}

impl Priority {
    /// Lane index; lower indices are dequeued first.
    const fn lane(self) -> usize {
        match self {
            Self::Urgent => 0,
            Self::High => 1,
            Self::Normal => 2,
        }
    }
}

/// Point-in-time statistics of a connection's request queue.
///
/// Obtained from [`ModbusTcpConnection::queue_stats`](crate::tcp::ModbusTcpConnection::queue_stats).
/// Every field is read from its own atomic without a lock, so the snapshot is not
/// guaranteed to be mutually consistent: fields may come from slightly different
/// moments. Counters are monotonic; derive rates from two snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueStats {
    /// Maximum number of queued requests across all levels (`max_queue_depth`).
    pub capacity: usize,
    /// Requests currently queued across all levels.
    pub queued: usize,
    /// Statistics of the [`Priority::Normal`] level.
    pub normal: LevelStats,
    /// Statistics of the [`Priority::High`] level.
    pub high: LevelStats,
    /// Statistics of the [`Priority::Urgent`] level.
    pub urgent: LevelStats,
}

impl QueueStats {
    /// Returns the statistics of one priority level.
    #[must_use]
    pub fn level(&self, priority: Priority) -> &LevelStats {
        match priority {
            Priority::Normal => &self.normal,
            Priority::High => &self.high,
            Priority::Urgent => &self.urgent,
        }
    }
}

/// Statistics of one priority level of a connection's request queue.
///
/// Useful signals:
/// - a **growing** queue shows as `enqueued_total` rising faster than
///   `dequeued_total`, or as `waiting_for_admission > 0`;
/// - a **slowly progressing** queue shows as a rising `avg_queue_wait`;
/// - a **stalled** queue shows as `queued > 0` while `since_last_dequeue` keeps
///   growing, which the wait figures cannot show because they only update on dequeue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LevelStats {
    /// Requests admitted to the queue and not yet taken by the actor.
    pub queued: usize,
    /// Senders awaiting capacity or their FIFO admission turn.
    pub waiting_for_admission: usize,
    /// Requests admitted to the queue since the connection was created.
    pub enqueued_total: u64,
    /// Requests taken from the queue by the actor, whatever their outcome.
    pub dequeued_total: u64,
    /// Requests that failed with [`ModbusError::QueueTimeout`], either while
    /// waiting for admission or while queued.
    pub expired_total: u64,
    /// Queued requests discarded because their caller stopped waiting.
    pub abandoned_total: u64,
    /// Time the most recently dequeued request spent in the queue.
    pub last_queue_wait: Option<Duration>,
    /// Exponential moving average of the queue wait, weighting the newest sample 1/8.
    pub avg_queue_wait: Option<Duration>,
    /// Time since the actor last took a request of this level from the queue.
    pub since_last_dequeue: Option<Duration>,
}

/// Lock-free counters of one priority level.
///
/// Optional durations are stored in microseconds offset by one, so that zero encodes
/// "no sample yet" and `AtomicU64::default` is a valid initial state.
#[derive(Debug, Default)]
struct LevelCounters {
    queued: AtomicUsize,
    waiting: AtomicUsize,
    enqueued_total: AtomicU64,
    dequeued_total: AtomicU64,
    expired_total: AtomicU64,
    abandoned_total: AtomicU64,
    last_wait_us: AtomicU64,
    avg_wait_us: AtomicU64,
    last_dequeue_us: AtomicU64,
}

impl LevelCounters {
    fn snapshot(&self, epoch: Instant, now: Instant) -> LevelStats {
        LevelStats {
            queued: self.queued.load(Ordering::Relaxed),
            waiting_for_admission: self.waiting.load(Ordering::Relaxed),
            enqueued_total: self.enqueued_total.load(Ordering::Relaxed),
            dequeued_total: self.dequeued_total.load(Ordering::Relaxed),
            expired_total: self.expired_total.load(Ordering::Relaxed),
            abandoned_total: self.abandoned_total.load(Ordering::Relaxed),
            last_queue_wait: decode_micros(self.last_wait_us.load(Ordering::Relaxed)),
            avg_queue_wait: decode_micros(self.avg_wait_us.load(Ordering::Relaxed)),
            since_last_dequeue: decode_micros(self.last_dequeue_us.load(Ordering::Relaxed))
                .map(|at| now.saturating_duration_since(epoch + at)),
        }
    }
}

/// State shared by all senders and the receiver of one queue.
#[derive(Debug)]
struct Shared {
    capacity: usize,
    /// Admitted and not yet dequeued requests across all lanes; the capacity gate.
    queued: AtomicUsize,
    /// Set when the receiver is dropped, so parked senders stop waiting.
    closed: AtomicBool,
    /// Woken when a slot frees, a waiter leaves, or the queue closes.
    slot_freed: Notify,
    /// FIFO admission, held until the request has been enqueued in its lane.
    admission: [Mutex<()>; LEVELS],
    /// Reference point for the dequeue timestamps.
    epoch: Instant,
    levels: [LevelCounters; LEVELS],
}

impl Shared {
    /// Reserves one queue slot for `lane` if capacity and priority allow it.
    ///
    /// The caller holds its lane's admission mutex, so only higher levels can block it.
    fn try_admit(&self, lane: usize) -> bool {
        if self.levels[..lane]
            .iter()
            .any(|level| level.waiting.load(Ordering::SeqCst) > 0)
        {
            return false;
        }
        let mut current = self.queued.load(Ordering::SeqCst);
        loop {
            if current >= self.capacity {
                return false;
            }
            match self.queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Releases one queue slot and wakes parked senders if there are any.
    ///
    /// The slot is released before the waiter counters are read, and a parked sender
    /// registers itself before re-checking capacity; with sequentially consistent
    /// ordering on both sides, either the releaser sees the waiter and wakes it, or
    /// the waiter sees the freed slot.
    fn release_slot(&self) {
        self.queued.fetch_sub(1, Ordering::SeqCst);
        if self
            .levels
            .iter()
            .any(|level| level.waiting.load(Ordering::SeqCst) > 0)
        {
            self.slot_freed.notify_waiters();
        }
    }

    fn stats(&self) -> QueueStats {
        let now = Instant::now();
        let [urgent, high, normal] = &self.levels;
        QueueStats {
            capacity: self.capacity,
            queued: self.queued.load(Ordering::Relaxed),
            normal: normal.snapshot(self.epoch, now),
            high: high.snapshot(self.epoch, now),
            urgent: urgent.snapshot(self.epoch, now),
        }
    }
}

/// Registration of one sender awaiting admission in its level's waiter count.
///
/// Dropping it, on admission, timeout, or cancellation, wakes the remaining waiters:
/// lower levels may have been held back only by this waiter.
struct WaitingGuard<'a> {
    shared: &'a Shared,
    lane: usize,
}

impl<'a> WaitingGuard<'a> {
    fn register(shared: &'a Shared, lane: usize) -> Self {
        shared.levels[lane].waiting.fetch_add(1, Ordering::SeqCst);
        Self { shared, lane }
    }
}

impl Drop for WaitingGuard<'_> {
    fn drop(&mut self) {
        self.shared.levels[self.lane]
            .waiting
            .fetch_sub(1, Ordering::SeqCst);
        self.shared.slot_freed.notify_waiters();
    }
}

/// A request in a lane, stamped with its admission time.
#[derive(Debug)]
struct Queued {
    command: RequestCommand,
    enqueued_at: Instant,
}

/// Creates a request queue holding at most `capacity` requests across all levels.
pub(crate) fn request_queue(capacity: usize) -> (RequestSender, RequestReceiver) {
    let shared = Arc::new(Shared {
        capacity,
        queued: AtomicUsize::new(0),
        closed: AtomicBool::new(false),
        slot_freed: Notify::new(),
        admission: Default::default(),
        epoch: Instant::now(),
        levels: Default::default(),
    });
    let (urgent_tx, urgent_rx) = mpsc::unbounded_channel();
    let (high_tx, high_rx) = mpsc::unbounded_channel();
    let (normal_tx, normal_rx) = mpsc::unbounded_channel();
    (
        RequestSender {
            shared: Arc::clone(&shared),
            lanes: [urgent_tx, high_tx, normal_tx],
        },
        RequestReceiver {
            shared,
            lanes: [urgent_rx, high_rx, normal_rx],
            lane_closed: [false; LEVELS],
            buffered: Default::default(),
            deadline_timer: None,
        },
    )
}

/// Sending side of the request queue, held by connection handles.
///
/// The queue counts as closed for the actor once every sender has been dropped.
#[derive(Clone, Debug)]
pub(crate) struct RequestSender {
    shared: Arc<Shared>,
    lanes: [UnboundedSender<Queued>; LEVELS],
}

impl RequestSender {
    /// Admits `command` into the lane of its priority, waiting for a slot if the
    /// queue is full.
    ///
    /// Waiting for admission is bounded by the command's queue deadline, so the time
    /// a caller spends parked counts against its queue timeout.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::QueueTimeout`] if the deadline passes before a slot
    /// frees, or a terminated-actor error if the receiver is gone.
    pub(crate) async fn send(&self, command: RequestCommand) -> Result<(), ModbusError> {
        let lane = command.priority.lane();
        let _waiting = WaitingGuard::register(&self.shared, lane);
        // Keep the lane locked through enqueue, not merely through slot reservation.
        let _turn = if let Ok(result) = timeout_at(
            command.queue_deadline,
            self.wait_for_admission(lane, command.queue_deadline),
        )
        .await
        {
            result?
        } else {
            self.shared.levels[lane]
                .expired_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(ModbusError::QueueTimeout);
        };

        let counters = &self.shared.levels[lane];
        counters.queued.fetch_add(1, Ordering::SeqCst);
        counters.enqueued_total.fetch_add(1, Ordering::Relaxed);
        let queued = Queued {
            command,
            enqueued_at: Instant::now(),
        };
        if self.lanes[lane].send(queued).is_err() {
            counters.queued.fetch_sub(1, Ordering::SeqCst);
            self.shared.release_slot();
            return Err(actor_terminated_error());
        }
        Ok(())
    }

    async fn wait_for_admission(
        &self,
        lane: usize,
        deadline: Instant,
    ) -> Result<MutexGuard<'_, ()>, ModbusError> {
        let shared = &*self.shared;
        let turn = shared.admission[lane].lock().await;
        // Enable before checking capacity so a concurrent release cannot be missed.
        let mut notified = pin!(shared.slot_freed.notified());
        notified.as_mut().enable();
        loop {
            if shared.closed.load(Ordering::SeqCst) {
                return Err(actor_terminated_error());
            }
            // Notifications can be continuously ready under load. Check explicitly
            // rather than relying only on timeout_at polling its timer.
            if Instant::now() >= deadline {
                shared.levels[lane]
                    .expired_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(ModbusError::QueueTimeout);
            }
            if shared.try_admit(lane) {
                return Ok(turn);
            }
            notified.as_mut().await;
            notified.set(shared.slot_freed.notified());
            notified.as_mut().enable();
        }
    }

    /// Returns a statistics snapshot of the queue.
    pub(crate) fn stats(&self) -> QueueStats {
        self.shared.stats()
    }
}

/// Receiving side of the request queue, owned by the actor.
#[derive(Debug)]
pub(crate) struct RequestReceiver {
    shared: Arc<Shared>,
    lanes: [UnboundedReceiver<Queued>; LEVELS],
    lane_closed: [bool; LEVELS],
    buffered: [VecDeque<Queued>; LEVELS],
    deadline_timer: Option<Pin<Box<Sleep>>>,
}

impl RequestReceiver {
    /// Takes the oldest request of the highest non-empty level.
    ///
    /// Returns `None` once every sender is dropped and all lanes are drained. The
    /// future is cancel-safe: requests are only removed on return or expiration.
    #[cfg(test)]
    pub(crate) async fn recv(&mut self) -> Option<RequestCommand> {
        self.recv_when(true).await
    }

    /// Keeps deadlines running even when the actor's in-flight window is full.
    /// Cancellation preserves all buffered requests that have not expired.
    pub(crate) async fn recv_when(&mut self, dispatch: bool) -> Option<RequestCommand> {
        poll_fn(|cx| self.poll_recv(cx, dispatch)).await
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>, dispatch: bool) -> Poll<Option<RequestCommand>> {
        // Capacity is retained while buffering, bounding this drain even if producers
        // keep sending. Poll every lane to register wakes for new, earlier deadlines.
        for lane in 0..LEVELS {
            while !self.lane_closed[lane] {
                match self.lanes[lane].poll_recv(cx) {
                    Poll::Ready(Some(queued)) => self.buffered[lane].push_back(queued),
                    Poll::Ready(None) => self.lane_closed[lane] = true,
                    Poll::Pending => break,
                }
            }
        }
        self.expire_buffered();
        if dispatch {
            if let Some(command) = self.pop_buffered() {
                return Poll::Ready(Some(command));
            }
        }
        if self.lane_closed.iter().all(|closed| *closed)
            && self.buffered.iter().all(VecDeque::is_empty)
        {
            return Poll::Ready(None);
        }
        if let Some(deadline) = self
            .buffered
            .iter()
            .flatten()
            .map(|queued| queued.command.queue_deadline)
            .min()
        {
            let timer = self
                .deadline_timer
                .get_or_insert_with(|| Box::pin(sleep_until(deadline)));
            timer.as_mut().reset(deadline);
            if timer.as_mut().poll(cx).is_ready() {
                cx.waker().wake_by_ref();
            }
        }
        Poll::Pending
    }

    /// Takes the oldest request of the highest non-empty level without waiting.
    pub(crate) fn try_recv(&mut self) -> Option<RequestCommand> {
        for lane in 0..LEVELS {
            while let Ok(queued) = self.lanes[lane].try_recv() {
                self.buffered[lane].push_back(queued);
            }
        }
        self.expire_buffered();
        self.pop_buffered()
    }

    fn pop_buffered(&mut self) -> Option<RequestCommand> {
        (0..LEVELS).find_map(|lane| {
            self.buffered[lane]
                .pop_front()
                .map(|queued| self.take(lane, queued))
        })
    }

    fn expire_buffered(&mut self) {
        let now = Instant::now();
        for lane in 0..LEVELS {
            // Deadlines need not follow FIFO order (each request can override its
            // timeout), so inspect the entire lane, preserving survivor order.
            let count = self.buffered[lane].len();
            for _ in 0..count {
                let Some(queued) = self.buffered[lane].pop_front() else {
                    break;
                };
                if queued.command.queue_deadline <= now {
                    let command = self.take(lane, queued);
                    self.record_expired(command.priority);
                    let _ = command.reply.send(Err(ModbusError::QueueTimeout));
                } else {
                    self.buffered[lane].push_back(queued);
                }
            }
        }
    }

    /// Records that a dequeued request expired before reaching the wire.
    pub(crate) fn record_expired(&self, priority: Priority) {
        self.shared.levels[priority.lane()]
            .expired_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records that a dequeued request was discarded because its caller stopped waiting.
    pub(crate) fn record_abandoned(&self, priority: Priority) {
        self.shared.levels[priority.lane()]
            .abandoned_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Updates the level statistics for a dequeued request and releases its slot.
    ///
    /// The receiver is the only writer of the wait figures, so plain stores suffice.
    fn take(&self, lane: usize, queued: Queued) -> RequestCommand {
        let now = Instant::now();
        let counters = &self.shared.levels[lane];
        counters.queued.fetch_sub(1, Ordering::SeqCst);
        counters.dequeued_total.fetch_add(1, Ordering::Relaxed);

        let wait = micros(now.saturating_duration_since(queued.enqueued_at));
        counters
            .last_wait_us
            .store(encode_micros(wait), Ordering::Relaxed);
        let average = match decode_raw(counters.avg_wait_us.load(Ordering::Relaxed)) {
            None => wait,
            Some(previous) => ewma(previous, wait),
        };
        counters
            .avg_wait_us
            .store(encode_micros(average), Ordering::Relaxed);
        counters.last_dequeue_us.store(
            encode_micros(micros(now.saturating_duration_since(self.shared.epoch))),
            Ordering::Relaxed,
        );

        self.shared.release_slot();
        queued.command
    }
}

impl Drop for RequestReceiver {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.slot_freed.notify_waiters();
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX - 1)
}

fn encode_micros(micros: u64) -> u64 {
    micros.saturating_add(1)
}

fn decode_raw(encoded: u64) -> Option<u64> {
    encoded.checked_sub(1)
}

fn decode_micros(encoded: u64) -> Option<Duration> {
    decode_raw(encoded).map(Duration::from_micros)
}

fn ewma(previous: u64, sample: u64) -> u64 {
    let blended = (u128::from(previous) * (EWMA_DIVISOR - 1) + u128::from(sample)) / EWMA_DIVISOR;
    u64::try_from(blended).unwrap_or(u64::MAX - 1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::ModbusRequest;

    type Reply = oneshot::Receiver<Result<Vec<u8>, ModbusError>>;

    /// Builds a command tagged through its unit id, so tests can assert order.
    fn command(priority: Priority, tag: u8) -> (RequestCommand, Reply) {
        command_with_deadline(priority, tag, Instant::now() + Duration::from_secs(5))
    }

    fn command_with_deadline(
        priority: Priority,
        tag: u8,
        queue_deadline: Instant,
    ) -> (RequestCommand, Reply) {
        let (reply, response) = oneshot::channel();
        (
            RequestCommand {
                unit_id: tag,
                pdu: ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 1,
                },
                reply,
                queue_deadline,
                priority,
            },
            response,
        )
    }

    async fn send(tx: &RequestSender, priority: Priority, tag: u8) -> Reply {
        let (command, reply) = command(priority, tag);
        tx.send(command).await.unwrap();
        reply
    }

    /// Spawns a sender that is expected to park, and waits until it has.
    async fn spawn_parked(
        tx: &RequestSender,
        priority: Priority,
        tag: u8,
    ) -> tokio::task::JoinHandle<Result<(), ModbusError>> {
        let waiting_before = tx.stats().level(priority).waiting_for_admission;
        let sender = tx.clone();
        let (command, reply) = command(priority, tag);
        let task = tokio::spawn(async move {
            let _reply = reply;
            sender.send(command).await
        });
        timeout(Duration::from_secs(1), async {
            while tx.stats().level(priority).waiting_for_admission == waiting_before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("sender must park on a full queue");
        task
    }

    fn drain_tags(rx: &mut RequestReceiver) -> Vec<u8> {
        std::iter::from_fn(|| rx.try_recv().map(|command| command.unit_id)).collect()
    }

    #[test]
    fn priority_orders_normal_below_high_below_urgent() {
        assert!(Priority::Normal < Priority::High);
        assert!(Priority::High < Priority::Urgent);
        assert_eq!(Priority::default(), Priority::Normal);
    }

    #[tokio::test]
    async fn requests_of_one_level_keep_submission_order() {
        let (tx, mut rx) = request_queue(8);
        let _replies = [
            send(&tx, Priority::Normal, 1).await,
            send(&tx, Priority::Normal, 2).await,
            send(&tx, Priority::Normal, 3).await,
        ];

        assert_eq!(drain_tags(&mut rx), [1, 2, 3]);
    }

    /// Poll the newcomer before waking the existing waiters: scheduling must not
    /// let it steal the slot that was just released.
    #[tokio::test]
    async fn same_level_newcomer_cannot_overtake_parked_senders() {
        let (tx, mut rx) = request_queue(1);
        let _occupant = send(&tx, Priority::Normal, 1).await;
        let (first, _first_reply) = command(Priority::Normal, 2);
        let (second, _second_reply) = command(Priority::Normal, 3);
        let (newcomer, _new_reply) = command(Priority::Normal, 4);
        let mut first = pin!(tx.send(first));
        let mut second = pin!(tx.send(second));
        let mut newcomer = pin!(tx.send(newcomer));
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        assert_eq!(rx.recv().await.unwrap().unit_id, 1);
        assert!(futures_util::poll!(&mut newcomer).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        first.await.unwrap();
        assert_eq!(rx.recv().await.unwrap().unit_id, 2);
        assert!(futures_util::poll!(&mut newcomer).is_pending());
        second.await.unwrap();
        assert_eq!(rx.recv().await.unwrap().unit_id, 3);
        newcomer.await.unwrap();
        assert_eq!(rx.recv().await.unwrap().unit_id, 4);
        assert_eq!(tx.stats().normal.waiting_for_admission, 0);
    }

    #[tokio::test]
    async fn cancelled_same_level_waiters_do_not_block_their_successor() {
        let (tx, mut rx) = request_queue(1);
        let _occupant = send(&tx, Priority::Normal, 1).await;
        let first = spawn_parked(&tx, Priority::Normal, 2).await;
        let middle = spawn_parked(&tx, Priority::Normal, 3).await;
        let last = spawn_parked(&tx, Priority::Normal, 4).await;
        middle.abort();
        assert!(middle.await.unwrap_err().is_cancelled());
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(rx.recv().await.unwrap().unit_id, 1);
        assert_eq!(rx.recv().await.unwrap().unit_id, 4);
        last.await.unwrap().unwrap();
        assert_eq!(tx.stats().normal.waiting_for_admission, 0);
    }

    #[tokio::test]
    async fn same_level_waiter_can_expire_while_waiting_for_its_turn() {
        let (tx, mut rx) = request_queue(1);
        let _occupant = send(&tx, Priority::Normal, 1).await;
        let first = spawn_parked(&tx, Priority::Normal, 2).await;
        let (expiring, _reply) = command_with_deadline(
            Priority::Normal,
            3,
            Instant::now() + Duration::from_millis(10),
        );
        assert!(matches!(
            tx.send(expiring).await,
            Err(ModbusError::QueueTimeout)
        ));
        assert_eq!(tx.stats().normal.expired_total, 1);
        assert_eq!(tx.stats().normal.waiting_for_admission, 1);
        assert_eq!(rx.recv().await.unwrap().unit_id, 1);
        assert_eq!(rx.recv().await.unwrap().unit_id, 2);
        first.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn urgent_traffic_does_not_suppress_lower_lane_expiration() {
        for synchronous in [false, true] {
            let (tx, mut rx) = request_queue(4);
            // A short deadline behind a long one must also expire independently.
            let _long = send(&tx, Priority::Normal, 1).await;
            let deadline = Instant::now() + Duration::from_millis(10);
            let (short, mut reply) = command_with_deadline(Priority::Normal, 2, deadline);
            tx.send(short).await.unwrap();
            while Instant::now() < deadline + Duration::from_millis(20) {
                let _urgent = send(&tx, Priority::Urgent, 3).await;
                let next = if synchronous {
                    rx.try_recv()
                } else {
                    rx.recv().await
                };
                assert_eq!(next.unwrap().unit_id, 3);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(matches!(
                reply.try_recv(),
                Ok(Err(ModbusError::QueueTimeout))
            ));
            let stats = tx.stats();
            assert_eq!(stats.normal.expired_total, 1);
            assert_eq!(stats.normal.dequeued_total, 1);
            assert_eq!(stats.normal.queued, 1);
            assert_eq!(stats.queued, 1);
            assert_eq!(rx.recv().await.unwrap().unit_id, 1);
            assert_eq!(tx.stats().normal.expired_total, 1);
        }
    }

    #[tokio::test]
    async fn queued_deadlines_run_while_dispatch_is_disabled() {
        let (tx, mut rx) = request_queue(2);
        let _long = send(&tx, Priority::Normal, 1).await;
        // Start polling before arrival, as the actor does with a full window.
        {
            let mut receive = pin!(rx.recv_when(false));
            assert!(futures_util::poll!(&mut receive).is_pending());
        }
        let (short, reply) = command_with_deadline(
            Priority::Normal,
            2,
            Instant::now() + Duration::from_millis(10),
        );
        tx.send(short).await.unwrap();
        timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = reply => assert!(matches!(result, Ok(Err(ModbusError::QueueTimeout)))),
                _ = rx.recv_when(false) => panic!("dispatch must remain disabled"),
            }
        })
        .await
        .expect("queued request must expire without a dequeue");
        assert_eq!(tx.stats().normal.expired_total, 1);
        assert_eq!(tx.stats().queued, 1);
        assert_eq!(rx.recv().await.unwrap().unit_id, 1);
    }

    #[tokio::test]
    async fn higher_levels_are_dequeued_first() {
        let (tx, mut rx) = request_queue(8);
        let _replies = [
            send(&tx, Priority::Normal, 1).await,
            send(&tx, Priority::High, 2).await,
            send(&tx, Priority::Normal, 3).await,
            send(&tx, Priority::Urgent, 4).await,
            send(&tx, Priority::High, 5).await,
        ];

        assert_eq!(drain_tags(&mut rx), [4, 2, 5, 1, 3]);
    }

    #[tokio::test]
    async fn recv_waits_for_a_request_and_prefers_the_highest_level() {
        let (tx, mut rx) = request_queue(8);
        let _normal = send(&tx, Priority::Normal, 1).await;
        let _urgent = send(&tx, Priority::Urgent, 2).await;

        assert_eq!(rx.recv().await.unwrap().unit_id, 2);
        assert_eq!(rx.recv().await.unwrap().unit_id, 1);
        assert!(
            timeout(Duration::from_millis(20), rx.recv()).await.is_err(),
            "an empty open queue must wait"
        );
    }

    #[tokio::test]
    async fn recv_drains_every_lane_before_reporting_closed() {
        let (tx, mut rx) = request_queue(8);
        let _replies = [
            send(&tx, Priority::Normal, 1).await,
            send(&tx, Priority::High, 2).await,
        ];
        drop(tx);

        assert_eq!(rx.recv().await.unwrap().unit_id, 2);
        assert_eq!(rx.recv().await.unwrap().unit_id, 1);
        assert!(rx.recv().await.is_none());
    }

    /// Under overload a freed slot goes to the highest parked level, not to the
    /// sender that parked first.
    #[tokio::test]
    async fn freed_slot_goes_to_the_highest_parked_level() {
        let (tx, mut rx) = request_queue(1);
        let _occupant = send(&tx, Priority::Normal, 1).await;
        let normal = spawn_parked(&tx, Priority::Normal, 2).await;
        let high = spawn_parked(&tx, Priority::High, 3).await;
        let urgent = spawn_parked(&tx, Priority::Urgent, 4).await;

        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(rx.recv().await.unwrap().unit_id);
        }

        assert_eq!(order, [1, 4, 3, 2]);
        for task in [normal, high, urgent] {
            task.await.unwrap().unwrap();
        }
    }

    #[test]
    fn admission_defers_to_parked_senders_of_higher_levels() {
        let (tx, _rx) = request_queue(8);
        let shared = &*tx.shared;
        let high = Priority::High.lane();
        shared.levels[high].waiting.store(1, Ordering::SeqCst);

        assert!(!shared.try_admit(Priority::Normal.lane()));
        assert!(shared.try_admit(high));
        assert!(shared.try_admit(Priority::Urgent.lane()));
    }

    /// A parked sender that gives up must stop holding back lower levels.
    #[tokio::test]
    async fn cancelled_parked_sender_releases_lower_levels() {
        let (tx, mut rx) = request_queue(1);
        let _occupant = send(&tx, Priority::Normal, 1).await;
        let high = spawn_parked(&tx, Priority::High, 2).await;
        let normal = spawn_parked(&tx, Priority::Normal, 3).await;

        high.abort();
        assert!(high.await.unwrap_err().is_cancelled());
        assert_eq!(tx.stats().high.waiting_for_admission, 0);

        assert_eq!(rx.recv().await.unwrap().unit_id, 1);
        assert_eq!(rx.recv().await.unwrap().unit_id, 3);
        normal.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn parked_sender_times_out_at_its_queue_deadline() {
        let (tx, _rx) = request_queue(1);
        let _occupant = send(&tx, Priority::Normal, 1).await;
        let (command, _reply) = command_with_deadline(
            Priority::High,
            2,
            Instant::now() + Duration::from_millis(20),
        );

        let result = timeout(Duration::from_secs(1), tx.send(command))
            .await
            .unwrap();

        assert!(matches!(result, Err(ModbusError::QueueTimeout)));
        let stats = tx.stats();
        assert_eq!(stats.high.expired_total, 1);
        assert_eq!(stats.high.waiting_for_admission, 0);
        assert_eq!(stats.high.enqueued_total, 0);
    }

    #[tokio::test]
    async fn dropping_the_receiver_fails_parked_and_new_senders() {
        let (tx, rx) = request_queue(1);
        let _occupant = send(&tx, Priority::Normal, 1).await;
        let parked = spawn_parked(&tx, Priority::Urgent, 2).await;

        drop(rx);

        assert!(matches!(
            parked.await.unwrap(),
            Err(ModbusError::ReadError(error))
                if error.kind() == std::io::ErrorKind::ConnectionAborted
        ));
        let (command, _reply) = command(Priority::Normal, 3);
        assert!(tx.send(command).await.is_err());
        assert_eq!(
            tx.stats().queued,
            1,
            "only the original occupant was admitted"
        );
    }

    #[tokio::test]
    async fn stats_track_gauges_counters_and_queue_wait() {
        let (tx, mut rx) = request_queue(8);
        let initial = tx.stats();
        assert_eq!(initial.capacity, 8);
        assert_eq!(initial.normal, LevelStats::default());

        let _replies = [
            send(&tx, Priority::Normal, 1).await,
            send(&tx, Priority::Normal, 2).await,
            send(&tx, Priority::Urgent, 3).await,
        ];
        let queued = tx.stats();
        assert_eq!(queued.queued, 3);
        assert_eq!(queued.normal.queued, 2);
        assert_eq!(queued.urgent.queued, 1);
        assert_eq!(queued.normal.enqueued_total, 2);
        assert_eq!(queued.high, LevelStats::default());

        tokio::time::sleep(Duration::from_millis(20)).await;
        let urgent = rx.recv().await.unwrap();
        rx.record_abandoned(urgent.priority);
        let first_normal = rx.recv().await.unwrap();
        rx.record_expired(first_normal.priority);

        let stats = tx.stats();
        assert_eq!(stats.queued, 1);
        assert_eq!(stats.urgent.queued, 0);
        assert_eq!(stats.urgent.dequeued_total, 1);
        assert_eq!(stats.urgent.abandoned_total, 1);
        assert_eq!(stats.normal.queued, 1);
        assert_eq!(stats.normal.dequeued_total, 1);
        assert_eq!(stats.normal.expired_total, 1);
        let wait = stats.normal.last_queue_wait.unwrap();
        assert!(wait >= Duration::from_millis(20), "wait was {wait:?}");
        assert_eq!(stats.normal.avg_queue_wait, Some(wait));
        assert!(stats.normal.since_last_dequeue.is_some());
        assert_eq!(stats.high.since_last_dequeue, None);
        assert_eq!(stats.level(Priority::Urgent), &stats.urgent);
    }

    /// Many concurrent senders on a tiny queue must all be admitted and delivered:
    /// a lost wakeup in the admission path shows up here as a timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_senders_on_a_full_queue_are_all_delivered() {
        const SENDERS: usize = 64;
        const PER_SENDER: usize = 50;
        let (tx, mut rx) = request_queue(2);
        let priorities = [Priority::Normal, Priority::High, Priority::Urgent];
        let mut tasks = Vec::new();
        for index in 0..SENDERS {
            let tx = tx.clone();
            let priority = priorities[index % priorities.len()];
            tasks.push(tokio::spawn(async move {
                for _ in 0..PER_SENDER {
                    let (command, _reply) = command_with_deadline(
                        priority,
                        0,
                        Instant::now() + Duration::from_secs(30),
                    );
                    tx.send(command).await.unwrap();
                }
            }));
        }
        drop(tx);

        let received = timeout(Duration::from_secs(20), async {
            let mut received = 0;
            while rx.recv().await.is_some() {
                received += 1;
                if received % 7 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            received
        })
        .await
        .expect("every admitted request must be delivered without a lost wakeup");

        assert_eq!(received, SENDERS * PER_SENDER);
        for task in tasks {
            task.await.unwrap();
        }
        let stats = rx.shared.stats();
        assert_eq!(stats.queued, 0);
        for priority in priorities {
            let level = stats.level(priority);
            assert_eq!(level.queued, 0);
            assert_eq!(level.waiting_for_admission, 0);
            assert_eq!(level.enqueued_total, level.dequeued_total);
        }
    }

    #[test]
    fn ewma_weights_the_newest_sample_one_eighth() {
        assert_eq!(ewma(800, 0), 700);
        assert_eq!(ewma(0, 800), 100);
        assert_eq!(ewma(u64::MAX - 1, u64::MAX - 1), u64::MAX - 1);
    }

    #[test]
    fn optional_micros_encode_none_as_zero() {
        assert_eq!(decode_micros(0), None);
        assert_eq!(decode_micros(encode_micros(0)), Some(Duration::ZERO));
        assert_eq!(
            decode_micros(encode_micros(micros(Duration::from_millis(3)))),
            Some(Duration::from_millis(3))
        );
    }
}
