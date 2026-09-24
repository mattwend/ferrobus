// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

use std::fmt;

use crate::ModbusRequest;
use crate::error::ModbusError;
use crate::limits::{
    MAX_READ_COILS, MAX_READ_DISCRETE_INPUTS, MAX_READ_HOLDING_REGISTERS, MAX_READ_INPUT_REGISTERS,
    MAX_WRITE_MULTIPLE_COILS, MAX_WRITE_MULTIPLE_REGISTERS,
};

fn byte_count(response: &[u8]) -> Result<usize, ModbusError> {
    if response.len() < 2 {
        return Err(ModbusError::DeserializationError(format!(
            "Invalid response: expected at least 2 bytes, got {}",
            response.len()
        )));
    }
    let byte_count = usize::from(response[1]);
    let expected_len = 2 + byte_count;
    if response.len() != expected_len {
        return Err(ModbusError::DeserializationError(format!(
            "Response length {} does not match byte count {}",
            response.len(),
            expected_len
        )));
    }
    Ok(byte_count)
}

fn require_exact_len(
    response: &[u8],
    expected_len: usize,
    context: &str,
) -> Result<(), ModbusError> {
    if response.len() != expected_len {
        return Err(ModbusError::DeserializationError(format!(
            "Invalid {context} response: expected {expected_len} bytes, got {}",
            response.len()
        )));
    }
    Ok(())
}

/// Packs bits LSB-first into Modbus coil/discrete-input payload bytes.
///
/// Shared by the request and response codecs; the inverse is
/// [`unpack_bits_payload`].
pub(crate) fn pack_bits_payload(bits: &[bool]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(bits.len().div_ceil(8));
    let mut current = 0u8;
    let mut bit_index = 0u8;
    for bit in bits {
        if *bit {
            current |= 1 << bit_index;
        }
        bit_index += 1;
        if bit_index == 8 {
            bytes.push(current);
            current = 0;
            bit_index = 0;
        }
    }
    if bit_index > 0 {
        bytes.push(current);
    }
    bytes
}

pub(crate) fn unpack_bits_payload(bits: &[u8]) -> Vec<bool> {
    let mut result = Vec::with_capacity(bits.len() * 8);
    for byte in bits {
        for bit in 0..8 {
            result.push((byte >> bit) & 1 == 1);
        }
    }
    result
}

fn unpack_bits(response: &[u8]) -> Result<Vec<bool>, ModbusError> {
    let byte_count = byte_count(response)?;
    Ok(unpack_bits_payload(&response[2..2 + byte_count]))
}

fn parse_registers(response: &[u8]) -> Result<Vec<u16>, ModbusError> {
    let byte_count = byte_count(response)?;
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

/// Error returned when a raw byte is not a valid Modbus request function code.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
#[error("invalid function code 0x{0:02X}: expected 0x01..=0x7F")]
pub struct InvalidFunctionCode(u8);

/// Raw Modbus request function code carried by exception responses.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionCode(u8);

pub(crate) const SYNTHETIC_ZERO_FUNCTION: FunctionCode = FunctionCode(0);

impl FunctionCode {
    pub(crate) fn normalize_for_exception(raw: u8) -> Self {
        let low = raw & 0x7F;
        if low == 0 {
            SYNTHETIC_ZERO_FUNCTION
        } else {
            Self(low)
        }
    }
}

impl TryFrom<u8> for FunctionCode {
    type Error = InvalidFunctionCode;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if (0x01..=0x7F).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidFunctionCode(value))
        }
    }
}

impl From<FunctionCode> for u8 {
    fn from(value: FunctionCode) -> Self {
        value.0
    }
}

impl fmt::Display for FunctionCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "0x{:02X}", self.0)
    }
}

/// Modbus exception code values, preserving unknown wire values.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionCode {
    /// Function code is not supported by the server.
    IllegalFunction,
    /// Requested data address is not available.
    IllegalDataAddress,
    /// Request value is structurally invalid for the function.
    IllegalDataValue,
    /// Server failed while processing a valid request.
    ServerDeviceFailure,
    /// Server accepted a long-running request.
    Acknowledge,
    /// Server is busy and the client may retry later.
    ServerDeviceBusy,
    /// Server detected a memory parity error.
    MemoryParityError,
    /// Gateway path is unavailable.
    GatewayPathUnavailable,
    /// Gateway target device did not respond.
    GatewayTargetDeviceFailedToRespond,
    /// Unknown exception byte preserved for forward compatibility.
    Unknown(u8),
}

