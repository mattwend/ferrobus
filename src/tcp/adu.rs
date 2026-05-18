// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use crate::error::ModbusError;
use crate::request::{ModbusRequest, serialize_modbus_request};

const MBAP_HEADER_LEN: usize = 7;

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
/// * `pdu` - The [`ModbusRequest`] PDU struct.
///
/// # Returns
/// A vector of bytes containing the complete Modbus TCP frame.
///
/// # Errors
/// Returns a [`ModbusError`] if the request cannot be serialized or the
/// resulting frame would exceed the Modbus TCP length field.
pub fn build_modbus_tcp_adu(
    transaction_id: u16,
    unit_id: u8,
    pdu: &ModbusRequest,
) -> Result<Vec<u8>, ModbusError> {
    let pdu = serialize_modbus_request(pdu)?;
    let length = u16::try_from(1 + pdu.len()).map_err(|_| {
        ModbusError::ValidationError(format!(
            "Modbus TCP ADU length is too large: {}",
            1 + pdu.len()
        ))
    })?;
    tracing::debug!("PDU: {:02X?}", pdu);

    let mut frame = Vec::with_capacity(MBAP_HEADER_LEN + pdu.len());
    frame.extend_from_slice(&transaction_id.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(unit_id);
    frame.extend_from_slice(&pdu);

    Ok(frame)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn build_modbus_tcp_adu_read_coils() {
        let pdu = ModbusRequest::ReadCoils {
            starting_address: 0x0010,
            quantity: 0x000A,
        };
        let frame = build_modbus_tcp_adu(0x0001, 0x11, &pdu).unwrap();

        let expected = [
            0x00, 0x01, // transaction_id
            0x00, 0x00, // protocol_id
            0x00, 0x06, // length (unit_id + pdu)
            0x11, // unit_id
            0x01, // function code
            0x00, 0x10, // starting_address
            0x00, 0x0A, // quantity
        ];
        assert_eq!(frame.len(), expected.len());
        assert_eq!(&frame[..], &expected);
    }

    #[test]
    fn build_modbus_tcp_adu_write_single_coil() {
        let pdu = ModbusRequest::WriteSingleCoil {
            address: 0x00FF,
            value: true,
        };
        let frame = build_modbus_tcp_adu(0x1234, 0x01, &pdu).unwrap();

        assert_eq!(frame.len(), 12);
        assert_eq!([frame[0], frame[1]], [0x12, 0x34]);
        assert_eq!([frame[2], frame[3]], [0x00, 0x00]);
        assert_eq!([frame[4], frame[5]], [0x00, 0x06]);
        assert_eq!(frame[6], 0x01);
        assert_eq!(&frame[7..], &[5u8, 0x00, 0xFF, 0xFF, 0x00]);
    }

    #[test]
    fn build_modbus_tcp_adu_write_multiple_registers() {
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0001,
            values: vec![0x1111, 0x2222],
        };
        let frame = build_modbus_tcp_adu(0xFFFF, 0xFF, &pdu).unwrap();

        let expected = [
            0xFF, 0xFF, // transaction_id
            0x00, 0x00, // protocol_id
            0x00, 0x0B, // length (unit_id + pdu = 1 + 10)
            0xFF, // unit_id
            0x10, // function code
            0x00, 0x01, // starting_address
            0x00, 0x02, // quantity
            0x04, // byte count
            0x11, 0x11, // first register
            0x22, 0x22, // second register
        ];
        assert_eq!(frame.len(), expected.len());
        assert_eq!(&frame[..], &expected);
    }

    #[test]
    fn build_modbus_tcp_adu_length_field_correct() {
        let pdu = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 0x0001,
        };
        let frame = build_modbus_tcp_adu(1, 1, &pdu).unwrap();
        let declared_length = u16::from_be_bytes([frame[4], frame[5]]);
        assert_eq!(declared_length, 6);
    }
}
