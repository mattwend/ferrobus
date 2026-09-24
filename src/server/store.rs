// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Reference in-memory Modbus data store.

use std::future::Future;
use std::ops::Range;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::server::ModbusServer;
use crate::{ExceptionCode, ModbusRequest, ModbusResponse};

const MAX_TABLE_LEN: usize = 65_536;

/// Errors returned by the in-memory reference store.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    /// An address or address range falls outside the table.
    #[error("address {address} out of range (table size {size})")]
    AddressOutOfRange {
        /// First address that could not be served.
        ///
        /// For a request that starts past the end of the table this is the
        /// requested start address; for a request that starts inside the table
        /// but runs past its end this is the table length.
        address: usize,
        /// Table size.
        size: usize,
    },
    /// A write request carried more values than the Modbus quantity field can express.
    #[error("write quantity {quantity} does not fit in the Modbus quantity field")]
    QuantityTooLarge {
        /// Number of values supplied by the request.
        quantity: usize,
    },
    /// A table was larger than the Modbus address space.
    #[error("{table} table size {size} exceeds 65536")]
    TableTooLarge {
        /// Table name.
        table: &'static str,
        /// Requested table size.
        size: usize,
    },
    /// A lock was poisoned by a panicking holder.
    #[error("internal store lock poisoned")]
    LockPoisoned,
}

#[derive(Debug, PartialEq, Eq)]
enum StoreAccessError {
    AddressOutOfRange { address: usize, size: usize },
    QuantityTooLarge { quantity: usize },
    LockPoisoned,
}

impl From<StoreAccessError> for StoreError {
    fn from(error: StoreAccessError) -> Self {
        match error {
            StoreAccessError::AddressOutOfRange { address, size } => {
                StoreError::AddressOutOfRange { address, size }
            }
            StoreAccessError::QuantityTooLarge { quantity } => {
                StoreError::QuantityTooLarge { quantity }
            }
            StoreAccessError::LockPoisoned => StoreError::LockPoisoned,
        }
    }
}

/// Cloneable in-memory store covering the four standard Modbus tables.
#[derive(Clone)]
pub struct InMemoryStore {
    coils: Arc<RwLock<Vec<bool>>>,
    discrete_inputs: Arc<RwLock<Vec<bool>>>,
    holding_registers: Arc<RwLock<Vec<u16>>>,
    input_registers: Arc<RwLock<Vec<u16>>>,
}

impl InMemoryStore {
    /// Creates a store with the requested table lengths.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TableTooLarge`] when any table exceeds 65,536 entries.
    pub fn new(
        coil_count: usize,
        discrete_count: usize,
        holding_count: usize,
        input_count: usize,
    ) -> Result<Self, StoreError> {
        validate_table_size("coils", coil_count)?;
        validate_table_size("discrete_inputs", discrete_count)?;
        validate_table_size("holding_registers", holding_count)?;
        validate_table_size("input_registers", input_count)?;
        Ok(Self {
            coils: Arc::new(RwLock::new(vec![false; coil_count])),
            discrete_inputs: Arc::new(RwLock::new(vec![false; discrete_count])),
            holding_registers: Arc::new(RwLock::new(vec![0; holding_count])),
            input_registers: Arc::new(RwLock::new(vec![0; input_count])),
        })
    }

