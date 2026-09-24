// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Modbus server abstractions.

use std::future::Future;

use crate::{ExceptionCode, ModbusRequest, ModbusResponse};

pub(crate) mod dispatch;
mod store;

pub use store::{InMemoryStore, StoreError};

/// Implemented by user code to back a Modbus server.
///
/// Implementors receive already-validated [`ModbusRequest`] values and must
/// return either a matching [`ModbusResponse`] variant or an [`ExceptionCode`].
/// The transport layer turns the [`ExceptionCode`] into a proper exception PDU;
/// implementors must not build exception responses themselves.
pub trait ModbusServer: Send + Sync + 'static {
    /// Handles one validated request for the supplied Modbus unit id.
    ///
    /// # Arguments
    ///
    /// * `unit_id` - Unit identifier from the transport ADU.
    /// * `request` - Validated request PDU.
    ///
    /// # Returns
    ///
    /// Returns a future resolving to a matching response or an exception code.
    fn handle(
        &self,
        unit_id: u8,
        request: ModbusRequest,
    ) -> impl Future<Output = Result<ModbusResponse, ExceptionCode>> + Send;
}
