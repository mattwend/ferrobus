// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

use std::convert::TryFrom;

use crate::ModbusRequest;
use crate::error::ModbusError;

fn unpack_bits(
    response: &[u8],
    min_len: usize,
    exact_count: Option<usize>,
) -> Result<(usize, Vec<bool>), ModbusError> {
    if response.len() < min_len {
        return Err(ModbusError::DeserializationError(format!(
            "Invalid response: expected at least {} bytes, got {}",
            min_len,
            response.len()
        )));
    }
    let byte_count = usize::from(response[1]);
    if response.len() < 2 + byte_count {
        return Err(ModbusError::DeserializationError(format!(
            "Response length {} does not match byte count {}",
            response.len(),
            2 + byte_count
        )));
    }
    let bits = &response[2..2 + byte_count];
    let mut result = Vec::with_capacity(byte_count * 8);
    for byte in bits {
        for bit in 0..8 {
            result.push((byte >> bit) & 1 == 1);
        }
    }
    if let Some(count) = exact_count {
        result.truncate(count);
    }
    Ok((byte_count, result))
}

fn parse_registers(response: &[u8], min_len: usize) -> Result<Vec<u16>, ModbusError> {
    if response.len() < min_len {
        return Err(ModbusError::DeserializationError(format!(
            "Invalid response: expected at least {} bytes, got {}",
            min_len,
            response.len()
        )));
    }
    let byte_count = usize::from(response[1]);
    if response.len() < 2 + byte_count {
        return Err(ModbusError::DeserializationError(format!(
            "Response length {} does not match byte count {}",
            response.len(),
            2 + byte_count
        )));
    }
    if byte_count % 2 != 0 {
        return Err(ModbusError::DeserializationError(
            "Byte count is not even for register data".to_string(),
        ));
    }
    let reg_count = byte_count / 2;
    let mut registers = Vec::with_capacity(reg_count);
    for i in 0..reg_count {
        let offset = 2 + i * 2;
        registers.push(u16::from_be_bytes([response[offset], response[offset + 1]]));
    }
    Ok(registers)
}

