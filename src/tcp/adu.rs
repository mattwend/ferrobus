// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use crate::error::ModbusError;
use crate::request::{ModbusRequest, serialize_modbus_request};

/// Builds a Modbus TCP frame by constructing the 7-byte MBAP header
/// and appending the Modbus PDU.
///
/// The MBAP header consists of:
/// - Transaction Identifier (2 bytes)
/// - Protocol Identifier (2 bytes, always 0)
/// - Length (2 bytes: the number of remaining bytes, i.e. Unit Identifier + PDU)
/// - Unit Identifier (1 byte)
///
/// # Arguments
/// * `transaction_id` - A unique transaction identifier for matching requests/replies.
/// * `unit_id` - The unit identifier of the remote slave device.
/// * `pdu` - The ModbusRequest PDU struct.
///
/// # Returns
/// A vector of bytes containing the complete Modbus TCP frame.
pub fn build_modbus_tcp_adu(
    transaction_id: u16,
    unit_id: u8,
    pdu: &ModbusRequest,
) -> Result<Vec<u8>, ModbusError> {
    let pdu = serialize_modbus_request(pdu)?;
    tracing::debug!("PDU: {:02X?}", pdu);

    let mut frame = Vec::with_capacity(7 + pdu.len());
    frame.extend_from_slice(&transaction_id.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&((1 + pdu.len()) as u16).to_be_bytes());
    frame.push(unit_id);
    frame.extend_from_slice(&pdu);

    Ok(frame)
}
