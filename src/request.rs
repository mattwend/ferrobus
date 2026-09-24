// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

use crate::error::ModbusError;
use crate::limits::{
    MAX_READ_COILS, MAX_READ_DISCRETE_INPUTS, MAX_READ_HOLDING_REGISTERS, MAX_READ_INPUT_REGISTERS,
    MAX_WRITE_MULTIPLE_COILS, MAX_WRITE_MULTIPLE_REGISTERS, READ_COILS, READ_DISCRETE_INPUTS,
    READ_HOLDING_REGISTERS, READ_INPUT_REGISTERS, WRITE_MULTIPLE_COILS, WRITE_MULTIPLE_REGISTERS,
    WRITE_SINGLE_COIL, WRITE_SINGLE_REGISTER,
};
use crate::response::{pack_bits_payload, unpack_bits_payload};

const MAX_REQUEST_PDU_LEN: usize = 252;

/// Errors returned when parsing a Modbus request PDU from bytes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestParseError {
    /// The first byte is not one of the supported request function codes.
    #[error("unknown function code 0x{0:02X}")]
    UnknownFunctionCode(u8),
    /// The PDU ended before all required bytes were present.
    #[error("PDU truncated: need {expected} bytes, got {actual}")]
    Truncated {
        /// Minimum required byte length.
        expected: usize,
        /// Actual byte length supplied by the caller.
        actual: usize,
    },
    /// The PDU contains surplus bytes or disagrees with a declared length.
    #[error("PDU length mismatch: expected {expected} bytes, got {actual}")]
    LengthMismatch {
        /// Exact expected byte length.
        expected: usize,
        /// Actual byte length supplied by the caller.
        actual: usize,
    },
    /// The Modbus byte-count field does not match the request quantity.
    #[error("declared byte count {declared} does not match quantity {quantity}")]
    ByteCountMismatch {
        /// Byte count declared by the request PDU.
        declared: u8,
        /// Quantity declared by the request PDU.
        quantity: u16,
    },
    /// A quantity was zero or above the protocol limit for the function.
    #[error("quantity {quantity} is outside protocol range 1..={limit}")]
    QuantityOutOfRange {
        /// Quantity declared by the request PDU.
        quantity: u16,
        /// Protocol-defined maximum for the function.
        limit: u16,
    },
    /// A write-single-coil value was neither the Modbus ON nor OFF sentinel.
    #[error("invalid coil value 0x{0:04X}: must be 0xFF00 or 0x0000")]
    InvalidCoilValue(u16),
}

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

    /// Extracts the scalar fields a response has to be checked against.
    pub(crate) fn echo(&self) -> RequestEcho {
        RequestEcho::from_request(self)
    }

    pub(crate) fn function_code(&self) -> u8 {
        match self {
            ModbusRequest::ReadCoils { .. } => READ_COILS,
            ModbusRequest::ReadDiscreteInputs { .. } => READ_DISCRETE_INPUTS,
            ModbusRequest::ReadHoldingRegisters { .. } => READ_HOLDING_REGISTERS,
            ModbusRequest::ReadInputRegisters { .. } => READ_INPUT_REGISTERS,
            ModbusRequest::WriteSingleCoil { .. } => WRITE_SINGLE_COIL,
            ModbusRequest::WriteSingleRegister { .. } => WRITE_SINGLE_REGISTER,
            ModbusRequest::WriteMultipleCoils { .. } => WRITE_MULTIPLE_COILS,
            ModbusRequest::WriteMultipleRegisters { .. } => WRITE_MULTIPLE_REGISTERS,
        }
    }
}

/// The scalar request fields that a response must agree with.
///
/// Every request PDU this crate supports has the same layout after the function
/// code: a two-byte address followed by a two-byte quantity, or — for the
/// single-write functions — the written value in the same position. Response
/// alignment never inspects anything else, so carrying just these five bytes
/// lets the server dispatch validate a response without cloning a request
/// payload that can hold up to 123 registers or 1968 coils.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestEcho {
    pub(crate) function_code: u8,
    pub(crate) address: u16,
    /// Quantity for reads and multi-writes, written value for single writes.
    pub(crate) quantity: u16,
}