impl From<u8> for ExceptionCode {
    fn from(value: u8) -> Self {
        match value {
            0x01 => Self::IllegalFunction,
            0x02 => Self::IllegalDataAddress,
            0x03 => Self::IllegalDataValue,
            0x04 => Self::ServerDeviceFailure,
            0x05 => Self::Acknowledge,
            0x06 => Self::ServerDeviceBusy,
            0x08 => Self::MemoryParityError,
            0x0A => Self::GatewayPathUnavailable,
            0x0B => Self::GatewayTargetDeviceFailedToRespond,
            other => Self::Unknown(other),
        }
    }
}

impl From<ExceptionCode> for u8 {
    fn from(value: ExceptionCode) -> Self {
        match value {
            ExceptionCode::IllegalFunction => 0x01,
            ExceptionCode::IllegalDataAddress => 0x02,
            ExceptionCode::IllegalDataValue => 0x03,
            ExceptionCode::ServerDeviceFailure => 0x04,
            ExceptionCode::Acknowledge => 0x05,
            ExceptionCode::ServerDeviceBusy => 0x06,
            ExceptionCode::MemoryParityError => 0x08,
            ExceptionCode::GatewayPathUnavailable => 0x0A,
            ExceptionCode::GatewayTargetDeviceFailedToRespond => 0x0B,
            ExceptionCode::Unknown(raw) => raw,
        }
    }
}

impl fmt::Display for ExceptionCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::IllegalFunction => "IllegalFunction",
            Self::IllegalDataAddress => "IllegalDataAddress",
            Self::IllegalDataValue => "IllegalDataValue",
            Self::ServerDeviceFailure => "ServerDeviceFailure",
            Self::Acknowledge => "Acknowledge",
            Self::ServerDeviceBusy => "ServerDeviceBusy",
            Self::MemoryParityError => "MemoryParityError",
            Self::GatewayPathUnavailable => "GatewayPathUnavailable",
            Self::GatewayTargetDeviceFailedToRespond => "GatewayTargetDeviceFailedToRespond",
            Self::Unknown(_) => "Unknown",
        };
        write!(formatter, "{name}(0x{:02X})", u8::from(*self))
    }
}

/// Typed Modbus response PDUs.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    ///
    /// [`crate::tcp::ModbusTcpConnection::send_message`] and
    /// [`crate::tcp::ModbusTcpConnection::send_message_with_unit_id`] convert this response into
    /// [`ModbusError::ExceptionResponse`] after decoding so callers can handle exceptions through
    /// the same error path that retry policy uses. This variant is primarily visible when parsing
    /// response PDUs directly with [`TryFrom`].
    Exception {
        /// Raw request function code without the high exception bit.
        function_code: FunctionCode,
        /// Modbus exception code.
        code: ExceptionCode,
    },
}

impl ModbusResponse {
    /// Serializes the response into a Modbus PDU.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::ValidationError`] when the response violates Modbus limits.
    pub fn serialize(&self) -> Result<Vec<u8>, ModbusError> {
        serialize_modbus_response(self)
    }

    /// Aligns this response with the request that produced it.
    ///
    /// # Arguments
    ///
    /// * `request` - The request expected to have produced this response.
    ///
    /// # Errors
    ///
    /// Returns [`ModbusError::RequestResponseMismatch`] if the response does not match.
    ///
    /// # Returns
    ///
    /// Returns the validated response, trimming bit-packed read results to the requested count.
    #[allow(clippy::too_many_lines)]
    pub fn align_to_request(self, request: &ModbusRequest) -> Result<ModbusResponse, ModbusError> {
        match (request, &self) {
            (_, ModbusResponse::Exception { .. }) => Ok(self),
            (
                ModbusRequest::ReadCoils {
                    quantity: requested,
                    ..
                },
                ModbusResponse::ReadCoils { coils },
            ) => {
                validate_bit_count("ReadCoils", *requested, coils.len())?;
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
                validate_bit_count("ReadDiscreteInputs", *requested, inputs.len())?;
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
                Ok(self)
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
                Ok(self)
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
                Ok(self)
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
                Ok(self)
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
                Ok(self)
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
                Ok(self)
            }
            _ => Err(ModbusError::RequestResponseMismatch(format!(
                "Request/response mismatch: got {self:?} for {request:?}"
            ))),
        }
    }
}