/// Verifies that a decoded response matches the request that produced it.
///
/// # Errors
///
/// Returns [`ModbusError::RequestResponseMismatch`] when the response cannot
/// be reconciled with the request.
#[allow(clippy::too_many_lines)]
pub fn align_response_to_request(
    request: &ModbusRequest,
    response: ModbusResponse,
) -> Result<ModbusResponse, ModbusError> {
    match (request, &response) {
        (_, ModbusResponse::Exception { .. }) => Ok(response),
        (
            ModbusRequest::ReadCoils {
                quantity: requested,
                ..
            },
            ModbusResponse::ReadCoils { coils },
        ) => {
            if coils.len() < *requested as usize {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "ReadCoils: requested {} coils but got {}",
                    requested,
                    coils.len()
                )));
            }
            let trimmed = coils[..*requested as usize].to_vec();
            Ok(ModbusResponse::ReadCoils { coils: trimmed })
        }
        (
            ModbusRequest::ReadDiscreteInputs {
                quantity: requested,
                ..
            },
            ModbusResponse::ReadDiscreteInputs { inputs },
        ) => {
            if inputs.len() < *requested as usize {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "ReadDiscreteInputs: requested {} inputs but got {}",
                    requested,
                    inputs.len()
                )));
            }
            let trimmed = inputs[..*requested as usize].to_vec();
            Ok(ModbusResponse::ReadDiscreteInputs { inputs: trimmed })
        }
        (
            ModbusRequest::ReadHoldingRegisters {
                quantity: requested,
                ..
            },
            ModbusResponse::ReadHoldingRegisters { registers },
        ) => {
            if registers.len() != *requested as usize {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "ReadHoldingRegisters: requested {} registers but got {}",
                    requested,
                    registers.len()
                )));
            }
            Ok(response)
        }
        (
            ModbusRequest::ReadInputRegisters {
                quantity: requested,
                ..
            },
            ModbusResponse::ReadInputRegisters { registers },
        ) => {
            if registers.len() != *requested as usize {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "ReadInputRegisters: requested {} registers but got {}",
                    requested,
                    registers.len()
                )));
            }
            Ok(response)
        }
        (
            ModbusRequest::WriteSingleCoil { address, value },
            ModbusResponse::WriteSingleCoil {
                address: response_address,
                value: response_value,
            },
        ) => {
            if address != response_address || value != response_value {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "WriteSingleCoil: wrote address {address} value {value} but server echoed address {response_address} value {response_value}"
                )));
            }
            Ok(response)
        }
        (
            ModbusRequest::WriteSingleRegister { address, value },
            ModbusResponse::WriteSingleRegister {
                address: response_address,
                value: response_value,
            },
        ) => {
            if address != response_address || value != response_value {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "WriteSingleRegister: wrote address {address} value {value} but server echoed address {response_address} value {response_value}"
                )));
            }
            Ok(response)
        }
        (
            ModbusRequest::WriteMultipleCoils {
                starting_address,
                values,
            },
            ModbusResponse::WriteMultipleCoils {
                starting_address: response_address,
                quantity: resp_qty,
            },
        ) => {
            let qty = u16::try_from(values.len()).map_err(|_| {
                ModbusError::RequestResponseMismatch(format!(
                    "WriteMultipleCoils: request quantity does not fit in u16: {}",
                    values.len()
                ))
            })?;
            if starting_address != response_address || qty != *resp_qty {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "WriteMultipleCoils: wrote address {starting_address} quantity {qty} but server acknowledged address {response_address} quantity {resp_qty}",
                )));
            }
            Ok(response)
        }
        (
            ModbusRequest::WriteMultipleRegisters {
                starting_address,
                values,
            },
            ModbusResponse::WriteMultipleRegisters {
                starting_address: response_address,
                quantity: resp_qty,
            },
        ) => {
            let qty = u16::try_from(values.len()).map_err(|_| {
                ModbusError::RequestResponseMismatch(format!(
                    "WriteMultipleRegisters: request quantity does not fit in u16: {}",
                    values.len()
                ))
            })?;
            if starting_address != response_address || qty != *resp_qty {
                return Err(ModbusError::RequestResponseMismatch(format!(
                    "WriteMultipleRegisters: wrote address {starting_address} quantity {qty} but server acknowledged address {response_address} quantity {resp_qty}",
                )));
            }
            Ok(response)
        }
        _ => Err(ModbusError::RequestResponseMismatch(format!(
            "Request/response mismatch: got {response:?} for {request:?}"
        ))),
    }
}

/// Typed Modbus response PDUs.
#[derive(Debug, Clone, PartialEq)]
pub enum ModbusResponse {
    /// Coil output values returned by a read-coils request.
    ReadCoils {
        /// Coil values in wire order.
        coils: Vec<bool>,
    },
    /// Discrete input values returned by a read-discrete-inputs request.
    ReadDiscreteInputs {
        /// Input values in wire order.
        inputs: Vec<bool>,
    },
    /// Holding register values returned by a read-holding-registers request.
    ReadHoldingRegisters {
        /// Register values in wire order.
        registers: Vec<u16>,
    },
    /// Input register values returned by a read-input-registers request.
    ReadInputRegisters {
        /// Register values in wire order.
        registers: Vec<u16>,
    },
    /// Echo response for writing one coil.
    WriteSingleCoil {
        /// Coil address echoed by the device.
        address: u16,
        /// Coil value echoed by the device.
        value: bool,
    },
    /// Echo response for writing one register.
    WriteSingleRegister {
        /// Register address echoed by the device.
        address: u16,
        /// Register value echoed by the device.
        value: u16,
    },
    /// Acknowledgement for writing multiple coils.
    WriteMultipleCoils {
        /// Starting address acknowledged by the device.
        starting_address: u16,
        /// Quantity acknowledged by the device.
        quantity: u16,
    },
    /// Acknowledgement for writing multiple registers.
    WriteMultipleRegisters {
        /// Starting address acknowledged by the device.
        starting_address: u16,
        /// Quantity acknowledged by the device.
        quantity: u16,
    },
    /// Modbus exception response PDU.
    Exception {
        /// Exception function code, including the high exception bit.
        function: u8,
        /// Modbus exception code.
        code: u8,
    },
}

