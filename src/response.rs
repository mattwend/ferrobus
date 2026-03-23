// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use std::convert::TryFrom;
use std::error::Error;

/// Represents a Modbus response supporting various function codes.
#[derive(Debug, PartialEq)]
pub enum ModbusResponse {
    /// Read Coils response: a vector of coil statuses (each bit represents one coil).
    ReadCoils { coils: Vec<bool> },
    /// Read Discrete Inputs response: a vector of input statuses.
    ReadDiscreteInputs { inputs: Vec<bool> },
    /// Read Holding Registers response: a vector of register values.
    ReadHoldingRegisters { registers: Vec<u16> },
    /// Read Input Registers response: a vector of register values.
    ReadInputRegisters { registers: Vec<u16> },
    /// Write Single Coil response: echo of the address and the coil value.
    WriteSingleCoil { address: u16, value: bool },
    /// Write Single Register response: echo of the address and register value.
    WriteSingleRegister { address: u16, value: u16 },
    /// Write Multiple Coils response: echo of the starting address and quantity of coils written.
    WriteMultipleCoils {
        starting_address: u16,
        quantity: u16,
    },
    /// Write Multiple Registers response: echo of the starting address and quantity of registers written.
    WriteMultipleRegisters {
        starting_address: u16,
        quantity: u16,
    },
    /// Exception response: function code (with error flag) and an exception code.
    Exception { function: u8, code: u8 },
}

/// Deserializes a Modbus response PDU (Protocol Data Unit).
///
/// Note that the PDU should not include the Modbus TCP header if you are working with Modbus TCP.
/// The function assumes that the first byte of the slice is the function code.
///
/// # Arguments
///
/// * `response` - A byte slice containing the Modbus response PDU.
///
/// # Returns
///
/// A `ModbusResponse` enum representing the decoded response.
pub fn deserialize_modbus_response(response: &[u8]) -> Result<ModbusResponse, Box<dyn Error>> {
    if response.is_empty() {
        return Err("Empty response".into());
    }

    let function_code = response[0];
    match function_code {
        // Read Coils (Function code 1):
        1 => {
            if response.len() < 2 {
                return Err("Invalid Read Coils response length".into());
            }
            let byte_count = response[1] as usize;
            if response.len() < 2 + byte_count {
                return Err("Response length does not match byte count".into());
            }
            let coil_bytes = &response[2..2 + byte_count];
            let mut coils = Vec::new();
            // Unpack each bit in each byte (LSB first, as per Modbus spec)
            for byte in coil_bytes {
                for bit in 0..8 {
                    let status = (byte >> bit) & 1 == 1;
                    coils.push(status);
                }
            }
            Ok(ModbusResponse::ReadCoils { coils })
        }
        // Read Discrete Inputs (Function code 2):
        2 => {
            if response.len() < 2 {
                return Err("Invalid Read Discrete Inputs response length".into());
            }
            let byte_count = response[1] as usize;
            if response.len() < 2 + byte_count {
                return Err("Response length does not match byte count".into());
            }
            let input_bytes = &response[2..2 + byte_count];
            let mut inputs = Vec::new();
            for byte in input_bytes {
                for bit in 0..8 {
                    let status = (byte >> bit) & 1 == 1;
                    inputs.push(status);
                }
            }
            Ok(ModbusResponse::ReadDiscreteInputs { inputs })
        }
        // Read Holding Registers (Function code 3):
        3 => {
            if response.len() < 2 {
                return Err("Invalid Read Holding Registers response length".into());
            }
            let byte_count = response[1] as usize;
            if response.len() < 2 + byte_count {
                return Err("Response length does not match byte count".into());
            }
            if !byte_count.is_multiple_of(2) {
                return Err("Byte count is not even for register data".into());
            }
            let mut registers = Vec::new();
            let reg_count = byte_count / 2;
            for i in 0..reg_count {
                let offset = 2 + i * 2;
                let reg = u16::from_be_bytes([response[offset], response[offset + 1]]);
                registers.push(reg);
            }
            Ok(ModbusResponse::ReadHoldingRegisters { registers })
        }
        // Read Input Registers (Function code 4):
        4 => {
            if response.len() < 2 {
                return Err("Invalid Read Input Registers response length".into());
            }
            let byte_count = response[1] as usize;
            if response.len() < 2 + byte_count {
                return Err("Response length does not match byte count".into());
            }
            if !byte_count.is_multiple_of(2) {
                return Err("Byte count is not even for register data".into());
            }
            let mut registers = Vec::new();
            let reg_count = byte_count / 2;
            for i in 0..reg_count {
                let offset = 2 + i * 2;
                let reg = u16::from_be_bytes([response[offset], response[offset + 1]]);
                registers.push(reg);
            }
            Ok(ModbusResponse::ReadInputRegisters { registers })
        }
        // Write Single Coil (Function code 5):
        5 => {
            if response.len() < 5 {
                return Err("Invalid Write Single Coil response length".into());
            }
            let address = u16::from_be_bytes([response[1], response[2]]);
            let coil_value = u16::from_be_bytes([response[3], response[4]]);
            let value = match coil_value {
                0xFF00 => true,
                0x0000 => false,
                _ => return Err("Invalid coil value in Write Single Coil response".into()),
            };
            Ok(ModbusResponse::WriteSingleCoil { address, value })
        }
        // Write Single Register (Function code 6):
        6 => {
            if response.len() < 5 {
                return Err("Invalid Write Single Register response length".into());
            }
            let address = u16::from_be_bytes([response[1], response[2]]);
            let value = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteSingleRegister { address, value })
        }
        // Write Multiple Coils (Function code 15):
        15 => {
            if response.len() < 5 {
                return Err("Invalid Write Multiple Coils response length".into());
            }
            let starting_address = u16::from_be_bytes([response[1], response[2]]);
            let quantity = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteMultipleCoils {
                starting_address,
                quantity,
            })
        }
        // Write Multiple Registers (Function code 16):
        16 => {
            if response.len() < 5 {
                return Err("Invalid Write Multiple Registers response length".into());
            }
            let starting_address = u16::from_be_bytes([response[1], response[2]]);
            let quantity = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteMultipleRegisters {
                starting_address,
                quantity,
            })
        }
        // Exception response: function code with MSB set.
        fc if fc & 0x80 != 0 => {
            if response.len() < 2 {
                return Err("Invalid Exception response length".into());
            }
            let exception_code = response[1];
            Ok(ModbusResponse::Exception {
                function: function_code,
                code: exception_code,
            })
        }
        _ => Err(format!("Unsupported function code: {}", function_code).into()),
    }
}

