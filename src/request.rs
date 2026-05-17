// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use crate::error::ModbusError;

const MAX_READ_COILS: u16 = 0x07D0;
const MAX_READ_DISCRETE_INPUTS: u16 = 0x07D0;
const MAX_READ_HOLDING_REGISTERS: u16 = 0x007D;
const MAX_READ_INPUT_REGISTERS: u16 = 0x007D;
const MAX_WRITE_MULTIPLE_COILS: u16 = 0x07B0;
const MAX_WRITE_MULTIPLE_REGISTERS: u16 = 0x007B;

/// Typed Modbus request PDUs.
#[derive(Debug, Clone, PartialEq)]
pub enum ModbusRequest {
    ReadCoils {
        starting_address: u16,
        quantity: u16,
    },
    ReadDiscreteInputs {
        starting_address: u16,
        quantity: u16,
    },
    ReadHoldingRegisters {
        starting_address: u16,
        quantity: u16,
    },
    ReadInputRegisters {
        starting_address: u16,
        quantity: u16,
    },
    WriteSingleCoil {
        address: u16,
        value: bool,
    },
    WriteSingleRegister {
        address: u16,
        value: u16,
    },
    WriteMultipleCoils {
        starting_address: u16,
        values: Vec<bool>,
    },
    WriteMultipleRegisters {
        starting_address: u16,
        values: Vec<u16>,
    },
}

impl ModbusRequest {
    fn validate(&self) -> Result<(), ModbusError> {
        match self {
            ModbusRequest::ReadCoils { quantity, .. } => {
                if *quantity == 0 || *quantity > MAX_READ_COILS {
                    return Err(ModbusError::ValidationError(format!(
                        "ReadCoils quantity must be 1-{}, got {}",
                        MAX_READ_COILS, quantity
                    )));
                }
            }
            ModbusRequest::ReadDiscreteInputs { quantity, .. } => {
                if *quantity == 0 || *quantity > MAX_READ_DISCRETE_INPUTS {
                    return Err(ModbusError::ValidationError(format!(
                        "ReadDiscreteInputs quantity must be 1-{}, got {}",
                        MAX_READ_DISCRETE_INPUTS, quantity
                    )));
                }
            }
            ModbusRequest::ReadHoldingRegisters { quantity, .. } => {
                if *quantity == 0 || *quantity > MAX_READ_HOLDING_REGISTERS {
                    return Err(ModbusError::ValidationError(format!(
                        "ReadHoldingRegisters quantity must be 1-{}, got {}",
                        MAX_READ_HOLDING_REGISTERS, quantity
                    )));
                }
            }
            ModbusRequest::ReadInputRegisters { quantity, .. } => {
                if *quantity == 0 || *quantity > MAX_READ_INPUT_REGISTERS {
                    return Err(ModbusError::ValidationError(format!(
                        "ReadInputRegisters quantity must be 1-{}, got {}",
                        MAX_READ_INPUT_REGISTERS, quantity
                    )));
                }
            }
            ModbusRequest::WriteMultipleCoils { values, .. } => {
                let qty = u16::try_from(values.len()).map_err(|_| {
                    ModbusError::ValidationError(format!(
                        "WriteMultipleCoils quantity must be 1-{}, got {}",
                        MAX_WRITE_MULTIPLE_COILS,
                        values.len()
                    ))
                })?;
                if qty == 0 || qty > MAX_WRITE_MULTIPLE_COILS {
                    return Err(ModbusError::ValidationError(format!(
                        "WriteMultipleCoils quantity must be 1-{}, got {}",
                        MAX_WRITE_MULTIPLE_COILS, qty
                    )));
                }
            }
            ModbusRequest::WriteMultipleRegisters { values, .. } => {
                let qty = u16::try_from(values.len()).map_err(|_| {
                    ModbusError::ValidationError(format!(
                        "WriteMultipleRegisters quantity must be 1-{}, got {}",
                        MAX_WRITE_MULTIPLE_REGISTERS,
                        values.len()
                    ))
                })?;
                if qty == 0 || qty > MAX_WRITE_MULTIPLE_REGISTERS {
                    return Err(ModbusError::ValidationError(format!(
                        "WriteMultipleRegisters quantity must be 1-{}, got {}",
                        MAX_WRITE_MULTIPLE_REGISTERS, qty
                    )));
                }
            }
            ModbusRequest::WriteSingleCoil { .. } | ModbusRequest::WriteSingleRegister { .. } => {}
        }
        Ok(())
    }