impl ModbusResponse {
    /// Deserializes a Modbus response PDU.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::DeserializationError`] when the bytes are not a supported response.
    pub fn deserialize(response: &[u8]) -> Result<ModbusResponse, ModbusError> {
        deserialize_modbus_response(response, None)
    }

    /// Deserializes a Modbus response PDU and truncates bit-packed reads to `count` values.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::DeserializationError`] when the bytes are not a supported response.
    pub fn deserialize_with_count(
        response: &[u8],
        count: usize,
    ) -> Result<ModbusResponse, ModbusError> {
        deserialize_modbus_response(response, Some(count))
    }

    /// Aligns this response with the request that produced it.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::RequestResponseMismatch`] if the response does not match.
    pub fn align_to_request(self, request: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        align_response_to_request(request, self)
    }
}

fn deserialize_modbus_response(
    response: &[u8],
    bit_count: Option<usize>,
) -> Result<ModbusResponse, ModbusError> {
    if response.is_empty() {
        return Err(ModbusError::DeserializationError(
            "Empty response".to_string(),
        ));
    }

    let function_code = response[0];
    match function_code {
        1 => {
            let (_, coils) = unpack_bits(response, 2, bit_count)?;
            Ok(ModbusResponse::ReadCoils { coils })
        }
        2 => {
            let (_, inputs) = unpack_bits(response, 2, bit_count)?;
            Ok(ModbusResponse::ReadDiscreteInputs { inputs })
        }
        3 => {
            let registers = parse_registers(response, 2)?;
            Ok(ModbusResponse::ReadHoldingRegisters { registers })
        }
        4 => {
            let registers = parse_registers(response, 2)?;
            Ok(ModbusResponse::ReadInputRegisters { registers })
        }
        5 => {
            if response.len() < 5 {
                return Err(ModbusError::DeserializationError(format!(
                    "Invalid Write Single Coil response: expected 5 bytes, got {}",
                    response.len()
                )));
            }
            let address = u16::from_be_bytes([response[1], response[2]]);
            let coil_value = u16::from_be_bytes([response[3], response[4]]);
            let value = match coil_value {
                0xFF00 => true,
                0x0000 => false,
                _ => {
                    return Err(ModbusError::DeserializationError(format!(
                        "Invalid coil value in Write Single Coil response: {coil_value:#06x}"
                    )));
                }
            };
            Ok(ModbusResponse::WriteSingleCoil { address, value })
        }
        6 => {
            if response.len() < 5 {
                return Err(ModbusError::DeserializationError(format!(
                    "Invalid Write Single Register response: expected 5 bytes, got {}",
                    response.len()
                )));
            }
            let address = u16::from_be_bytes([response[1], response[2]]);
            let value = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteSingleRegister { address, value })
        }
        15 => {
            if response.len() < 5 {
                return Err(ModbusError::DeserializationError(format!(
                    "Invalid Write Multiple Coils response: expected 5 bytes, got {}",
                    response.len()
                )));
            }
            let starting_address = u16::from_be_bytes([response[1], response[2]]);
            let quantity = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteMultipleCoils {
                starting_address,
                quantity,
            })
        }
        16 => {
            if response.len() < 5 {
                return Err(ModbusError::DeserializationError(format!(
                    "Invalid Write Multiple Registers response: expected 5 bytes, got {}",
                    response.len()
                )));
            }
            let starting_address = u16::from_be_bytes([response[1], response[2]]);
            let quantity = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteMultipleRegisters {
                starting_address,
                quantity,
            })
        }
        fc if fc & 0x80 != 0 => {
            if response.len() < 2 {
                return Err(ModbusError::DeserializationError(
                    "Invalid Exception response length".to_string(),
                ));
            }
            let exception_code = response[1];
            Ok(ModbusResponse::Exception {
                function: function_code,
                code: exception_code,
            })
        }
        _ => Err(ModbusError::DeserializationError(format!(
            "Unsupported function code: {function_code}"
        ))),
    }
}

impl TryFrom<&[u8]> for ModbusResponse {
    type Error = ModbusError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        ModbusResponse::deserialize(bytes)
    }
}

