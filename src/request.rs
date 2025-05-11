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
use std::error::Error;

/// Represents a Modbus request supporting various function codes.
#[derive(Debug, PartialEq)]
pub enum ModbusRequest {
    /// Read Coils (Function code 1)
    ReadCoils {
        starting_address: u16,
        quantity: u16,
    },
    /// Read Discrete Inputs (Function code 2)
    ReadDiscreteInputs {
        starting_address: u16,
        quantity: u16,
    },
    /// Read Holding Registers (Function code 3)
    ReadHoldingRegisters {
        starting_address: u16,
        quantity: u16,
    },
    /// Read Input Registers (Function code 4)
    ReadInputRegisters {
        starting_address: u16,
        quantity: u16,
    },
    /// Write Single Coil (Function code 5)
    WriteSingleCoil { address: u16, value: bool },
    /// Write Single Register (Function code 6)
    WriteSingleRegister { address: u16, value: u16 },
    /// Write Multiple Coils (Function code 15)
    WriteMultipleCoils {
        starting_address: u16,
        values: Vec<bool>,
    },
    /// Write Multiple Registers (Function code 16)
    WriteMultipleRegisters {
        starting_address: u16,
        values: Vec<u16>,
    },
}

/// Helper function that packs a slice of boolean coil values into bytes,
/// with one bit per coil. The first coil corresponds to the least significant
/// bit of the first byte.
fn pack_coils(coils: &[bool]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut current_byte = 0;
    let mut bit_index = 0;
    for &coil in coils {
        if coil {
            current_byte |= 1 << bit_index;
        }
        bit_index += 1;
        if bit_index == 8 {
            bytes.push(current_byte);
            current_byte = 0;
            bit_index = 0;
        }
    }
    if bit_index > 0 {
        bytes.push(current_byte);
    }
    bytes
}

