// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use bincode::Options;
use serde::Serialize;
use std::error::Error;

use crate::request::{ModbusRequest, serialize_modbus_request};

/// A Modbus TCP header used to build a Modbus TCP frame.
///
/// The header consists of:
/// - Transaction Identifier (2 bytes)
/// - Protocol Identifier (2 bytes, always 0)
/// - Length (2 bytes: the number of remaining bytes, i.e. Unit Identifier + PDU)
/// - Unit Identifier (1 byte)
#[derive(Serialize, Debug)]
struct ModbusTcpHeader {
    /// A unique transaction identifier for matching requests/replies.
    transaction_id: u16,
    /// The Modbus protocol identifier (always 0).
    protocol_id: u16,
    /// The length of the remaining bytes in the frame (Unit Identifier + PDU).
    length: u16,
    /// The unit identifier of the remote slave device.
    unit_id: u8,
}

/// Builds a Modbus TCP frame by serializing the header using bincode
/// and appending the Modbus PDU.
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
) -> Result<Vec<u8>, Box<dyn Error>> {
    let config = bincode::config::DefaultOptions::new()
        .with_fixint_encoding()
        .with_big_endian();

    let pdu = serialize_modbus_request(pdu)?;
    tracing::debug!("PDU: {:02X?}", pdu);

    // The length field equals 1 byte for the unit identifier plus the PDU length.
    let header = ModbusTcpHeader {
        transaction_id,
        protocol_id: 0,
        length: (1 + pdu.len()) as u16,
        unit_id,
    };

    // Serialize the header using bincode with fixed-length encoding and big-endian.
    let mut frame = config.serialize(&header)?;
    frame.extend_from_slice(&pdu);

    //frame.extend_from_slice(&pdu);
    Ok(frame)
}
