// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Modbus TCP server transport.

use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tokio_util::codec::Framed;
use tracing::{debug, info, warn};

use crate::error::ModbusError;
use crate::server::{self, ModbusServer};
use crate::tcp::adu::build_modbus_tcp_adu_from_pdu_bytes;
use crate::tcp::codec::MbapCodec;
use crate::tcp::frame::MBAP_HEADER_LEN;

const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for a Modbus TCP server before binding.
pub struct ModbusTcpServer<S: ModbusServer> {
    server: Arc<S>,
    timeouts: ModbusTcpServerTimeouts,
    max_connections: Option<NonZeroUsize>,
}

/// A Modbus TCP server with a bound listener.
pub struct BoundModbusTcpServer<S: ModbusServer> {
    listener: TcpListener,
    local_addr: SocketAddr,
    config: ModbusTcpServer<S>,
}

/// Per-connection server I/O timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModbusTcpServerTimeouts {
    /// Maximum time to read a header or PDU body before disconnecting an idle client.
    pub read_timeout: Duration,
    /// Maximum time to write one response frame.
    pub write_timeout: Duration,
}

impl Default for ModbusTcpServerTimeouts {
    fn default() -> Self {
        Self {
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(5),
        }
    }
}

impl<S: ModbusServer> ModbusTcpServer<S> {
    /// Creates a server configuration with default timeouts.
    #[must_use]
    pub fn new(server: S) -> Self {
        Self {
            server: Arc::new(server),
            timeouts: ModbusTcpServerTimeouts::default(),
            max_connections: None,
        }
    }

    /// Sets custom per-connection I/O timeouts.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: ModbusTcpServerTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Sets a maximum number of concurrent connection tasks.
    #[must_use]
    pub fn with_max_connections(mut self, max_connections: NonZeroUsize) -> Self {
        self.max_connections = Some(max_connections);
        self
    }

    /// Binds a TCP listener and records its local address.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::BindError`] if binding or obtaining the local address fails.
    pub async fn bind(self, bind: SocketAddr) -> Result<BoundModbusTcpServer<S>, ModbusError> {
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|source| ModbusError::BindError {
                addr: bind,
                source: Arc::new(source),
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ModbusError::BindError {
                addr: bind,
                source: Arc::new(source),
            })?;
        info!(%local_addr, "bound Modbus TCP server");
        Ok(BoundModbusTcpServer {
            listener,
            local_addr,
            config: self,
        })
    }
}

impl<S: ModbusServer> BoundModbusTcpServer<S> {
    /// Returns the actual local address of the bound listener.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves connections until the task is cancelled or the listener is closed.
    ///
    /// # Errors
    ///
    /// Never returns an error in practice: every failure that can abort startup is
    /// already reported by [`ModbusTcpServer::bind`], and per-connection or transient
    /// accept errors are logged and recovered from. The `Result` is kept so future
    /// listener-level failures stay a non-breaking change.
    pub async fn serve(self) -> Result<(), ModbusError> {
        self.serve_with_shutdown(std::future::pending::<()>()).await
    }

    /// Serves connections until `shutdown` resolves, then drains connection tasks.
    ///
    /// Resolving `shutdown` stops the accept loop and asks every live connection task
    /// to stop at its next frame boundary, so idle clients are closed promptly instead
    /// of waiting for their read timeout. In-flight requests are allowed to finish;
    /// tasks still running after a 10-second drain deadline are aborted.
    ///
    /// # Errors
    ///
    /// Never returns an error in practice; see [`Self::serve`].
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<(), ModbusError>
    where
        F: Future<Output = ()> + Send,
    {
        self.run_accept_loop(shutdown).await;
        Ok(())
    }