    /// Sets one coil value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn set_coil(&self, address: u16, value: bool) -> Result<(), StoreError> {
        write_single(&self.coils, address, value).map_err(StoreError::from)
    }

    /// Reads one coil value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn get_coil(&self, address: u16) -> Result<bool, StoreError> {
        read_single(&self.coils, address).map_err(StoreError::from)
    }

    /// Sets one discrete-input value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn set_discrete(&self, address: u16, value: bool) -> Result<(), StoreError> {
        write_single(&self.discrete_inputs, address, value).map_err(StoreError::from)
    }

    /// Reads one discrete-input value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn get_discrete(&self, address: u16) -> Result<bool, StoreError> {
        read_single(&self.discrete_inputs, address).map_err(StoreError::from)
    }

    /// Sets one holding-register value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn set_holding(&self, address: u16, value: u16) -> Result<(), StoreError> {
        write_single(&self.holding_registers, address, value).map_err(StoreError::from)
    }

    /// Reads one holding-register value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn get_holding(&self, address: u16) -> Result<u16, StoreError> {
        read_single(&self.holding_registers, address).map_err(StoreError::from)
    }

    /// Sets one input-register value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn set_input(&self, address: u16, value: u16) -> Result<(), StoreError> {
        write_single(&self.input_registers, address, value).map_err(StoreError::from)
    }

    /// Reads one input-register value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the address is out of range or the table lock is poisoned.
    pub fn get_input(&self, address: u16) -> Result<u16, StoreError> {
        read_single(&self.input_registers, address).map_err(StoreError::from)
    }

    fn handle_sync(&self, request: ModbusRequest) -> Result<ModbusResponse, StoreAccessError> {
        match request {
            ModbusRequest::ReadCoils {
                starting_address,
                quantity,
            } => Ok(ModbusResponse::ReadCoils {
                coils: read_range(&self.coils, starting_address, quantity)?,
            }),
            ModbusRequest::ReadDiscreteInputs {
                starting_address,
                quantity,
            } => Ok(ModbusResponse::ReadDiscreteInputs {
                inputs: read_range(&self.discrete_inputs, starting_address, quantity)?,
            }),
            ModbusRequest::ReadHoldingRegisters {
                starting_address,
                quantity,
            } => Ok(ModbusResponse::ReadHoldingRegisters {
                registers: read_range(&self.holding_registers, starting_address, quantity)?,
            }),
            ModbusRequest::ReadInputRegisters {
                starting_address,
                quantity,
            } => Ok(ModbusResponse::ReadInputRegisters {
                registers: read_range(&self.input_registers, starting_address, quantity)?,
            }),
            ModbusRequest::WriteSingleCoil { address, value } => {
                write_single(&self.coils, address, value)?;
                Ok(ModbusResponse::WriteSingleCoil { address, value })
            }
            ModbusRequest::WriteSingleRegister { address, value } => {
                write_single(&self.holding_registers, address, value)?;
                Ok(ModbusResponse::WriteSingleRegister { address, value })
            }
            ModbusRequest::WriteMultipleCoils {
                starting_address,
                values,
            } => {
                // Compute the echoed quantity before touching the table so an
                // unrepresentable quantity cannot leave a partial write behind.
                let quantity = acknowledged_quantity(values.len())?;
                write_range(&self.coils, starting_address, &values)?;
                Ok(ModbusResponse::WriteMultipleCoils {
                    starting_address,
                    quantity,
                })
            }
            ModbusRequest::WriteMultipleRegisters {
                starting_address,
                values,
            } => {
                let quantity = acknowledged_quantity(values.len())?;
                write_range(&self.holding_registers, starting_address, &values)?;
                Ok(ModbusResponse::WriteMultipleRegisters {
                    starting_address,
                    quantity,
                })
            }
        }
    }
}

impl ModbusServer for InMemoryStore {
    fn handle(
        &self,
        _unit_id: u8,
        request: ModbusRequest,
    ) -> impl Future<Output = Result<ModbusResponse, ExceptionCode>> + Send {
        let result = self
            .handle_sync(request)
            .map_err(|error| store_access_error_to_exception(&error));
        async move { result }
    }
}

fn validate_table_size(table: &'static str, size: usize) -> Result<(), StoreError> {
    if size > MAX_TABLE_LEN {
        return Err(StoreError::TableTooLarge { table, size });
    }
    Ok(())
}

fn acknowledged_quantity(len: usize) -> Result<u16, StoreAccessError> {
    u16::try_from(len).map_err(|_| StoreAccessError::QuantityTooLarge { quantity: len })
}

/// Builds the error for a range that does not fit inside a table of `size` entries.
///
/// The reported address is the first one that could not be served.
fn out_of_range(start: usize, size: usize) -> StoreAccessError {
    StoreAccessError::AddressOutOfRange {
        address: start.max(size),
        size,
    }
}

/// Resolves `start..start + quantity` against an already-locked table.
///
/// Returning a range that was checked against the very slice the caller holds is
/// what makes indexing the guard panic-free by construction: no length is
/// observed outside the guard, so there is no window in which the table could
/// change between validation and use.
fn checked_range(
    len: usize,
    start: u16,
    quantity: usize,
) -> Result<Range<usize>, StoreAccessError> {
    let start = usize::from(start);
    let end = start
        .checked_add(quantity)
        .ok_or_else(|| out_of_range(start, len))?;
    if end > len {
        return Err(out_of_range(start, len));
    }
    Ok(start..end)
}

fn read_single<T: Copy>(table: &RwLock<Vec<T>>, address: u16) -> Result<T, StoreAccessError> {
    let guard = table.read().map_err(|_| StoreAccessError::LockPoisoned)?;
    let index = usize::from(address);
    guard
        .get(index)
        .copied()
        .ok_or(StoreAccessError::AddressOutOfRange {
            address: index,
            size: guard.len(),
        })
}

fn write_single<T: Copy>(
    table: &RwLock<Vec<T>>,
    address: u16,
    value: T,
) -> Result<(), StoreAccessError> {
    let mut guard = table.write().map_err(|_| StoreAccessError::LockPoisoned)?;
    let index = usize::from(address);
    let size = guard.len();
    let Some(slot) = guard.get_mut(index) else {
        return Err(StoreAccessError::AddressOutOfRange {
            address: index,
            size,
        });
    };
    *slot = value;
    Ok(())
}