impl TryFrom<Vec<u8>> for ModbusResponse {
    type Error = ModbusError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        ModbusResponse::try_from(bytes.as_slice())
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::uninlined_format_args, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_read_coils() {
        let response = vec![1u8, 1u8, 0x25];
        let expected_coils = vec![true, false, true, false, false, true, false, false];
        let result = ModbusResponse::deserialize(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadCoils {
                coils: expected_coils
            }
        );
    }

    #[test]
    fn test_deserialize_read_discrete_inputs() {
        let response = vec![2u8, 1u8, 0xAA];
        let expected_inputs = vec![false, true, false, true, false, true, false, true];
        let result = ModbusResponse::deserialize(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadDiscreteInputs {
                inputs: expected_inputs
            }
        );
    }

    #[test]
    fn test_deserialize_read_holding_registers() {
        let response = vec![3u8, 4u8, 0x01, 0x02, 0x03, 0x04];
        let expected_registers = vec![0x0102, 0x0304];
        let result = ModbusResponse::deserialize(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadHoldingRegisters {
                registers: expected_registers
            }
        );
    }

    #[test]
    fn test_deserialize_read_input_registers() {
        let response = vec![4u8, 2u8, 0xAB, 0xCD];
        let expected_registers = vec![0xABCD];
        let result = ModbusResponse::deserialize(&response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadInputRegisters {
                registers: expected_registers
            }
        );
    }

    #[test]
    fn test_deserialize_write_single_coil_true() {
        let response = vec![5u8, 0x00, 0x10, 0xFF, 0x00];
        let result = ModbusResponse::deserialize(&response).unwrap();
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
        let response = vec![5u8, 0x00, 0x10, 0x00, 0x00];
        let result = ModbusResponse::deserialize(&response).unwrap();
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
        let response = vec![6u8, 0x00, 0x10, 0x12, 0x34];
        let result = ModbusResponse::deserialize(&response).unwrap();
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
        let response = vec![15u8, 0x00, 0x01, 0x00, 0x06];
        let result = ModbusResponse::deserialize(&response).unwrap();
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
        let response = vec![16u8, 0x00, 0x01, 0x00, 0x02];
        let result = ModbusResponse::deserialize(&response).unwrap();
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
        let response = vec![0x81u8, 0x02];
        let result = ModbusResponse::deserialize(&response).unwrap();
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
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_response_too_short_for_registers() {
        let response = vec![3u8, 1u8];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_align_response_trim_coils_to_requested_quantity() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 10,
        };
        let response = ModbusResponse::ReadCoils {
            coils: vec![
                true, false, true, false, false, true, false, false, true, false, true, false,
            ],
        };
        let result = align_response_to_request(&request, response).unwrap();
        match result {
            ModbusResponse::ReadCoils { coils } => {
                assert_eq!(coils.len(), 10);
                assert_eq!(
                    coils,
                    vec![
                        true, false, true, false, false, true, false, false, true, false
                    ]
                );
            }
            _ => panic!("Expected ReadCoils response"),
        }
    }

