// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Application-layer flow control for Modbus TCP actors.

use std::time::Duration;

use crate::error::ModbusError;
use crate::tcp::actor::COMMAND_CHANNEL_CAPACITY;

/// Application-layer flow control for one Modbus TCP transport actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModbusTcpFlowControl {
    /// Maximum number of transactions on the wire at once. Must be >= 1.
    pub max_in_flight: usize,
    /// Maximum number of admitted-but-not-yet-on-wire requests buffered in the actor.
    pub max_queue_depth: usize,
    /// Maximum time a request may wait between submission and reaching the wire.
    pub queue_timeout: Duration,
    /// Time a timed-out/cancelled transaction id remains reserved.
    pub quarantine_ttl: Duration,
}

impl ModbusTcpFlowControl {
    /// Preset for serial-backed gateways where the downstream bus is sequential.
    #[must_use]
    pub fn serial_gateway() -> Self {
        Self { max_in_flight: 1, ..Self::default() }
    }

    /// Validates flow-control invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] if any numeric bound is zero.
    pub fn validate(&self) -> Result<(), ModbusError> {
        if self.max_in_flight == 0 {
            return Err(ModbusError::ValidationError("flow_control max_in_flight must be >= 1".to_string()));
        }
        if self.max_queue_depth == 0 {
            return Err(ModbusError::ValidationError("flow_control max_queue_depth must be >= 1".to_string()));
        }
        if self.queue_timeout.is_zero() {
            return Err(ModbusError::ValidationError("flow_control queue_timeout must be > 0".to_string()));
        }
        if self.quarantine_ttl.is_zero() {
            return Err(ModbusError::ValidationError("flow_control quarantine_ttl must be > 0".to_string()));
        }
        Ok(())
    }
}

impl Default for ModbusTcpFlowControl {
    fn default() -> Self {
        Self {
            max_in_flight: 16,
            max_queue_depth: COMMAND_CHANNEL_CAPACITY,
            queue_timeout: Duration::from_secs(5),
            quarantine_ttl: Duration::from_secs(10),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_serial_gateway_are_documented() {
        assert_eq!(ModbusTcpFlowControl::default().max_in_flight, 16);
        assert_eq!(ModbusTcpFlowControl::serial_gateway().max_in_flight, 1);
    }

    #[test]
    fn validate_rejects_invalid_values() {
        assert!(ModbusTcpFlowControl { max_in_flight: 0, ..Default::default() }.validate().is_err());
        assert!(ModbusTcpFlowControl { max_queue_depth: 0, ..Default::default() }.validate().is_err());
        assert!(ModbusTcpFlowControl { queue_timeout: Duration::ZERO, ..Default::default() }.validate().is_err());
        assert!(ModbusTcpFlowControl { quarantine_ttl: Duration::ZERO, ..Default::default() }.validate().is_err());
        assert!(ModbusTcpFlowControl::default().validate().is_ok());
    }
}
