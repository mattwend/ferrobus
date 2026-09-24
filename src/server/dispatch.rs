// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Transport-neutral request PDU dispatch.

use tracing::error;

use crate::request::RequestParseError;
use crate::response::FunctionCode;
use crate::server::ModbusServer;
use crate::{ExceptionCode, ModbusRequest, ModbusResponse};

/// Drive a [`ModbusServer`] from raw PDU bytes.
///
/// Returns response PDU bytes (including exception PDUs). The caller is
/// responsible for ADU framing.
pub(crate) async fn process_pdu<S: ModbusServer>(
    server: &S,
    unit_id: u8,
    pdu_bytes: &[u8],
) -> Vec<u8> {
    let request = match ModbusRequest::try_from(pdu_bytes) {
        Ok(request) => request,
        Err(error) => return parse_error_exception_pdu(pdu_bytes, &error),
    };
    let function_code = FunctionCode::normalize_for_exception(request.function_code());
    // Retain only the five scalar bytes the response has to agree with, so the
    // request (up to 123 registers) can be moved into the handler instead of cloned.
    let echo = request.echo();

    let response = match server.handle(unit_id, request).await {
        Ok(ModbusResponse::Exception { .. }) => {
            error!("ModbusServer returned a direct exception response");
            return exception_pdu(function_code, ExceptionCode::ServerDeviceFailure);
        }
        Ok(response) => response,
        Err(code) => return exception_pdu(function_code, code),
    };

    let response = match response.align_to_echo(echo) {
        Ok(response) => response,
        Err(error) => {
            error!(%error, "ModbusServer returned a mismatched response");
            return exception_pdu(function_code, ExceptionCode::ServerDeviceFailure);
        }
    };

    match response.serialize() {
        Ok(bytes) => bytes,
        Err(error) => {
            error!(%error, "failed to serialize Modbus server response");
            exception_pdu(function_code, ExceptionCode::ServerDeviceFailure)
        }
    }
}

fn parse_error_exception_pdu(pdu_bytes: &[u8], error: &RequestParseError) -> Vec<u8> {
    let raw = pdu_bytes.first().copied().unwrap_or(0);
    let code = match error {
        RequestParseError::UnknownFunctionCode(_) => ExceptionCode::IllegalFunction,
        RequestParseError::Truncated { .. }
        | RequestParseError::LengthMismatch { .. }
        | RequestParseError::ByteCountMismatch { .. }
        | RequestParseError::QuantityOutOfRange { .. }
        | RequestParseError::InvalidCoilValue(_) => ExceptionCode::IllegalDataValue,
    };
    exception_pdu(FunctionCode::normalize_for_exception(raw), code)
}

fn exception_pdu(function_code: FunctionCode, code: ExceptionCode) -> Vec<u8> {
    vec![u8::from(function_code) | 0x80, u8::from(code)]
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::server::InMemoryStore;

    #[tokio::test]
    async fn process_read_holding_registers() {
        let store = InMemoryStore::new(10, 10, 10, 10).unwrap();
        store.set_holding(0, 0x1234).unwrap();
        let response = process_pdu(&store, 1, &[0x03, 0, 0, 0, 1]).await;
        assert_eq!(response, vec![0x03, 0x02, 0x12, 0x34]);
    }

    #[tokio::test]
    async fn parse_errors_return_exceptions() {
        let store = InMemoryStore::new(1, 1, 1, 1).unwrap();
        assert_eq!(process_pdu(&store, 1, &[0x07]).await, vec![0x87, 0x01]);
        assert_eq!(process_pdu(&store, 1, &[]).await, vec![0x80, 0x01]);
        assert_eq!(process_pdu(&store, 1, &[0x80]).await, vec![0x80, 0x01]);
        assert_eq!(process_pdu(&store, 1, &[0x03, 0]).await, vec![0x83, 0x03]);
    }

    #[tokio::test]
    async fn out_of_range_returns_illegal_data_address() {
        let store = InMemoryStore::new(1, 1, 1, 1).unwrap();
        let response = process_pdu(&store, 1, &[0x03, 0, 1, 0, 1]).await;
        assert_eq!(response, vec![0x83, 0x02]);
    }
}