fn read_range<T: Copy>(
    table: &RwLock<Vec<T>>,
    starting_address: u16,
    quantity: u16,
) -> Result<Vec<T>, StoreAccessError> {
    let guard = table.read().map_err(|_| StoreAccessError::LockPoisoned)?;
    let range = checked_range(guard.len(), starting_address, usize::from(quantity))?;
    Ok(guard[range].to_vec())
}

fn write_range<T: Copy>(
    table: &RwLock<Vec<T>>,
    starting_address: u16,
    values: &[T],
) -> Result<(), StoreAccessError> {
    let mut guard = table.write().map_err(|_| StoreAccessError::LockPoisoned)?;
    let range = checked_range(guard.len(), starting_address, values.len())?;
    guard[range].copy_from_slice(values);
    Ok(())
}

fn store_access_error_to_exception(error: &StoreAccessError) -> ExceptionCode {
    match error {
        StoreAccessError::AddressOutOfRange { .. } => ExceptionCode::IllegalDataAddress,
        StoreAccessError::QuantityTooLarge { .. } => ExceptionCode::IllegalDataValue,
        StoreAccessError::LockPoisoned => ExceptionCode::ServerDeviceFailure,
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn exactly_full_address_space_is_accepted() {
        let store = InMemoryStore::new(65_536, 65_536, 65_536, 65_536).unwrap();
        store.set_holding(u16::MAX, 7).unwrap();
        assert_eq!(store.get_holding(u16::MAX).unwrap(), 7);
    }

    #[test]
    fn oversized_table_is_rejected() {
        assert!(matches!(
            InMemoryStore::new(65_537, 0, 0, 0),
            Err(StoreError::TableTooLarge { table: "coils", .. })
        ));
    }

    #[tokio::test]
    async fn handle_reads_and_writes() {
        let store = InMemoryStore::new(10, 10, 10, 10).unwrap();
        store.set_discrete(0, true).unwrap();
        store.set_input(0, 42).unwrap();
        store
            .handle(
                1,
                ModbusRequest::WriteMultipleRegisters {
                    starting_address: 0,
                    values: vec![1, 2],
                },
            )
            .await
            .unwrap();
        let response = store
            .handle(
                1,
                ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0,
                    quantity: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            response,
            ModbusResponse::ReadHoldingRegisters {
                registers: vec![1, 2]
            }
        );
    }

    #[test]
    fn range_error_reports_the_first_address_that_could_not_be_served() {
        let store = InMemoryStore::new(0, 0, 4, 0).unwrap();

        // Starts inside the table but runs past the end: the table length is the
        // first unavailable address.
        assert_eq!(
            read_range(&store.holding_registers, 3, 4).unwrap_err(),
            StoreAccessError::AddressOutOfRange {
                address: 4,
                size: 4
            }
        );

        // Starts past the end: the requested start address is already unavailable.
        assert_eq!(
            read_range(&store.holding_registers, 9, 1).unwrap_err(),
            StoreAccessError::AddressOutOfRange {
                address: 9,
                size: 4
            }
        );

        // `start + quantity` overflowing usize is reported the same way.
        assert_eq!(
            checked_range(4, u16::MAX, usize::MAX).unwrap_err(),
            StoreAccessError::AddressOutOfRange {
                address: usize::from(u16::MAX),
                size: 4
            }
        );
    }

    #[test]
    fn full_range_of_a_table_is_readable_and_writable() {
        let store = InMemoryStore::new(0, 0, 4, 0).unwrap();
        write_range(&store.holding_registers, 0, &[1, 2, 3, 4]).unwrap();
        assert_eq!(
            read_range(&store.holding_registers, 0, 4).unwrap(),
            [1, 2, 3, 4]
        );
        // Empty ranges at the boundary are in range and observable.
        assert!(
            read_range(&store.holding_registers, 4, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn oversized_write_quantity_maps_to_illegal_data_value() {
        let store = InMemoryStore::new(0, 0, MAX_TABLE_LEN, 0).unwrap();
        let error = store
            .handle(
                1,
                ModbusRequest::WriteMultipleRegisters {
                    starting_address: 0,
                    values: vec![0; MAX_TABLE_LEN + 1],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, ExceptionCode::IllegalDataValue);

        // The rejected write must not have touched the table.
        assert_eq!(store.get_holding(0).unwrap(), 0);
        assert_eq!(
            acknowledged_quantity(MAX_TABLE_LEN + 1).unwrap_err(),
            StoreAccessError::QuantityTooLarge {
                quantity: MAX_TABLE_LEN + 1
            }
        );
    }

    #[tokio::test]
    async fn out_of_range_maps_to_illegal_data_address() {
        let store = InMemoryStore::new(1, 1, 1, 1).unwrap();
        let error = store
            .handle(
                1,
                ModbusRequest::ReadHoldingRegisters {
                    starting_address: 1,
                    quantity: 1,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, ExceptionCode::IllegalDataAddress);
    }
}