/// Encodes a Modbus request into a Modbus PDU (Protocol Data Unit) using bincode
/// with fixed-length integer encoding and big-endian (network) byte order.
///
/// The resulting byte vector starts with the function code followed by the
/// encoded payload as defined by the Modbus specification.
///
/// # Arguments
///
/// * `pdu` - A reference to the `ModbusRequest` variant.
///
/// # Returns
///
/// A vector of bytes containing the encoded Modbus PDU.
pub fn serialize_modbus_request(pdu: &ModbusRequest) -> Result<Vec<u8>, Box<dyn Error>> {
    // Use a bincode configuration that ensures fixed-length encoding and big-endian order.
    let config = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_big_endian();

    let mut frame = Vec::new();
    match pdu {
        ModbusRequest::ReadCoils {
            starting_address,
            quantity,
        } => {
            // Function code 1.
            frame.push(1u8);
            let payload = config.serialize(&(*starting_address, *quantity))?;
            frame.extend_from_slice(&payload);
        }
        ModbusRequest::ReadDiscreteInputs {
            starting_address,
            quantity,
        } => {
            // Function code 2.
            frame.push(2u8);
            let payload = config.serialize(&(*starting_address, *quantity))?;
            frame.extend_from_slice(&payload);
        }
        ModbusRequest::ReadHoldingRegisters {
            starting_address,
            quantity,
        } => {
            // Function code 3.
            frame.push(3u8);
            let payload = config.serialize(&(*starting_address, *quantity))?;
            frame.extend_from_slice(&payload);
        }
        ModbusRequest::ReadInputRegisters {
            starting_address,
            quantity,
        } => {
            // Function code 4.
            frame.push(4u8);
            let payload = config.serialize(&(*starting_address, *quantity))?;
            frame.extend_from_slice(&payload);
        }
        ModbusRequest::WriteSingleCoil { address, value } => {
            // Function code 5.
            frame.push(5u8);
            // Per Modbus spec, true (ON) is 0xFF00 and false (OFF) is 0x0000.
            let coil_value: u16 = if *value { 0xFF00 } else { 0x0000 };
            let payload = config.serialize(&(*address, coil_value))?;
            frame.extend_from_slice(&payload);
        }
        ModbusRequest::WriteSingleRegister { address, value } => {
            // Function code 6.
            frame.push(6u8);
            let payload = config.serialize(&(*address, *value))?;
            frame.extend_from_slice(&payload);
        }
        ModbusRequest::WriteMultipleCoils {
            starting_address,
            values,
        } => {
            // Function code 15.
            frame.push(15u8);
            let quantity = values.len() as u16;
            let coil_bytes = pack_coils(values);
            let byte_count = coil_bytes.len() as u8;
            let header = config.serialize(&(*starting_address, quantity))?;
            frame.extend_from_slice(&header);
            frame.push(byte_count);
            frame.extend_from_slice(&coil_bytes);
        }
        ModbusRequest::WriteMultipleRegisters {
            starting_address,
            values,
        } => {
            // Function code 16.
            frame.push(16u8);
            let quantity = values.len() as u16;
            let byte_count = (quantity * 2) as u8;
            let header = config.serialize(&(*starting_address, quantity))?;
            frame.extend_from_slice(&header);
            frame.push(byte_count);
            // Serialize each register value (u16).
            for reg in values {
                let reg_bytes = config.serialize(reg)?;
                frame.extend_from_slice(&reg_bytes);
            }
        }
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_coils() {
        let pdu = ModbusRequest::ReadCoils {
            starting_address: 0x0010,
            quantity: 0x000A,
        };

        // Expected: function code (1) then two u16 values in big-endian.
        // starting_address 0x0010 -> [0x00, 0x10]
        // quantity 0x000A -> [0x00, 0x0A]
        let expected = vec![1u8, 0x00, 0x10, 0x00, 0x0A];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_discrete_inputs() {
        let pdu = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0020,
            quantity: 0x0005,
        };

        // Expected: function code (2) then header.
        let expected = vec![2u8, 0x00, 0x20, 0x00, 0x05];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_holding_registers() {
        let pdu = ModbusRequest::ReadHoldingRegisters {
            starting_address: 0x0100,
            quantity: 0x0003,
        };

        let expected = vec![3u8, 0x01, 0x00, 0x00, 0x03];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_input_registers() {
        let pdu = ModbusRequest::ReadInputRegisters {
            starting_address: 0x00FF,
            quantity: 0x0001,
        };

        let expected = vec![4u8, 0x00, 0xFF, 0x00, 0x01];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_coil_on() {
        let pdu = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: true,
        };

        // Function code (5), then address (0x0010), then coil value (true -> 0xFF00).
        let expected = vec![5u8, 0x00, 0x10, 0xFF, 0x00];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_coil_off() {
        let pdu = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: false,
        };

        // Function code (5), then address (0x0010), then coil value (false -> 0x0000).
        let expected = vec![5u8, 0x00, 0x10, 0x00, 0x00];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_register() {
        let pdu = ModbusRequest::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };

        // Function code (6), then address (0x0010), then value (0x1234).
        let expected = vec![6u8, 0x00, 0x10, 0x12, 0x34];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_multiple_coils() {
        let coils = vec![true, false, true, false, false, true]; // 6 coils
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0001,
            values: coils,
        };

        // Header: starting_address (0x0001) -> [0x00, 0x01], quantity (6) -> [0x00, 0x06]
        // pack_coils on [true, false, true, false, false, true] produces:
        // bit0: 1, bit1: 0, bit2: 1, bit3: 0, bit4: 0, bit5: 1 => 0x25 (0010 0101)
        // byte count: 1 byte.
        // Function code: 15.
        let expected = vec![
            15u8, // function code
            0x00, 0x01, // starting_address
            0x00, 0x06, // quantity
            0x01, // byte count
            0x25, // coil byte
        ];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_multiple_registers() {
        let values = vec![0x1111, 0x2222];
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0001,
            values: values,
        };

        // Header: starting_address (0x0001) -> [0x00, 0x01], quantity (2) -> [0x00, 0x02]
        // Byte count: 2 registers * 2 bytes each = 4.
        // Each register is serialized as u16 in big-endian.
        // Function code: 16.
        let expected = vec![
            16u8, // function code
            0x00, 0x01, // starting_address
            0x00, 0x02, // quantity
            0x04, // byte count
            0x11, 0x11, // first register
            0x22, 0x22, // second register
        ];
        let result = serialize_modbus_request(&pdu).unwrap();
        assert_eq!(result, expected);
    }
}