    async fn run_accept_loop<F>(self, shutdown: F)
    where
        F: Future<Output = ()> + Send,
    {
        let Self {
            listener,
            local_addr,
            config,
        } = self;
        let ModbusTcpServer {
            server,
            timeouts,
            max_connections,
        } = config;
        let semaphore = max_connections.map(|limit| Arc::new(Semaphore::new(limit.get())));
        let mut joinset = JoinSet::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!(%local_addr, "shutting down Modbus TCP server");
                    // Ask live connections to stop at their next frame boundary so the
                    // drain does not have to wait for idle clients to time out.
                    let _observed = shutdown_tx.send(true);
                    drain_connections(&mut joinset).await;
                    return;
                }
                joined = joinset.join_next(), if !joinset.is_empty() => {
                    if let Some(Err(error)) = joined {
                        warn!(%error, "Modbus TCP connection task failed");
                    }
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer_addr)) => {
                            debug!(%peer_addr, "accepted Modbus TCP connection");
                            let permit = match &semaphore {
                                Some(semaphore) => {
                                    if let Ok(permit) = semaphore.clone().try_acquire_owned() {
                                        Some(permit)
                                    } else {
                                        debug!(%peer_addr, "closing connection because max_connections is saturated");
                                        drop(stream);
                                        continue;
                                    }
                                }
                                None => None,
                            };
                            if let Err(error) = stream.set_nodelay(true) {
                                debug!(%peer_addr, %error, "failed to disable Nagle on accepted connection");
                            }
                            let server = server.clone();
                            let connection_shutdown = shutdown_rx.clone();
                            joinset.spawn(async move {
                                handle_connection(
                                    stream,
                                    peer_addr,
                                    server,
                                    timeouts,
                                    connection_shutdown,
                                    permit,
                                )
                                .await;
                            });
                        }
                        Err(error) => {
                            warn!(%error, "failed to accept Modbus TCP connection");
                            sleep(ACCEPT_BACKOFF).await;
                        }
                    }
                }
            }
        }
    }
}

async fn drain_connections(joinset: &mut JoinSet<()>) {
    let drain = async {
        while let Some(joined) = joinset.join_next().await {
            if let Err(error) = joined {
                warn!(%error, "Modbus TCP connection task failed during shutdown");
            }
        }
    };
    if timeout(SHUTDOWN_DRAIN_TIMEOUT, drain).await.is_err() {
        warn!("aborting Modbus TCP connection tasks after shutdown deadline");
        joinset.abort_all();
        while let Some(joined) = joinset.join_next().await {
            if let Err(error) = joined {
                warn!(%error, "aborted Modbus TCP connection task");
            }
        }
    }
}

/// Resolves once the accept loop has requested shutdown, or the sender is gone.
async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    let _result = shutdown.wait_for(|stop| *stop).await;
}

async fn handle_connection<S: ModbusServer>(
    stream: TcpStream,
    peer_addr: SocketAddr,
    server: Arc<S>,
    timeouts: ModbusTcpServerTimeouts,
    mut shutdown: watch::Receiver<bool>,
    permit: Option<OwnedSemaphorePermit>,
) {
    let mut framed = Framed::new(stream, MbapCodec);
    loop {
        // Only the next-frame read races shutdown: once a complete frame is in
        // hand the connection finishes the exchange before exiting.
        let frame = tokio::select! {
            biased;
            () = shutdown_requested(&mut shutdown) => {
                debug!(%peer_addr, "closing idle connection for server shutdown");
                break;
            }
            result = timeout(timeouts.read_timeout, framed.next()) => match result {
                Err(_) => {
                    debug!(%peer_addr, "stopping connection after frame read timeout");
                    break;
                }
                Ok(None) => {
                    debug!(%peer_addr, "client closed Modbus TCP connection");
                    break;
                }
                Ok(Some(Err(error))) => {
                    warn!(%peer_addr, %error, "stopping connection after frame read failure");
                    break;
                }
                Ok(Some(Ok(frame))) => frame,
            },
        };

        let header = &frame[..MBAP_HEADER_LEN];
        let transaction_id = u16::from_be_bytes([header[0], header[1]]);
        let protocol_id = u16::from_be_bytes([header[2], header[3]]);
        if protocol_id != 0 {
            warn!(%peer_addr, protocol_id, "closing connection after invalid Modbus protocol id");
            break;
        }
        let unit_id = header[6];
        let pdu_bytes = &frame[MBAP_HEADER_LEN..];

        debug!(%peer_addr, unit_id, function_code = pdu_bytes.first().copied().unwrap_or(0), "processing Modbus TCP frame");
        let response_pdu = server::dispatch::process_pdu(&*server, unit_id, pdu_bytes).await;
        let response_frame =
            match build_modbus_tcp_adu_from_pdu_bytes(transaction_id, unit_id, &response_pdu) {
                Ok(frame) => frame,
                Err(error) => {
                    warn!(%peer_addr, %error, "failed to frame Modbus TCP response");
                    break;
                }
            };
        match timeout(timeouts.write_timeout, framed.send(response_frame)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                debug!(%peer_addr, %error, "TCP write failed");
                break;
            }
            Err(_) => {
                debug!(%peer_addr, "TCP write timed out");
                break;
            }
        }
    }
    drop(permit);
}