impl TryFrom<&[u8]> for ModbusResponse {
    type Error = Box<dyn Error>;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        deserialize_modbus_response(bytes)
    }
}

impl TryFrom<Vec<u8>> for ModbusResponse {
    type Error = Box<dyn Error>;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        ModbusResponse::try_from(bytes.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_read_coils() {
        // Construct a Read Coils response:
        // Function code (1), Byte count (1), Coil data: 0x25 (0010 0101).
        let response = vec![1u8, 1u8, 0x25];
        // Expected coils (LSB first):
        // bit0: 1, bit1: 0, bit2: 1, bit3: 0, bit4: 0, bit5: 1, bit6: 0, bit7: 0.
        let expected_coils = vec![true, false, true, false, false, true, false, false];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadCoils {
                coils: expected_coils
            }
        );
    }

    #[test]
    fn test_deserialize_read_discrete_inputs() {
        // Construct a Read Discrete Inputs response:
        // Function code (2), Byte count (1), Input data: 0xAA (10101010).
        let response = vec![2u8, 1u8, 0xAA];
        // For 0xAA (binary 10101010) and LSB-first ordering:
        // bit0: 0, bit1: 1, bit2: 0, bit3: 1, bit4: 0, bit5: 1, bit6: 0, bit7: 1.
        let expected_inputs = vec![false, true, false, true, false, true, false, true];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadDiscreteInputs {
                inputs: expected_inputs
            }
        );
    }

    #[test]
    fn test_deserialize_read_holding_registers() {
        // Construct a Read Holding Registers response:
        // Function code (3), Byte count (4), Registers: 0x0102, 0x0304.
        let response = vec![3u8, 4u8, 0x01, 0x02, 0x03, 0x04];
        let expected_registers = vec![0x0102, 0x0304];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadHoldingRegisters {
                registers: expected_registers
            }
        );
    }

    #[test]
    fn test_deserialize_read_input_registers() {
        // Construct a Read Input Registers response:
        // Function code (4), Byte count (2), Register: 0xABCD.
        let response = vec![4u8, 2u8, 0xAB, 0xCD];
        let expected_registers = vec![0xABCD];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadInputRegisters {
                registers: expected_registers
            }
        );
    }

    #[test]
    fn test_deserialize_write_single_coil_true() {
        // Construct a Write Single Coil response:
        // Function code (5), Address: 0x0010, Coil value: 0xFF00 (ON).
        let response = vec![5u8, 0x00, 0x10, 0xFF, 0x00];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteSingleCoil {
                address: 0x0010,
                value: true
            }
        );
    }

    #[test]
    fn test_deserialize_write_single_coil_false() {
        // Construct a Write Single Coil response:
        // Function code (5), Address: 0x0010, Coil value: 0x0000 (OFF).
        let response = vec![5u8, 0x00, 0x10, 0x00, 0x00];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteSingleCoil {
                address: 0x0010,
                value: false
            }
        );
    }

    #[test]
    fn test_deserialize_write_single_register() {
        // Construct a Write Single Register response:
        // Function code (6), Address: 0x0010, Value: 0x1234.
        let response = vec![6u8, 0x00, 0x10, 0x12, 0x34];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteSingleRegister {
                address: 0x0010,
                value: 0x1234
            }
        );
    }

    #[test]
    fn test_deserialize_write_multiple_coils() {
        // Construct a Write Multiple Coils response:
        // Function code (15), Starting address: 0x0001, Quantity: 0x0006.
        let response = vec![15u8, 0x00, 0x01, 0x00, 0x06];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteMultipleCoils {
                starting_address: 0x0001,
                quantity: 0x0006
            }
        );
    }

    #[test]
    fn test_deserialize_write_multiple_registers() {
        // Construct a Write Multiple Registers response:
        // Function code (16), Starting address: 0x0001, Quantity: 0x0002.
        let response = vec![16u8, 0x00, 0x01, 0x00, 0x02];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteMultipleRegisters {
                starting_address: 0x0001,
                quantity: 0x0002
            }
        );
    }

    #[test]
    fn test_deserialize_exception() {
        // Construct an Exception response:
        // For example, function code 0x81 (exception for function code 1) and exception code 0x02.
        let response = vec![0x81u8, 0x02];
        let result = deserialize_modbus_response(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::Exception {
                function: 0x81,
                code: 0x02
            }
        );
    }

    #[test]
    fn test_invalid_response_empty() {
        let response = vec![];
        let result = deserialize_modbus_response(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_response_too_short_for_registers() {
        // Function code 3 requires at least 2 bytes after the function code (byte count and then register data).
        let response = vec![3u8, 1u8]; // Too short.
        let result = deserialize_modbus_response(&response);
        assert!(result.is_err());
    }
}