    /// Serializes the request into a Modbus PDU.
    ///
    /// Validation runs before serialization, so protocol-limit violations are
    /// returned as [`ModbusError::ValidationError`].
    pub fn serialize(&self) -> Result<Vec<u8>, ModbusError> {
        self.validate()?;
        Ok(serialize_modbus_request_internal(self))
    }
}

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

fn serialize_modbus_request_internal(pdu: &ModbusRequest) -> Vec<u8> {
    let mut frame = Vec::new();
    match pdu {
        ModbusRequest::ReadCoils {
            starting_address,
            quantity,
        }
        | ModbusRequest::ReadDiscreteInputs {
            starting_address,
            quantity,
        }
        | ModbusRequest::ReadHoldingRegisters {
            starting_address,
            quantity,
        }
        | ModbusRequest::ReadInputRegisters {
            starting_address,
            quantity,
        } => {
            let function_code = match pdu {
                ModbusRequest::ReadCoils { .. } => 1,
                ModbusRequest::ReadDiscreteInputs { .. } => 2,
                ModbusRequest::ReadHoldingRegisters { .. } => 3,
                ModbusRequest::ReadInputRegisters { .. } => 4,
                _ => unreachable!(),
            };
            frame.push(function_code);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::WriteSingleCoil { address, value } => {
            frame.push(5u8);
            frame.extend_from_slice(&address.to_be_bytes());
            let coil_value: u16 = if *value { 0xFF00 } else { 0x0000 };
            frame.extend_from_slice(&coil_value.to_be_bytes());
        }
        ModbusRequest::WriteSingleRegister { address, value } => {
            frame.push(6u8);
            frame.extend_from_slice(&address.to_be_bytes());
            frame.extend_from_slice(&value.to_be_bytes());
        }
        ModbusRequest::WriteMultipleCoils {
            starting_address,
            values,
        } => {
            debug_assert!(values.len() <= usize::from(MAX_WRITE_MULTIPLE_COILS));

            frame.push(15u8);
            let quantity = values.len() as u16;
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
            let coil_bytes = pack_coils(values);
            let coil_byte_count = coil_bytes.len() as u8;
            frame.push(coil_byte_count);
            frame.extend_from_slice(&coil_bytes);
        }
        ModbusRequest::WriteMultipleRegisters {
            starting_address,
            values,
        } => {
            debug_assert!(values.len() <= usize::from(MAX_WRITE_MULTIPLE_REGISTERS));

            frame.push(16u8);
            let quantity = values.len() as u16;
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
            let byte_count = (values.len() * 2) as u8;
            frame.push(byte_count);
            for reg in values {
                frame.extend_from_slice(&reg.to_be_bytes());
            }
        }
    }
    frame
}

#[must_use]
/// Serializes a request without validation.
///
/// Prefer [`ModbusRequest::serialize`] when accepting user input or external data.
pub fn serialize_modbus_request(pdu: &ModbusRequest) -> Vec<u8> {
    serialize_modbus_request_internal(pdu)
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

        let expected = vec![1u8, 0x00, 0x10, 0x00, 0x0A];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_discrete_inputs() {
        let pdu = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0020,
            quantity: 0x0005,
        };

        let expected = vec![2u8, 0x00, 0x20, 0x00, 0x05];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_holding_registers() {
        let pdu = ModbusRequest::ReadHoldingRegisters {
            starting_address: 0x0100,
            quantity: 0x0003,
        };

        let expected = vec![3u8, 0x01, 0x00, 0x00, 0x03];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_input_registers() {
        let pdu = ModbusRequest::ReadInputRegisters {
            starting_address: 0x00FF,
            quantity: 0x0001,
        };

        let expected = vec![4u8, 0x00, 0xFF, 0x00, 0x01];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_coil_on() {
        let pdu = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: true,
        };

        let expected = vec![5u8, 0x00, 0x10, 0xFF, 0x00];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_coil_off() {
        let pdu = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: false,
        };

        let expected = vec![5u8, 0x00, 0x10, 0x00, 0x00];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_register() {
        let pdu = ModbusRequest::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };

        let expected = vec![6u8, 0x00, 0x10, 0x12, 0x34];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_multiple_coils() {
        let coils = vec![true, false, true, false, false, true];
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0001,
            values: coils,
        };

        let expected = vec![15u8, 0x00, 0x01, 0x00, 0x06, 0x01, 0x25];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_multiple_registers() {
        let values = vec![0x1111, 0x2222];
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0001,
            values,
        };

        let expected = vec![16u8, 0x00, 0x01, 0x00, 0x02, 0x04, 0x11, 0x11, 0x22, 0x22];
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_pack_coils_empty() {
        let bytes = pack_coils(&[]);
        assert!(bytes.is_empty());
    }

    #[test]
    fn test_pack_coils_single_coil() {
        let bytes = pack_coils(&[true]);
        assert_eq!(bytes, &[0x01]);
    }

    #[test]
    fn test_pack_coils_exactly_eight() {
        let coils = vec![true, false, true, false, true, false, true, false];
        let bytes = pack_coils(&coils);
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes[0], 0x55);
    }

    #[test]
    fn test_pack_coils_nine_bits() {
        let coils = vec![true; 9];
        let bytes = pack_coils(&coils);
        assert_eq!(bytes.len(), 2);
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1], 0x01);
    }

    #[test]
    fn test_pack_coils_alternating() {
        let bytes = pack_coils(&[
            true, false, true, false, true, false, true, false, true, false,
        ]);
        assert_eq!(bytes.len(), 2);
        assert_eq!(bytes[0], 0x55);
        assert_eq!(bytes[1], 0x01);
    }

    #[test]
    fn test_write_multiple_coils_exactly_8_coils() {
        let coils = vec![true, false, true, false, true, false, true, false];
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: coils,
        };
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result[5], 1);
        assert_eq!(result[6], 0x55);
    }

    #[test]
    fn test_write_multiple_coils_9_coils() {
        let coils = vec![true; 9];
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: coils,
        };
        let result = serialize_modbus_request(&pdu);
        assert_eq!(result[5], 2);
        assert_eq!(result[6], 0xFF);
        assert_eq!(result[7], 0x01);
    }

    #[test]
    fn test_pack_coils_all_false() {
        let bytes = pack_coils(&[false, false, false, false]);
        assert_eq!(bytes, &[0x00]);
    }

    #[test]
    fn test_write_multiple_registers_empty() {
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0001,
            values: vec![],
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_write_multiple_coils_empty() {
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0001,
            values: vec![],
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_read_coils_zero_quantity() {
        let pdu = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 0,
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_read_coils_exceeds_max() {
        let pdu = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 0x07D1,
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_read_holding_registers_exceeds_max() {
        let pdu = ModbusRequest::ReadHoldingRegisters {
            starting_address: 0x0000,
            quantity: 0x007E,
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_write_multiple_coils_exceeds_max() {
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: vec![true; 1969],
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_write_multiple_registers_exceeds_max() {
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0000,
            values: vec![0x0000; 124],
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_write_multiple_coils_len_overflows_u16() {
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: vec![true; usize::from(u16::MAX) + 1],
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }

    #[test]
    fn test_write_multiple_registers_len_overflows_u16() {
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0000,
            values: vec![0x0000; usize::from(u16::MAX) + 1],
        };
        let result = pdu.serialize();
        assert!(result.is_err());
    }
}
