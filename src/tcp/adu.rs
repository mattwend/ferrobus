// tinymb - A simple Modbus library for Rust
// Copyright (C) 2025 tinymb authors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use bincode::Options;
use serde::Serialize;
use std::error::Error;

use crate::request::{serialize_modbus_request, ModbusRequest};

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
