// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

use crate::error::ModbusError;

const MAX_READ_COILS: u16 = 0x07D0;
const MAX_READ_DISCRETE_INPUTS: u16 = 0x07D0;
const MAX_READ_HOLDING_REGISTERS: u16 = 0x007D;
const MAX_READ_INPUT_REGISTERS: u16 = 0x007D;
const MAX_WRITE_MULTIPLE_COILS: u16 = 0x07B0;
const MAX_WRITE_MULTIPLE_REGISTERS: u16 = 0x007B;
const MAX_REQUEST_PDU_LEN: usize = 252;

/// Typed Modbus request PDUs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModbusRequest {
    /// Read coil outputs starting at `starting_address`.
    ReadCoils {
        /// Zero-based starting address.
        starting_address: u16,
        /// Number of coils to read.
        quantity: u16,
    },
    /// Read discrete inputs starting at `starting_address`.
    ReadDiscreteInputs {
        /// Zero-based starting address.
        starting_address: u16,
        /// Number of inputs to read.
        quantity: u16,
    },
    /// Read holding registers starting at `starting_address`.
    ReadHoldingRegisters {
        /// Zero-based starting address.
        starting_address: u16,
        /// Number of registers to read.
        quantity: u16,
    },
    /// Read input registers starting at `starting_address`.
    ReadInputRegisters {
        /// Zero-based starting address.
        starting_address: u16,
        /// Number of registers to read.
        quantity: u16,
    },
    /// Write one coil output.
    WriteSingleCoil {
        /// Coil address to write.
        address: u16,
        /// Coil value to write.
        value: bool,
    },
    /// Write one holding register.
    WriteSingleRegister {
        /// Register address to write.
        address: u16,
        /// Register value to write.
        value: u16,
    },
    /// Write multiple coil outputs starting at `starting_address`.
    WriteMultipleCoils {
        /// Zero-based starting address.
        starting_address: u16,
        /// Coil values to write.
        values: Vec<bool>,
    },
    /// Write multiple holding registers starting at `starting_address`.
    WriteMultipleRegisters {
        /// Zero-based starting address.
        starting_address: u16,
        /// Register values to write.
        values: Vec<u16>,
    },
}

impl ModbusRequest {
    fn validate(&self) -> Result<(), ModbusError> {
        match self {
            ModbusRequest::ReadCoils { quantity, .. } => {
                validate_quantity("ReadCoils", *quantity, MAX_READ_COILS)?;
            }
            ModbusRequest::ReadDiscreteInputs { quantity, .. } => {
                validate_quantity("ReadDiscreteInputs", *quantity, MAX_READ_DISCRETE_INPUTS)?;
            }
            ModbusRequest::ReadHoldingRegisters { quantity, .. } => {
                validate_quantity(
                    "ReadHoldingRegisters",
                    *quantity,
                    MAX_READ_HOLDING_REGISTERS,
                )?;
            }
            ModbusRequest::ReadInputRegisters { quantity, .. } => {
                validate_quantity("ReadInputRegisters", *quantity, MAX_READ_INPUT_REGISTERS)?;
            }
            ModbusRequest::WriteMultipleCoils { values, .. } => {
                validate_values_len("WriteMultipleCoils", values.len(), MAX_WRITE_MULTIPLE_COILS)?;
            }
            ModbusRequest::WriteMultipleRegisters { values, .. } => {
                validate_values_len(
                    "WriteMultipleRegisters",
                    values.len(),
                    MAX_WRITE_MULTIPLE_REGISTERS,
                )?;
            }
            ModbusRequest::WriteSingleCoil { .. } | ModbusRequest::WriteSingleRegister { .. } => {}
        }
        Ok(())
    }

    /// Serializes the request into a Modbus PDU.
    ///
    /// Validation runs before serialization, so protocol-limit violations are
    /// returned as [`ModbusError::ValidationError`].
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] when the request violates Modbus limits.
    pub(crate) fn serialize(&self) -> Result<Vec<u8>, ModbusError> {
        self.validate()?;
        serialize_modbus_request(self)
    }
}