impl RequestEcho {
    fn from_request(request: &ModbusRequest) -> Self {
        let function_code = request.function_code();
        let (address, quantity) = match request {
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
            } => (*starting_address, *quantity),
            ModbusRequest::WriteSingleCoil { address, value } => {
                (*address, if *value { 0xFF00 } else { 0x0000 })
            }
            ModbusRequest::WriteSingleRegister { address, value } => (*address, *value),
            ModbusRequest::WriteMultipleCoils {
                starting_address,
                values,
            } => (
                *starting_address,
                u16::try_from(values.len()).unwrap_or(u16::MAX),
            ),
            ModbusRequest::WriteMultipleRegisters {
                starting_address,
                values,
            } => (
                *starting_address,
                u16::try_from(values.len()).unwrap_or(u16::MAX),
            ),
        };
        Self {
            function_code,
            address,
            quantity,
        }
    }

    /// Reconstructs the boolean written by a write-single-coil request.
    pub(crate) fn coil_value(self) -> bool {
        self.quantity == 0xFF00
    }
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

fn parse_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn require_min_len(pdu: &[u8], expected: usize) -> Result<(), RequestParseError> {
    if pdu.len() < expected {
        return Err(RequestParseError::Truncated {
            expected,
            actual: pdu.len(),
        });
    }
    Ok(())
}

fn require_exact_len(pdu: &[u8], expected: usize) -> Result<(), RequestParseError> {
    require_min_len(pdu, expected)?;
    if pdu.len() > expected {
        return Err(RequestParseError::LengthMismatch {
            expected,
            actual: pdu.len(),
        });
    }
    Ok(())
}

fn validate_parsed_quantity(quantity: u16, limit: u16) -> Result<(), RequestParseError> {
    if quantity == 0 || quantity > limit {
        return Err(RequestParseError::QuantityOutOfRange { quantity, limit });
    }
    Ok(())
}

fn parse_read_request(
    pdu: &[u8],
    limit: u16,
    build: impl FnOnce(u16, u16) -> ModbusRequest,
) -> Result<ModbusRequest, RequestParseError> {
    require_exact_len(pdu, 5)?;
    let starting_address = parse_u16(pdu, 1);
    let quantity = parse_u16(pdu, 3);
    validate_parsed_quantity(quantity, limit)?;
    Ok(build(starting_address, quantity))
}

fn parse_multi_write_header(pdu: &[u8]) -> Result<(u16, u16, u8), RequestParseError> {
    require_min_len(pdu, 6)?;
    let starting_address = parse_u16(pdu, 1);
    let quantity = parse_u16(pdu, 3);
    let declared = pdu[5];
    Ok((starting_address, quantity, declared))
}

impl TryFrom<&[u8]> for ModbusRequest {
    type Error = RequestParseError;

    fn try_from(pdu: &[u8]) -> Result<Self, Self::Error> {
        require_min_len(pdu, 1)?;
        match pdu[0] {
            READ_COILS => parse_read_request(pdu, MAX_READ_COILS, |starting_address, quantity| {
                ModbusRequest::ReadCoils {
                    starting_address,
                    quantity,
                }
            }),
            READ_DISCRETE_INPUTS => parse_read_request(
                pdu,
                MAX_READ_DISCRETE_INPUTS,
                |starting_address, quantity| ModbusRequest::ReadDiscreteInputs {
                    starting_address,
                    quantity,
                },
            ),
            READ_HOLDING_REGISTERS => parse_read_request(
                pdu,
                MAX_READ_HOLDING_REGISTERS,
                |starting_address, quantity| ModbusRequest::ReadHoldingRegisters {
                    starting_address,
                    quantity,
                },
            ),
            READ_INPUT_REGISTERS => parse_read_request(
                pdu,
                MAX_READ_INPUT_REGISTERS,
                |starting_address, quantity| ModbusRequest::ReadInputRegisters {
                    starting_address,
                    quantity,
                },
            ),
            WRITE_SINGLE_COIL => {
                require_exact_len(pdu, 5)?;
                let address = parse_u16(pdu, 1);
                let raw_value = parse_u16(pdu, 3);
                let value = match raw_value {
                    0xFF00 => true,
                    0x0000 => false,
                    other => return Err(RequestParseError::InvalidCoilValue(other)),
                };
                Ok(ModbusRequest::WriteSingleCoil { address, value })
            }
            WRITE_SINGLE_REGISTER => {
                require_exact_len(pdu, 5)?;
                Ok(ModbusRequest::WriteSingleRegister {
                    address: parse_u16(pdu, 1),
                    value: parse_u16(pdu, 3),
                })
            }
            WRITE_MULTIPLE_COILS => {
                let (starting_address, quantity, declared) = parse_multi_write_header(pdu)?;
                validate_parsed_quantity(quantity, MAX_WRITE_MULTIPLE_COILS)?;
                let expected_count = (usize::from(quantity) + 7) / 8;
                if usize::from(declared) != expected_count {
                    return Err(RequestParseError::ByteCountMismatch { declared, quantity });
                }
                let expected_len = 6 + expected_count;
                require_exact_len(pdu, expected_len)?;
                let mut values = unpack_bits_payload(&pdu[6..]);
                values.truncate(usize::from(quantity));
                Ok(ModbusRequest::WriteMultipleCoils {
                    starting_address,
                    values,
                })
            }
            WRITE_MULTIPLE_REGISTERS => {
                let (starting_address, quantity, declared) = parse_multi_write_header(pdu)?;
                validate_parsed_quantity(quantity, MAX_WRITE_MULTIPLE_REGISTERS)?;
                let expected_count = usize::from(quantity) * 2;
                if usize::from(declared) != expected_count {
                    return Err(RequestParseError::ByteCountMismatch { declared, quantity });
                }
                let expected_len = 6 + expected_count;
                require_exact_len(pdu, expected_len)?;
                let mut values = Vec::with_capacity(usize::from(quantity));
                for chunk in pdu[6..].chunks_exact(2) {
                    values.push(u16::from_be_bytes([chunk[0], chunk[1]]));
                }
                Ok(ModbusRequest::WriteMultipleRegisters {
                    starting_address,
                    values,
                })
            }
            other => Err(RequestParseError::UnknownFunctionCode(other)),
        }
    }
}

