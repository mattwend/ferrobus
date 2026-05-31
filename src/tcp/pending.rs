// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Pending request map, transaction-id allocation, and cleanup guard.

use std::collections::{HashMap, hash_map::Entry};
use std::io;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU16, Ordering},
};

use tokio::sync::oneshot;
use tracing::{debug, error};

use crate::error::ModbusError;

pub(crate) const MAX_TID_PROBES: usize = 256;

pub(crate) type Pending = StdMutex<HashMap<u16, oneshot::Sender<Result<Vec<u8>, ModbusError>>>>;

/// RAII cleanup for one pending transaction-id entry.
///
/// The guard is armed after a request inserts its response channel into the
/// pending map. If the request future is cancelled before a response arrives,
/// dropping the armed guard removes that entry so a later stray response is
/// ignored instead of holding stale state. Once the reader has matched the
/// response and removed the entry, callers must disarm the guard.
pub(crate) struct PendingGuard {
    /// Shared pending-response map containing this request's transaction id.
    pub(crate) pending: Arc<Pending>,
    /// Transaction id allocated for the guarded request.
    pub(crate) tid: u16,
    /// Whether drop should remove `tid` from `pending`.
    pub(crate) armed: bool,
}

impl PendingGuard {
    /// Disables automatic pending-map removal on drop.
    ///
    /// Call this after the reader has already removed the entry and delivered the
    /// response to avoid logging a misleading second removal.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            match self.pending.lock() {
                Ok(mut map) => {
                    let _ = map.remove(&self.tid);
                    debug!(
                        tid = self.tid,
                        in_flight = map.len(),
                        "pending request removed"
                    );
                }
                Err(_) => {
                    error!(
                        tid = self.tid,
                        "pending map poisoned while removing request"
                    );
                }
            }
        }
    }
}

/// Allocates and inserts the next available transaction id into the pending map.
///
/// # Arguments
/// * `transaction_id` - Atomic counter containing the next transaction id probe.
/// * `pending` - Map of in-flight transaction ids.
/// * `sender` - Response sender to store for the allocated transaction id.
///
/// # Returns
/// Returns the allocated transaction id.
///
/// # Errors
/// Returns [`ModbusError::ReadError`] if the pending map is poisoned, or
/// [`ModbusError::NoFreeTransactionId`] if no id is free after the probe limit.
pub(crate) fn allocate_transaction_id(
    transaction_id: &AtomicU16,
    pending: &Pending,
    sender: oneshot::Sender<Result<Vec<u8>, ModbusError>>,
) -> Result<u16, ModbusError> {
    let mut map = pending
        .lock()
        .map_err(|_| ModbusError::ReadError(io::Error::other("pending map poisoned")))?;

    for _ in 0..MAX_TID_PROBES {
        let tid = transaction_id.fetch_add(1, Ordering::Relaxed);
        if let Entry::Vacant(entry) = map.entry(tid) {
            entry.insert(sender);
            debug!(tid, in_flight = map.len(), "pending request inserted");
            return Ok(tid);
        }
    }

    Err(ModbusError::NoFreeTransactionId)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn allocate_pending_transaction_id_inserts_atomically() {
        let transaction_id = AtomicU16::new(0);
        let pending = StdMutex::new(HashMap::new());
        let (tx, _rx) = oneshot::channel();

        let tid = allocate_transaction_id(&transaction_id, &pending, tx).unwrap();

        let map = pending.lock().unwrap();
        assert_eq!(tid, 0);
        assert!(map.contains_key(&0));
    }

    #[test]
    fn allocate_pending_transaction_id_reports_no_free_tid_after_probe_limit() {
        let transaction_id = AtomicU16::new(0);
        let pending = StdMutex::new(HashMap::new());
        {
            let mut map = pending.lock().unwrap();
            for tid in 0..u16::try_from(MAX_TID_PROBES).unwrap() {
                let (tx, _rx) = oneshot::channel();
                map.insert(tid, tx);
            }
        }
        let (tx, _rx) = oneshot::channel();

        let error = allocate_transaction_id(&transaction_id, &pending, tx).unwrap_err();

        assert!(matches!(error, ModbusError::NoFreeTransactionId));
    }

    #[test]
    fn allocate_pending_transaction_id_wraps_around_u16_max() {
        let transaction_id = AtomicU16::new(u16::MAX);
        let pending = StdMutex::new(HashMap::new());
        let (first_tx, _first_rx) = oneshot::channel();
        let (second_tx, _second_rx) = oneshot::channel();

        let first_tid = allocate_transaction_id(&transaction_id, &pending, first_tx).unwrap();
        let second_tid = allocate_transaction_id(&transaction_id, &pending, second_tx).unwrap();

        let map = pending.lock().unwrap();
        assert_eq!(first_tid, u16::MAX);
        assert_eq!(second_tid, 0);
        assert!(map.contains_key(&u16::MAX));
        assert!(map.contains_key(&0));
    }

    #[test]
    fn pending_guard_removes_cancelled_request_entry() {
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx);

        let guard = PendingGuard {
            pending: Arc::clone(&pending),
            tid: 7,
            armed: true,
        };
        drop(guard);

        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn pending_guard_tolerates_poisoned_map_on_drop() {
        let pending: Arc<Pending> = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().insert(7u16, tx);
        let poisoner = Arc::clone(&pending);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the pending map");
        })
        .join();

        let guard = PendingGuard {
            pending: Arc::clone(&pending),
            tid: 7,
            armed: true,
        };

        // Dropping an armed guard against a poisoned map must not panic; because
        // the lock cannot be acquired, the pending entry remains untouched.
        drop(guard);

        let lock_error = pending.lock().unwrap_err();
        assert!(lock_error.into_inner().contains_key(&7));
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
}