fn pack_coils(coils: &[bool]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(coils.len().div_ceil(8));
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

fn validate_quantity(name: &str, quantity: u16, max: u16) -> Result<(), ModbusError> {
    if quantity == 0 || quantity > max {
        return Err(ModbusError::ValidationError(format!(
            "{name} quantity must be 1-{max}, got {quantity}"
        )));
    }
    Ok(())
}

fn validate_values_len(name: &str, len: usize, max: u16) -> Result<u16, ModbusError> {
    let quantity = u16::try_from(len).map_err(|_| {
        ModbusError::ValidationError(format!("{name} quantity must be 1-{max}, got {len}"))
    })?;
    validate_quantity(name, quantity, max)?;
    Ok(quantity)
}

fn serialize_modbus_request(pdu: &ModbusRequest) -> Result<Vec<u8>, ModbusError> {
    let mut frame = Vec::with_capacity(MAX_REQUEST_PDU_LEN);
    match pdu {
        ModbusRequest::ReadCoils {
            starting_address,
            quantity,
        } => {
            frame.push(1);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::ReadDiscreteInputs {
            starting_address,
            quantity,
        } => {
            frame.push(2);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::ReadHoldingRegisters {
            starting_address,
            quantity,
        } => {
            frame.push(3);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::ReadInputRegisters {
            starting_address,
            quantity,
        } => {
            frame.push(4);
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
            // `ModbusRequest::serialize` validates the public API bounds first.
            // Keep the conversions defensive here as a backstop for tests that
            // intentionally bypass validation inside this module.
            frame.push(15u8);
            let quantity = u16::try_from(values.len()).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "WriteMultipleCoils quantity must fit in u16, got {}",
                    values.len()
                ))
            })?;
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
            let coil_bytes = pack_coils(values);
            let coil_byte_count = u8::try_from(coil_bytes.len()).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "WriteMultipleCoils byte count must fit in u8, got {}",
                    coil_bytes.len()
                ))
            })?;
            frame.push(coil_byte_count);
            frame.extend_from_slice(&coil_bytes);
        }
        ModbusRequest::WriteMultipleRegisters {
            starting_address,
            values,
        } => {
            // `ModbusRequest::serialize` validates the public API bounds first.
            // Keep the conversions defensive here as a backstop for tests that
            // intentionally bypass validation inside this module.
            frame.push(16u8);
            let quantity = u16::try_from(values.len()).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "WriteMultipleRegisters quantity must fit in u16, got {}",
                    values.len()
                ))
            })?;
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
            let register_byte_len = values.len().saturating_mul(2);
            let byte_count = u8::try_from(register_byte_len).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "WriteMultipleRegisters byte count must fit in u8, got {register_byte_len}"
                ))
            })?;
            frame.push(byte_count);
            for reg in values {
                frame.extend_from_slice(&reg.to_be_bytes());
            }
        }
    }
    Ok(frame)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_read_coils() {
        let pdu = ModbusRequest::ReadCoils {
            starting_address: 0x0010,
            quantity: 0x000A,
        };

        let expected = vec![1u8, 0x00, 0x10, 0x00, 0x0A];
        let result = pdu.serialize().unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_discrete_inputs() {
        let pdu = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0020,
            quantity: 0x0005,
        };

        let expected = vec![2u8, 0x00, 0x20, 0x00, 0x05];
        let result = pdu.serialize().unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_holding_registers() {
        let pdu = ModbusRequest::ReadHoldingRegisters {
            starting_address: 0x0100,
            quantity: 0x0003,
        };

        let expected = vec![3u8, 0x01, 0x00, 0x00, 0x03];
        let result = pdu.serialize().unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_input_registers() {
        let pdu = ModbusRequest::ReadInputRegisters {
            starting_address: 0x00FF,
            quantity: 0x0001,
        };

        let expected = vec![4u8, 0x00, 0xFF, 0x00, 0x01];
        let result = pdu.serialize().unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_coil_on() {
        let pdu = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: true,
        };

        let expected = vec![5u8, 0x00, 0x10, 0xFF, 0x00];
        let result = pdu.serialize().unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_coil_off() {
        let pdu = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: false,
        };

        let expected = vec![5u8, 0x00, 0x10, 0x00, 0x00];
        let result = pdu.serialize().unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_write_single_register() {
        let pdu = ModbusRequest::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };

        let expected = vec![6u8, 0x00, 0x10, 0x12, 0x34];
        let result = pdu.serialize().unwrap();
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
        let result = pdu.serialize().unwrap();
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
        let result = pdu.serialize().unwrap();
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
        let result = pdu.serialize().unwrap();
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
        let result = pdu.serialize().unwrap();
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
    fn test_read_discrete_inputs_validation_error() {
        let pdu = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0000,
            quantity: 0,
        };
        let err = pdu.serialize().unwrap_err();
        match err {
            ModbusError::ValidationError(msg) => {
                assert!(msg.contains("ReadDiscreteInputs"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }

        let pdu = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0000,
            quantity: 0x07D1,
        };
        assert!(pdu.serialize().is_err());
    }

    #[test]
    fn test_read_input_registers_validation_error() {
        let pdu = ModbusRequest::ReadInputRegisters {
            starting_address: 0x0000,
            quantity: 0,
        };
        let err = pdu.serialize().unwrap_err();
        match err {
            ModbusError::ValidationError(msg) => {
                assert!(msg.contains("ReadInputRegisters"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }

        let pdu = ModbusRequest::ReadInputRegisters {
            starting_address: 0x0000,
            quantity: 0x007E,
        };
        assert!(pdu.serialize().is_err());
    }

    /// Exercises the defensive `u16::try_from`/`u8::try_from` branches inside
    /// `serialize_modbus_request` by bypassing [`ModbusRequest::serialize`]'s
    /// validation step.
    #[test]
    fn serialize_write_multiple_coils_rejects_oversized_quantity() {
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: vec![true; usize::from(u16::MAX) + 1],
        };
        let err = serialize_modbus_request(&pdu).unwrap_err();
        match err {
            ModbusError::ValidationError(msg) => {
                assert!(msg.contains("WriteMultipleCoils"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

    #[test]
    fn serialize_write_multiple_coils_rejects_oversized_byte_count() {
        // Quantity fits in u16, but the packed-coil byte count overflows u8.
        // It intentionally exceeds the Modbus spec max to bypass public validation
        // and exercise the crate-internal defensive conversion.
        let pdu = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: vec![true; (usize::from(u8::MAX) + 1) * 8],
        };
        let err = serialize_modbus_request(&pdu).unwrap_err();
        match err {
            ModbusError::ValidationError(msg) => {
                assert!(msg.contains("byte count"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

    #[test]
    fn serialize_write_multiple_registers_rejects_oversized_quantity() {
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0000,
            values: vec![0x0000; usize::from(u16::MAX) + 1],
        };
        let err = serialize_modbus_request(&pdu).unwrap_err();
        match err {
            ModbusError::ValidationError(msg) => {
                assert!(msg.contains("WriteMultipleRegisters"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

    #[test]
    fn serialize_write_multiple_registers_rejects_oversized_byte_count() {
        // Quantity fits in u16, but byte count (quantity * 2) overflows u8.
        // It intentionally exceeds the Modbus spec max (123 registers) to bypass
        // public validation and exercise the crate-internal defensive conversion.
        let pdu = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0000,
            values: vec![0x0000; 200],
        };
        let err = serialize_modbus_request(&pdu).unwrap_err();
        match err {
            ModbusError::ValidationError(msg) => {
                assert!(msg.contains("byte count"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
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
