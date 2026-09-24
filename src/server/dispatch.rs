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
    use std::future::Future;
    use std::sync::Mutex;

    /// Store seeded with known values, rebuilt for every fixture so mutating
    /// requests cannot leak into later cases.
    fn seeded_store() -> InMemoryStore {
        let store = InMemoryStore::new(16, 16, 16, 16).unwrap();
        store.set_coil(0, true).unwrap();
        store.set_coil(2, true).unwrap();
        store.set_discrete(1, true).unwrap();
        store.set_holding(0, 0x1234).unwrap();
        store.set_input(0, 0xABCD).unwrap();
        store
    }

    /// Request PDU -> response PDU fixtures covering every supported function
    /// code and every parse-error class the dispatch maps to an exception.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn dispatch_fixtures() {
        let fixtures: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
            // --- happy path, one per function code -------------------------
            (
                "read one coil",
                vec![0x01, 0x00, 0x00, 0x00, 0x01],
                vec![0x01, 0x01, 0x01],
            ),
            (
                "read coils spanning two payload bytes",
                vec![0x01, 0x00, 0x00, 0x00, 0x09],
                vec![0x01, 0x02, 0x05, 0x00],
            ),
            (
                "read discrete inputs",
                vec![0x02, 0x00, 0x00, 0x00, 0x02],
                vec![0x02, 0x01, 0x02],
            ),
            (
                "read holding registers",
                vec![0x03, 0x00, 0x00, 0x00, 0x01],
                vec![0x03, 0x02, 0x12, 0x34],
            ),
            (
                "read input registers",
                vec![0x04, 0x00, 0x00, 0x00, 0x01],
                vec![0x04, 0x02, 0xAB, 0xCD],
            ),
            (
                "write single coil on echoes the request",
                vec![0x05, 0x00, 0x04, 0xFF, 0x00],
                vec![0x05, 0x00, 0x04, 0xFF, 0x00],
            ),
            (
                "write single coil off echoes the request",
                vec![0x05, 0x00, 0x00, 0x00, 0x00],
                vec![0x05, 0x00, 0x00, 0x00, 0x00],
            ),
            (
                "write single register echoes the request",
                vec![0x06, 0x00, 0x01, 0x55, 0xAA],
                vec![0x06, 0x00, 0x01, 0x55, 0xAA],
            ),
            (
                "write multiple coils acknowledges the quantity",
                vec![0x0F, 0x00, 0x00, 0x00, 0x03, 0x01, 0x05],
                vec![0x0F, 0x00, 0x00, 0x00, 0x03],
            ),
            (
                "write multiple registers acknowledges the quantity",
                vec![0x10, 0x00, 0x00, 0x00, 0x02, 0x04, 0x11, 0x11, 0x22, 0x22],
                vec![0x10, 0x00, 0x00, 0x00, 0x02],
            ),
            // --- exception paths -------------------------------------------
            ("unknown function code", vec![0x07], vec![0x87, 0x01]),
            (
                "empty PDU has no function code to echo",
                vec![],
                vec![0x80, 0x03],
            ),
            (
                "function code 0x00 is not a valid request",
                vec![0x00],
                vec![0x80, 0x01],
            ),
            (
                "exception-flagged function code is normalized to zero",
                vec![0x80],
                vec![0x80, 0x01],
            ),
            ("truncated read request", vec![0x03, 0x00], vec![0x83, 0x03]),
            (
                "trailing byte after a complete read request",
                vec![0x01, 0x00, 0x00, 0x00, 0x01, 0x00],
                vec![0x81, 0x03],
            ),
            (
                "zero quantity",
                vec![0x03, 0x00, 0x00, 0x00, 0x00],
                vec![0x83, 0x03],
            ),
            (
                "quantity above the protocol limit",
                vec![0x03, 0x00, 0x00, 0x00, 0x7E],
                vec![0x83, 0x03],
            ),
            (
                "byte count disagrees with quantity",
                vec![0x0F, 0x00, 0x00, 0x00, 0x09, 0x01, 0x00],
                vec![0x8F, 0x03],
            ),
            (
                "write single coil value is neither ON nor OFF",
                vec![0x05, 0x00, 0x00, 0x12, 0x34],
                vec![0x85, 0x03],
            ),
            (
                "read past the end of the table",
                vec![0x03, 0x00, 0x10, 0x00, 0x01],
                vec![0x83, 0x02],
            ),
            (
                "write past the end of the table",
                vec![0x10, 0x00, 0x0F, 0x00, 0x02, 0x04, 0x00, 0x01, 0x00, 0x02],
                vec![0x90, 0x02],
            ),
        ];

        for (name, request, expected) in fixtures {
            let store = seeded_store();
            let response = process_pdu(&store, 1, &request).await;
            assert_eq!(response, expected, "{name}");
        }
    }

    /// Handler that returns a scripted result regardless of the request, used to
    /// exercise the `ModbusServer` contract violations.
    struct ScriptedServer {
        result: Result<ModbusResponse, ExceptionCode>,
        seen_unit_id: Mutex<Option<u8>>,
    }

    impl ScriptedServer {
        fn new(result: Result<ModbusResponse, ExceptionCode>) -> Self {
            Self {
                result,
                seen_unit_id: Mutex::new(None),
            }
        }
    }

    impl ModbusServer for ScriptedServer {
        fn handle(
            &self,
            unit_id: u8,
            _request: ModbusRequest,
        ) -> impl Future<Output = Result<ModbusResponse, ExceptionCode>> + Send {
            if let Ok(mut seen) = self.seen_unit_id.lock() {
                *seen = Some(unit_id);
            }
            let result = self.result.clone();
            async move { result }
        }
    }

    const READ_ONE_HOLDING: [u8; 5] = [0x03, 0x00, 0x00, 0x00, 0x01];

    #[tokio::test]
    async fn handler_returning_a_direct_exception_is_a_contract_violation() {
        // Implementors must return `Err(ExceptionCode)`; building the exception
        // PDU themselves is rejected rather than forwarded.
        let server = ScriptedServer::new(Ok(ModbusResponse::Exception {
            function_code: FunctionCode::normalize_for_exception(0x03),
            code: ExceptionCode::IllegalDataAddress,
        }));
        assert_eq!(
            process_pdu(&server, 1, &READ_ONE_HOLDING).await,
            vec![0x83, 0x04]
        );
    }

    #[tokio::test]
    async fn handler_returning_a_mismatched_variant_is_a_contract_violation() {
        let server = ScriptedServer::new(Ok(ModbusResponse::ReadCoils { coils: vec![true] }));
        assert_eq!(
            process_pdu(&server, 1, &READ_ONE_HOLDING).await,
            vec![0x83, 0x04]
        );
    }

    #[tokio::test]
    async fn handler_returning_too_many_values_is_a_contract_violation() {
        let server = ScriptedServer::new(Ok(ModbusResponse::ReadHoldingRegisters {
            registers: vec![0x1111; 126],
        }));
        assert_eq!(
            process_pdu(&server, 1, &READ_ONE_HOLDING).await,
            vec![0x83, 0x04]
        );
    }

    #[tokio::test]
    async fn handler_returning_too_few_values_is_a_contract_violation() {
        let server = ScriptedServer::new(Ok(ModbusResponse::ReadHoldingRegisters {
            registers: Vec::new(),
        }));
        assert_eq!(
            process_pdu(&server, 1, &READ_ONE_HOLDING).await,
            vec![0x83, 0x04]
        );
    }

    #[tokio::test]
    async fn handler_exception_codes_are_forwarded_verbatim() {
        for code in [
            ExceptionCode::IllegalFunction,
            ExceptionCode::IllegalDataAddress,
            ExceptionCode::IllegalDataValue,
            ExceptionCode::ServerDeviceFailure,
            ExceptionCode::ServerDeviceBusy,
            ExceptionCode::GatewayPathUnavailable,
            ExceptionCode::Unknown(0x42),
        ] {
            let server = ScriptedServer::new(Err(code));
            assert_eq!(
                process_pdu(&server, 1, &READ_ONE_HOLDING).await,
                vec![0x83, u8::from(code)],
                "{code}"
            );
        }
    }

    #[tokio::test]
    async fn unit_id_is_passed_through_to_the_handler() {
        let server = ScriptedServer::new(Ok(ModbusResponse::ReadHoldingRegisters {
            registers: vec![0x1111],
        }));
        let _response = process_pdu(&server, 0x2A, &READ_ONE_HOLDING).await;
        assert_eq!(*server.seen_unit_id.lock().unwrap(), Some(0x2A));
    }
}