fn serialize_modbus_request(pdu: &ModbusRequest) -> Result<Vec<u8>, ModbusError> {
    let mut frame = Vec::with_capacity(MAX_REQUEST_PDU_LEN);
    match pdu {
        ModbusRequest::ReadCoils {
            starting_address,
            quantity,
        } => {
            frame.push(READ_COILS);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::ReadDiscreteInputs {
            starting_address,
            quantity,
        } => {
            frame.push(READ_DISCRETE_INPUTS);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::ReadHoldingRegisters {
            starting_address,
            quantity,
        } => {
            frame.push(READ_HOLDING_REGISTERS);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::ReadInputRegisters {
            starting_address,
            quantity,
        } => {
            frame.push(READ_INPUT_REGISTERS);
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusRequest::WriteSingleCoil { address, value } => {
            frame.push(WRITE_SINGLE_COIL);
            frame.extend_from_slice(&address.to_be_bytes());
            let coil_value: u16 = if *value { 0xFF00 } else { 0x0000 };
            frame.extend_from_slice(&coil_value.to_be_bytes());
        }
        ModbusRequest::WriteSingleRegister { address, value } => {
            frame.push(WRITE_SINGLE_REGISTER);
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
            frame.push(WRITE_MULTIPLE_COILS);
            let quantity = u16::try_from(values.len()).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "WriteMultipleCoils quantity must fit in u16, got {}",
                    values.len()
                ))
            })?;
            frame.extend_from_slice(&starting_address.to_be_bytes());
            frame.extend_from_slice(&quantity.to_be_bytes());
            let coil_bytes = pack_bits_payload(values);
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
            frame.push(WRITE_MULTIPLE_REGISTERS);
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

    #[test]
    fn parse_all_supported_request_functions() {
        let fixtures = [
            (
                vec![0x01, 0x00, 0x10, 0x00, 0x02],
                ModbusRequest::ReadCoils {
                    starting_address: 0x0010,
                    quantity: 2,
                },
            ),
            (
                vec![0x02, 0x00, 0x10, 0x00, 0x02],
                ModbusRequest::ReadDiscreteInputs {
                    starting_address: 0x0010,
                    quantity: 2,
                },
            ),
            (
                vec![0x03, 0x00, 0x10, 0x00, 0x02],
                ModbusRequest::ReadHoldingRegisters {
                    starting_address: 0x0010,
                    quantity: 2,
                },
            ),
            (
                vec![0x04, 0x00, 0x10, 0x00, 0x02],
                ModbusRequest::ReadInputRegisters {
                    starting_address: 0x0010,
                    quantity: 2,
                },
            ),
            (
                vec![0x05, 0x00, 0x10, 0xFF, 0x00],
                ModbusRequest::WriteSingleCoil {
                    address: 0x0010,
                    value: true,
                },
            ),
            (
                vec![0x06, 0x00, 0x10, 0x12, 0x34],
                ModbusRequest::WriteSingleRegister {
                    address: 0x0010,
                    value: 0x1234,
                },
            ),
            (
                vec![0x0F, 0x00, 0x10, 0x00, 0x03, 0x01, 0x05],
                ModbusRequest::WriteMultipleCoils {
                    starting_address: 0x0010,
                    values: vec![true, false, true],
                },
            ),
            (
                vec![0x10, 0x00, 0x10, 0x00, 0x02, 0x04, 0x12, 0x34, 0x56, 0x78],
                ModbusRequest::WriteMultipleRegisters {
                    starting_address: 0x0010,
                    values: vec![0x1234, 0x5678],
                },
            ),
        ];
        for (bytes, expected) in fixtures {
            assert_eq!(ModbusRequest::try_from(bytes.as_slice()).unwrap(), expected);
        }
    }

    #[test]
    fn request_echo_matches_the_serialized_wire_fields() {
        // The echo exists so response alignment never has to keep the request
        // payload around. It must be exactly bytes 0..5 of the request PDU.
        let requests = [
            ModbusRequest::ReadCoils {
                starting_address: 0x0010,
                quantity: 9,
            },
            ModbusRequest::ReadDiscreteInputs {
                starting_address: 0x0011,
                quantity: 3,
            },
            ModbusRequest::ReadHoldingRegisters {
                starting_address: 0x0012,
                quantity: 2,
            },
            ModbusRequest::ReadInputRegisters {
                starting_address: 0x0013,
                quantity: 1,
            },
            ModbusRequest::WriteSingleCoil {
                address: 0x0014,
                value: true,
            },
            ModbusRequest::WriteSingleCoil {
                address: 0x0015,
                value: false,
            },
            ModbusRequest::WriteSingleRegister {
                address: 0x0016,
                value: 0x1234,
            },
            ModbusRequest::WriteMultipleCoils {
                starting_address: 0x0017,
                values: vec![true, false, true],
            },
            ModbusRequest::WriteMultipleRegisters {
                starting_address: 0x0018,
                values: vec![0x1111, 0x2222],
            },
        ];
        for request in requests {
            let bytes = request.serialize().unwrap();
            let echo = request.echo();
            assert_eq!(echo.function_code, bytes[0], "{request:?}");
            assert_eq!(
                echo.address,
                u16::from_be_bytes([bytes[1], bytes[2]]),
                "{request:?}"
            );
            assert_eq!(
                echo.quantity,
                u16::from_be_bytes([bytes[3], bytes[4]]),
                "{request:?}"
            );
        }

        assert!(
            ModbusRequest::WriteSingleCoil {
                address: 0,
                value: true
            }
            .echo()
            .coil_value()
        );
        assert!(
            !ModbusRequest::WriteSingleCoil {
                address: 0,
                value: false
            }
            .echo()
            .coil_value()
        );
    }

    #[test]
    fn parse_request_reports_structured_errors() {
        assert_eq!(
            ModbusRequest::try_from([0x07].as_slice()).unwrap_err(),
            RequestParseError::UnknownFunctionCode(0x07)
        );
        assert!(matches!(
            ModbusRequest::try_from([].as_slice()),
            Err(RequestParseError::Truncated { .. })
        ));
        assert!(matches!(
            ModbusRequest::try_from([0x01, 0x00].as_slice()),
            Err(RequestParseError::Truncated { .. })
        ));
        assert_eq!(
            ModbusRequest::try_from([0x05, 0, 0, 0x12, 0x34].as_slice()).unwrap_err(),
            RequestParseError::InvalidCoilValue(0x1234)
        );
        assert!(matches!(
            ModbusRequest::try_from([0x01, 0, 0, 0, 1, 0].as_slice()),
            Err(RequestParseError::LengthMismatch { .. })
        ));
        assert!(matches!(
            ModbusRequest::try_from([0x03, 0, 0, 0, 0].as_slice()),
            Err(RequestParseError::QuantityOutOfRange { .. })
        ));
        assert!(matches!(
            ModbusRequest::try_from([0x0F, 0, 0, 0, 9, 1, 0].as_slice()),
            Err(RequestParseError::ByteCountMismatch { .. })
        ));
        assert!(matches!(
            ModbusRequest::try_from([0x10, 0, 0, 0, 2, 4, 0].as_slice()),
            Err(RequestParseError::Truncated { .. })
        ));
    }
}