fn validate_len(name: &str, len: usize, limit: u16) -> Result<u16, ModbusError> {
    let quantity = u16::try_from(len).map_err(|_| {
        ModbusError::ValidationError(format!("{name} quantity must be 0-{limit}, got {len}"))
    })?;
    if quantity > limit {
        return Err(ModbusError::ValidationError(format!(
            "{name} quantity must be 0-{limit}, got {quantity}"
        )));
    }
    Ok(quantity)
}

fn validate_ack_quantity(name: &str, quantity: u16, limit: u16) -> Result<(), ModbusError> {
    if quantity == 0 || quantity > limit {
        return Err(ModbusError::ValidationError(format!(
            "{name} quantity must be 1-{limit}, got {quantity}"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn serialize_modbus_response(response: &ModbusResponse) -> Result<Vec<u8>, ModbusError> {
    let mut pdu = Vec::new();
    match response {
        ModbusResponse::ReadCoils { coils } => {
            validate_len("ReadCoils", coils.len(), MAX_READ_COILS)?;
            let payload = pack_bits_payload(coils);
            let byte_count = u8::try_from(payload.len()).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "ReadCoils byte count must fit in u8, got {}",
                    payload.len()
                ))
            })?;
            pdu.push(0x01);
            pdu.push(byte_count);
            pdu.extend_from_slice(&payload);
        }
        ModbusResponse::ReadDiscreteInputs { inputs } => {
            validate_len("ReadDiscreteInputs", inputs.len(), MAX_READ_DISCRETE_INPUTS)?;
            let payload = pack_bits_payload(inputs);
            let byte_count = u8::try_from(payload.len()).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "ReadDiscreteInputs byte count must fit in u8, got {}",
                    payload.len()
                ))
            })?;
            pdu.push(0x02);
            pdu.push(byte_count);
            pdu.extend_from_slice(&payload);
        }
        ModbusResponse::ReadHoldingRegisters { registers } => {
            validate_len(
                "ReadHoldingRegisters",
                registers.len(),
                MAX_READ_HOLDING_REGISTERS,
            )?;
            let byte_count = u8::try_from(registers.len() * 2).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "ReadHoldingRegisters byte count must fit in u8, got {}",
                    registers.len() * 2
                ))
            })?;
            pdu.push(0x03);
            pdu.push(byte_count);
            for register in registers {
                pdu.extend_from_slice(&register.to_be_bytes());
            }
        }
        ModbusResponse::ReadInputRegisters { registers } => {
            validate_len(
                "ReadInputRegisters",
                registers.len(),
                MAX_READ_INPUT_REGISTERS,
            )?;
            let byte_count = u8::try_from(registers.len() * 2).map_err(|_| {
                ModbusError::ValidationError(format!(
                    "ReadInputRegisters byte count must fit in u8, got {}",
                    registers.len() * 2
                ))
            })?;
            pdu.push(0x04);
            pdu.push(byte_count);
            for register in registers {
                pdu.extend_from_slice(&register.to_be_bytes());
            }
        }
        ModbusResponse::WriteSingleCoil { address, value } => {
            pdu.push(0x05);
            pdu.extend_from_slice(&address.to_be_bytes());
            let raw = if *value { 0xFF00u16 } else { 0x0000u16 };
            pdu.extend_from_slice(&raw.to_be_bytes());
        }
        ModbusResponse::WriteSingleRegister { address, value } => {
            pdu.push(0x06);
            pdu.extend_from_slice(&address.to_be_bytes());
            pdu.extend_from_slice(&value.to_be_bytes());
        }
        ModbusResponse::WriteMultipleCoils {
            starting_address,
            quantity,
        } => {
            validate_ack_quantity("WriteMultipleCoils", *quantity, MAX_WRITE_MULTIPLE_COILS)?;
            pdu.push(0x0F);
            pdu.extend_from_slice(&starting_address.to_be_bytes());
            pdu.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusResponse::WriteMultipleRegisters {
            starting_address,
            quantity,
        } => {
            validate_ack_quantity(
                "WriteMultipleRegisters",
                *quantity,
                MAX_WRITE_MULTIPLE_REGISTERS,
            )?;
            pdu.push(0x10);
            pdu.extend_from_slice(&starting_address.to_be_bytes());
            pdu.extend_from_slice(&quantity.to_be_bytes());
        }
        ModbusResponse::Exception {
            function_code,
            code,
        } => {
            pdu.push(u8::from(*function_code) | 0x80);
            pdu.push(u8::from(*code));
        }
    }
    Ok(pdu)
}