    #[test]
    fn test_align_response_trim_discrete_inputs_to_requested_quantity() {
        let request = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0000,
            quantity: 9,
        };
        let response = ModbusResponse::ReadDiscreteInputs {
            inputs: vec![
                false, true, false, true, false, true, false, true, false, true, false, true,
            ],
        };
        let result = align_response_to_request(&request, response).unwrap();
        match result {
            ModbusResponse::ReadDiscreteInputs { inputs } => {
                assert_eq!(inputs.len(), 9);
                assert_eq!(
                    inputs,
                    vec![false, true, false, true, false, true, false, true, false]
                );
            }
            _ => panic!("Expected ReadDiscreteInputs response"),
        }
    }

    #[test]
    fn test_align_response_preserves_when_response_matches_request() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 8,
        };
        let response = ModbusResponse::ReadCoils {
            coils: vec![true, false, true, false, false, true, false, false],
        };
        let result = align_response_to_request(&request, response).unwrap();
        match result {
            ModbusResponse::ReadCoils { coils } => {
                assert_eq!(coils.len(), 8);
            }
            _ => panic!("Expected ReadCoils response"),
        }
    }

    #[test]
    fn test_align_response_request_response_mismatch() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 10,
        };
        let response = ModbusResponse::ReadDiscreteInputs {
            inputs: vec![false; 10],
        };
        let result = align_response_to_request(&request, response);
        assert!(result.is_err());
        match result.unwrap_err() {
            ModbusError::RequestResponseMismatch(_) => {}
            e => panic!("Expected RequestResponseMismatch error, got {:?}", e),
        }
    }

    #[test]
    fn test_align_response_register_count_mismatch() {
        let request = ModbusRequest::ReadHoldingRegisters {
            starting_address: 0x0000,
            quantity: 3,
        };
        let response = ModbusResponse::ReadHoldingRegisters {
            registers: vec![0x0102, 0x0304],
        };
        let result = align_response_to_request(&request, response);
        assert!(result.is_err());
    }

    #[test]
    fn test_align_response_rejects_short_coil_response() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 10,
        };
        let response = ModbusResponse::ReadCoils {
            coils: vec![true, false, true, false, false, true, false, false],
        };
        let result = align_response_to_request(&request, response);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn test_align_response_allows_exception_response() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 10,
        };
        let response = ModbusResponse::Exception {
            function: 0x81,
            code: 0x02,
        };
        let result = align_response_to_request(&request, response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::Exception {
                function: 0x81,
                code: 0x02
            }
        );
    }

    #[test]
    fn test_align_response_write_single_coil_value_mismatch() {
        let request = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: true,
        };
        let response = ModbusResponse::WriteSingleCoil {
            address: 0x0010,
            value: false,
        };
        let result = align_response_to_request(&request, response);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn test_align_response_write_single_coil_address_mismatch() {
        let request = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: true,
        };
        let response = ModbusResponse::WriteSingleCoil {
            address: 0x0011,
            value: true,
        };
        let result = align_response_to_request(&request, response);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn test_align_response_write_single_register_address_mismatch() {
        let request = ModbusRequest::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };
        let response = ModbusResponse::WriteSingleRegister {
            address: 0x0011,
            value: 0x1234,
        };
        let result = align_response_to_request(&request, response);
        assert!(result.is_err());
    }

    #[test]
    fn test_align_response_write_multiple_coils_quantity_match() {
        let request = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: vec![true, false, true, false, false, true],
        };
        let response = ModbusResponse::WriteMultipleCoils {
            starting_address: 0x0000,
            quantity: 6,
        };
        let result = align_response_to_request(&request, response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteMultipleCoils {
                starting_address: 0x0000,
                quantity: 6,
            }
        );
    }

    #[test]
    fn test_align_response_write_multiple_coils_quantity_mismatch() {
        let request = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0000,
            values: vec![true, false, true, false, false, true],
        };
        let response = ModbusResponse::WriteMultipleCoils {
            starting_address: 0x0000,
            quantity: 5,
        };
        let result = align_response_to_request(&request, response);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_unsupported_function_code() {
        let response = vec![7u8];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
        match result.unwrap_err() {
            ModbusError::DeserializationError(msg) => {
                assert!(msg.contains("Unsupported function code"));
            }
            e => panic!("Expected DeserializationError, got {:?}", e),
        }
    }

    #[test]
    fn test_deserialize_write_single_coil_invalid_value() {
        let response = vec![5u8, 0x00, 0x10, 0x01, 0x00];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
        match result.unwrap_err() {
            ModbusError::DeserializationError(msg) => {
                assert!(msg.contains("Invalid coil value"));
            }
            e => panic!("Expected DeserializationError, got {:?}", e),
        }
    }

    #[test]
    fn test_deserialize_read_holding_registers_odd_byte_count() {
        let response = vec![3u8, 3u8, 0x01, 0x02, 0x03];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
        match result.unwrap_err() {
            ModbusError::DeserializationError(msg) => {
                assert!(msg.contains("not even"));
            }
            e => panic!("Expected DeserializationError, got {:?}", e),
        }
    }

    #[test]
    fn test_deserialize_read_coils_response_too_short_for_byte_count() {
        let response = vec![1u8, 5u8, 0x01, 0x02];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
        match result.unwrap_err() {
            ModbusError::DeserializationError(msg) => {
                assert!(msg.contains("does not match byte count"));
            }
            e => panic!("Expected DeserializationError, got {:?}", e),
        }
    }

    #[test]
    fn test_deserialize_read_holding_registers_response_too_short_for_byte_count() {
        let response = vec![3u8, 4u8, 0x01, 0x02];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
        match result.unwrap_err() {
            ModbusError::DeserializationError(msg) => {
                assert!(msg.contains("does not match byte count"));
            }
            e => panic!("Expected DeserializationError, got {:?}", e),
        }
    }

    #[test]
    fn test_deserialize_write_single_coil_too_short() {
        let response = vec![5u8, 0x00, 0x10, 0xFF];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_write_single_register_too_short() {
        let response = vec![6u8, 0x00, 0x10, 0x12];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_write_multiple_coils_too_short() {
        let response = vec![15u8, 0x00, 0x01, 0x00];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_write_multiple_registers_too_short() {
        let response = vec![16u8, 0x00, 0x01, 0x00];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_exception_too_short() {
        let response = vec![0x81u8];
        let result = ModbusResponse::deserialize(&response);
        assert!(result.is_err());
    }

    #[test]
    fn test_try_from_slice() {
        let response = vec![3u8, 4u8, 0x01, 0x02, 0x03, 0x04];
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadHoldingRegisters {
                registers: vec![0x0102, 0x0304]
            }
        );
    }

    #[test]
    fn test_try_from_vec() {
        let response = vec![3u8, 4u8, 0x01, 0x02, 0x03, 0x04];
        let result = ModbusResponse::try_from(response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadHoldingRegisters {
                registers: vec![0x0102, 0x0304]
            }
        );
    }

    #[test]
    fn test_try_from_slice_propagates_error() {
        let error = ModbusResponse::try_from([7u8].as_slice()).unwrap_err();
        match error {
            ModbusError::DeserializationError(message) => {
                assert!(message.contains("Unsupported function code"));
            }
            other => panic!("Expected DeserializationError, got {other:?}"),
        }
    }

    #[test]
    fn test_align_response_read_input_registers_count_mismatch() {
        let request = ModbusRequest::ReadInputRegisters {
            starting_address: 0x0000,
            quantity: 3,
        };
        let response = ModbusResponse::ReadInputRegisters {
            registers: vec![0x0102, 0x0304],
        };
        let result = align_response_to_request(&request, response);
        assert!(result.is_err());
    }

    #[test]
    fn test_align_response_write_single_register_value_mismatch() {
        let request = ModbusRequest::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };
        let response = ModbusResponse::WriteSingleRegister {
            address: 0x0010,
            value: 0x5678,
        };
        let result = align_response_to_request(&request, response);
        assert!(result.is_err());
    }

    #[test]
    fn test_align_response_write_multiple_registers_quantity_mismatch() {
        let request = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0000,
            values: vec![0x1111, 0x2222, 0x3333],
        };
        let response = ModbusResponse::WriteMultipleRegisters {
            starting_address: 0x0000,
            quantity: 2,
        };
        let result = align_response_to_request(&request, response);
        assert!(result.is_err());
    }

    #[test]
    fn test_align_response_write_multiple_coils_address_mismatch() {
        let request = ModbusRequest::WriteMultipleCoils {
            starting_address: 0x0001,
            values: vec![true, false, true],
        };
        let response = ModbusResponse::WriteMultipleCoils {
            starting_address: 0x0002,
            quantity: 3,
        };
        let result = align_response_to_request(&request, response);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn test_align_response_write_multiple_registers_address_mismatch() {
        let request = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0001,
            values: vec![0x1111, 0x2222, 0x3333],
        };
        let response = ModbusResponse::WriteMultipleRegisters {
            starting_address: 0x0002,
            quantity: 3,
        };
        let result = align_response_to_request(&request, response);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn align_response_write_single_coil_match() {
        let request = ModbusRequest::WriteSingleCoil {
            address: 0x0010,
            value: true,
        };
        let response = ModbusResponse::WriteSingleCoil {
            address: 0x0010,
            value: true,
        };
        let result = align_response_to_request(&request, response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteSingleCoil {
                address: 0x0010,
                value: true
            }
        );
    }

    #[test]
    fn align_response_write_single_register_match() {
        let request = ModbusRequest::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };
        let response = ModbusResponse::WriteSingleRegister {
            address: 0x0010,
            value: 0x1234,
        };
        let result = align_response_to_request(&request, response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteSingleRegister {
                address: 0x0010,
                value: 0x1234
            }
        );
    }

    #[test]
    fn align_response_read_holding_registers_match() {
        let request = ModbusRequest::ReadHoldingRegisters {
            starting_address: 0x0000,
            quantity: 2,
        };
        let response = ModbusResponse::ReadHoldingRegisters {
            registers: vec![0x0102, 0x0304],
        };
        let result = align_response_to_request(&request, response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadHoldingRegisters {
                registers: vec![0x0102, 0x0304]
            }
        );
    }

    #[test]
    fn align_response_read_input_registers_match() {
        let request = ModbusRequest::ReadInputRegisters {
            starting_address: 0x0000,
            quantity: 1,
        };
        let response = ModbusResponse::ReadInputRegisters {
            registers: vec![0xABCD],
        };
        let result = align_response_to_request(&request, response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::ReadInputRegisters {
                registers: vec![0xABCD]
            }
        );
    }

    #[test]
    fn align_response_write_multiple_registers_match() {
        let request = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0x0001,
            values: vec![0x1111, 0x2222],
        };
        let response = ModbusResponse::WriteMultipleRegisters {
            starting_address: 0x0001,
            quantity: 2,
        };
        let result = align_response_to_request(&request, response).unwrap();
        assert_eq!(
            result,
            ModbusResponse::WriteMultipleRegisters {
                starting_address: 0x0001,
                quantity: 2
            }
        );
    }

    #[test]
    fn align_response_read_discrete_inputs_short() {
        let request = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0000,
            quantity: 10,
        };
        let response = ModbusResponse::ReadDiscreteInputs {
            inputs: vec![false; 5],
        };
        let result = align_response_to_request(&request, response);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn align_write_multiple_coils_with_oversized_request_quantity_errors() {
        // Bypass `ModbusRequest::serialize` validation by constructing the
        // request directly with a length that exceeds u16::MAX. This is the
        // only way to exercise the defensive `u16::try_from` branch in
        // `align_response_to_request`.
        let request = ModbusRequest::WriteMultipleCoils {
            starting_address: 0,
            values: vec![true; usize::from(u16::MAX) + 1],
        };
        let response = ModbusResponse::WriteMultipleCoils {
            starting_address: 0,
            quantity: 1,
        };
        let err = align_response_to_request(&request, response).unwrap_err();
        match err {
            ModbusError::RequestResponseMismatch(msg) => {
                assert!(msg.contains("WriteMultipleCoils"));
            }
            other => panic!("expected RequestResponseMismatch, got {other:?}"),
        }
    }

    #[test]
    fn align_write_multiple_registers_with_oversized_request_quantity_errors() {
        let request = ModbusRequest::WriteMultipleRegisters {
            starting_address: 0,
            values: vec![0; usize::from(u16::MAX) + 1],
        };
        let response = ModbusResponse::WriteMultipleRegisters {
            starting_address: 0,
            quantity: 1,
        };
        let err = align_response_to_request(&request, response).unwrap_err();
        match err {
            ModbusError::RequestResponseMismatch(msg) => {
                assert!(msg.contains("WriteMultipleRegisters"));
            }
            other => panic!("expected RequestResponseMismatch, got {other:?}"),
        }
    }

    #[test]
    fn align_to_request_method_delegates_to_free_function() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0,
            quantity: 2,
        };
        let response = ModbusResponse::ReadCoils {
            coils: vec![true, false],
        };
        let via_method = response.clone().align_to_request(&request).unwrap();
        let via_fn = align_response_to_request(&request, response).unwrap();
        assert_eq!(via_method, via_fn);
    }

    #[test]
    fn test_modbus_response_clone() {
        let response = ModbusResponse::ReadHoldingRegisters {
            registers: vec![0x0102, 0x0304],
        };
        let cloned = response.clone();
        assert_eq!(response, cloned);
    }
}