fn validate_bit_count(
    function_name: &str,
    requested: u16,
    returned_bits: usize,
) -> Result<(), ModbusError> {
    let requested = usize::from(requested);
    if returned_bits < requested {
        return Err(ModbusError::RequestResponseMismatch(format!(
            "{function_name}: requested {requested} bits but got {returned_bits}"
        )));
    }
    if returned_bits > requested + 7 {
        return Err(ModbusError::RequestResponseMismatch(format!(
            "{function_name}: requested {requested} bits but got {returned_bits}, more than one packed byte of padding"
        )));
    }
    Ok(())
}

fn deserialize_modbus_response(response: &[u8]) -> Result<ModbusResponse, ModbusError> {
    if response.is_empty() {
        return Err(ModbusError::DeserializationError(
            "Empty response".to_string(),
        ));
    }

    let function_code = response[0];
    match function_code {
        1 => {
            let coils = unpack_bits(response)?;
            Ok(ModbusResponse::ReadCoils { coils })
        }
        2 => {
            let inputs = unpack_bits(response)?;
            Ok(ModbusResponse::ReadDiscreteInputs { inputs })
        }
        3 => {
            let registers = parse_registers(response)?;
            Ok(ModbusResponse::ReadHoldingRegisters { registers })
        }
        4 => {
            let registers = parse_registers(response)?;
            Ok(ModbusResponse::ReadInputRegisters { registers })
        }
        5 => {
            require_exact_len(response, 5, "Write Single Coil")?;
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
            require_exact_len(response, 5, "Write Single Register")?;
            let address = u16::from_be_bytes([response[1], response[2]]);
            let value = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteSingleRegister { address, value })
        }
        15 => {
            require_exact_len(response, 5, "Write Multiple Coils")?;
            let starting_address = u16::from_be_bytes([response[1], response[2]]);
            let quantity = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteMultipleCoils {
                starting_address,
                quantity,
            })
        }
        16 => {
            require_exact_len(response, 5, "Write Multiple Registers")?;
            let starting_address = u16::from_be_bytes([response[1], response[2]]);
            let quantity = u16::from_be_bytes([response[3], response[4]]);
            Ok(ModbusResponse::WriteMultipleRegisters {
                starting_address,
                quantity,
            })
        }
        fc if fc & 0x80 != 0 => {
            require_exact_len(response, 2, "Exception")?;
            Ok(ModbusResponse::Exception {
                function_code: FunctionCode::normalize_for_exception(function_code),
                code: ExceptionCode::from(response[1]),
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
        deserialize_modbus_response(bytes)
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
    fn pack_bits_payload_empty() {
        let bytes = pack_bits_payload(&[]);
        assert!(bytes.is_empty());
    }

    #[test]
    fn pack_bits_payload_single_coil() {
        let bytes = pack_bits_payload(&[true]);
        assert_eq!(bytes, &[0x01]);
    }

    #[test]
    fn pack_bits_payload_exactly_eight() {
        let coils = vec![true, false, true, false, true, false, true, false];
        let bytes = pack_bits_payload(&coils);
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes[0], 0x55);
    }

    #[test]
    fn pack_bits_payload_nine_bits() {
        let coils = vec![true; 9];
        let bytes = pack_bits_payload(&coils);
        assert_eq!(bytes.len(), 2);
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1], 0x01);
    }

    #[test]
    fn pack_bits_payload_alternating() {
        let bytes = pack_bits_payload(&[
            true, false, true, false, true, false, true, false, true, false,
        ]);
        assert_eq!(bytes.len(), 2);
        assert_eq!(bytes[0], 0x55);
        assert_eq!(bytes[1], 0x01);
    }

    #[test]
    fn pack_bits_payload_all_false() {
        let bytes = pack_bits_payload(&[false, false, false, false]);
        assert_eq!(bytes, &[0x00]);
    }

    #[test]
    fn pack_bits_payload_round_trips_through_unpack_bits_payload() {
        // The request and response codecs share one packer/unpacker pair; this
        // pins the two halves against each other.
        let bits = vec![
            true, false, true, true, false, false, false, true, true, false,
        ];
        let packed = pack_bits_payload(&bits);
        let mut unpacked = unpack_bits_payload(&packed);
        unpacked.truncate(bits.len());
        assert_eq!(unpacked, bits);
    }

    #[test]
    fn test_deserialize_read_coils() {
        let response = vec![1u8, 1u8, 0x25];
        let expected_coils = vec![true, false, true, false, false, true, false, false];
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
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
        let result = ModbusResponse::try_from(response.as_slice()).unwrap();
        assert_eq!(
            result,
            ModbusResponse::Exception {
                function_code: FunctionCode::try_from(0x01).unwrap(),
                code: ExceptionCode::IllegalDataAddress
            }
        );
    }

    #[test]
    fn test_invalid_response_empty() {
        let response = vec![];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_response_too_short_for_registers() {
        let response = vec![3u8, 1u8];
        let result = ModbusResponse::try_from(response.as_slice());
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn test_align_response_rejects_extra_packed_coil_byte() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0x0000,
            quantity: 1,
        };
        let response = ModbusResponse::ReadCoils {
            coils: vec![true; 16],
        };
        let result = response.align_to_request(&request);
        assert!(matches!(
            result,
            Err(ModbusError::RequestResponseMismatch(_))
        ));
    }

    #[test]
    fn test_align_response_rejects_extra_packed_discrete_input_byte() {
        let request = ModbusRequest::ReadDiscreteInputs {
            starting_address: 0x0000,
            quantity: 1,
        };
        let response = ModbusResponse::ReadDiscreteInputs {
            inputs: vec![true; 16],
        };
        let result = response.align_to_request(&request);
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
            function_code: FunctionCode::try_from(0x01).unwrap(),
            code: ExceptionCode::IllegalDataAddress,
        };
        let result = response.align_to_request(&request).unwrap();
        assert_eq!(
            result,
            ModbusResponse::Exception {
                function_code: FunctionCode::try_from(0x01).unwrap(),
                code: ExceptionCode::IllegalDataAddress
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_unsupported_function_code() {
        let response = vec![7u8];
        let result = ModbusResponse::try_from(response.as_slice());
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
        let result = ModbusResponse::try_from(response.as_slice());
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
        let result = ModbusResponse::try_from(response.as_slice());
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
        let result = ModbusResponse::try_from(response.as_slice());
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
        let result = ModbusResponse::try_from(response.as_slice());
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
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_write_single_register_too_short() {
        let response = vec![6u8, 0x00, 0x10, 0x12];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_write_multiple_coils_too_short() {
        let response = vec![15u8, 0x00, 0x01, 0x00];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_write_multiple_registers_too_short() {
        let response = vec![16u8, 0x00, 0x01, 0x00];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_exception_too_short() {
        let response = vec![0x81u8];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_read_coils_rejects_trailing_bytes() {
        let response = vec![1u8, 1u8, 0x25, 0xFF];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(matches!(result, Err(ModbusError::DeserializationError(_))));
    }

    #[test]
    fn test_deserialize_read_holding_registers_rejects_trailing_bytes() {
        let response = vec![3u8, 2u8, 0x01, 0x02, 0xFF];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(matches!(result, Err(ModbusError::DeserializationError(_))));
    }

    #[test]
    fn test_deserialize_write_echo_rejects_trailing_bytes() {
        let response = vec![6u8, 0x00, 0x10, 0x12, 0x34, 0xFF];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(matches!(result, Err(ModbusError::DeserializationError(_))));
    }

    #[test]
    fn test_deserialize_exception_rejects_trailing_bytes() {
        let response = vec![0x81u8, 0x02, 0xFF];
        let result = ModbusResponse::try_from(response.as_slice());
        assert!(matches!(result, Err(ModbusError::DeserializationError(_))));
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request);
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request).unwrap();
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
        let result = response.align_to_request(&request);
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
        let err = response.align_to_request(&request).unwrap_err();
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
        let err = response.align_to_request(&request).unwrap_err();
        match err {
            ModbusError::RequestResponseMismatch(msg) => {
                assert!(msg.contains("WriteMultipleRegisters"));
            }
            other => panic!("expected RequestResponseMismatch, got {other:?}"),
        }
    }

    #[test]
    fn align_to_request_method_aligns_response() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 0,
            quantity: 2,
        };
        let response = ModbusResponse::ReadCoils {
            coils: vec![true, false],
        };
        let aligned = response.align_to_request(&request).unwrap();
        assert_eq!(
            aligned,
            ModbusResponse::ReadCoils {
                coils: vec![true, false]
            }
        );
    }

    #[test]
    fn test_modbus_response_clone() {
        let response = ModbusResponse::ReadHoldingRegisters {
            registers: vec![0x0102, 0x0304],
        };
        let cloned = response.clone();
        assert_eq!(response, cloned);
    }

    #[test]
    fn typed_exception_codes_round_trip() {
        assert!(FunctionCode::try_from(0x01).is_ok());
        assert!(FunctionCode::try_from(0x7F).is_ok());
        assert!(FunctionCode::try_from(0x00).is_err());
        assert!(FunctionCode::try_from(0x80).is_err());
        assert!(FunctionCode::try_from(0xFF).is_err());

        let response = ModbusResponse::Exception {
            function_code: FunctionCode::try_from(0x03).unwrap(),
            code: ExceptionCode::IllegalDataValue,
        };
        let bytes = response.serialize().unwrap();
        assert_eq!(bytes, vec![0x83, 0x03]);
        assert_eq!(
            ModbusResponse::try_from(bytes.as_slice()).unwrap(),
            response
        );

        let unknown = ModbusResponse::try_from([0x83, 0x09].as_slice()).unwrap();
        assert_eq!(
            unknown,
            ModbusResponse::Exception {
                function_code: FunctionCode::try_from(0x03).unwrap(),
                code: ExceptionCode::Unknown(0x09),
            }
        );
        assert_eq!(unknown.serialize().unwrap(), vec![0x83, 0x09]);
    }

    #[test]
    fn serialize_response_fixtures() {
        let fixtures = [
            (
                ModbusResponse::ReadCoils {
                    coils: vec![true, false, true],
                },
                vec![0x01, 0x01, 0x05],
            ),
            (
                ModbusResponse::ReadDiscreteInputs {
                    inputs: vec![false, true, false, true],
                },
                vec![0x02, 0x01, 0x0A],
            ),
            (
                ModbusResponse::ReadHoldingRegisters {
                    registers: vec![0x1234, 0x5678],
                },
                vec![0x03, 0x04, 0x12, 0x34, 0x56, 0x78],
            ),
            (
                ModbusResponse::ReadInputRegisters {
                    registers: vec![0x1234],
                },
                vec![0x04, 0x02, 0x12, 0x34],
            ),
            (
                ModbusResponse::WriteSingleCoil {
                    address: 0x0010,
                    value: true,
                },
                vec![0x05, 0x00, 0x10, 0xFF, 0x00],
            ),
            (
                ModbusResponse::WriteSingleRegister {
                    address: 0x0010,
                    value: 0x1234,
                },
                vec![0x06, 0x00, 0x10, 0x12, 0x34],
            ),
            (
                ModbusResponse::WriteMultipleCoils {
                    starting_address: 0x0010,
                    quantity: 3,
                },
                vec![0x0F, 0x00, 0x10, 0x00, 0x03],
            ),
            (
                ModbusResponse::WriteMultipleRegisters {
                    starting_address: 0x0010,
                    quantity: 2,
                },
                vec![0x10, 0x00, 0x10, 0x00, 0x02],
            ),
        ];
        for (response, expected) in fixtures {
            assert_eq!(response.serialize().unwrap(), expected);
        }
    }
}
