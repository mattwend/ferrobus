// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! MBAP header constants and frame length validation.
//!
//! The same MBAP header precedes requests and responses, and this module is used
//! in both directions: the client codec sizes response bodies with it and the TCP
//! server sizes request bodies with it. Nothing here is direction-specific, so the
//! fixtures below cover both.

use crate::error::ModbusError;

pub(crate) const MBAP_HEADER_LEN: usize = 7;
pub(crate) const MAX_MODBUS_TCP_FRAME: usize = 260;

/// Returns the PDU length encoded by an MBAP header.
///
/// # Arguments
/// * `header` - Seven-byte MBAP header read from the TCP stream.
///
/// # Returns
/// Returns the number of bytes following the MBAP header, excluding the unit identifier.
///
/// # Errors
/// Returns [`ModbusError::MalformedResponse`] when the MBAP length is invalid or oversized.
pub(crate) fn body_len_from_header(header: [u8; MBAP_HEADER_LEN]) -> Result<usize, ModbusError> {
    let pdu_length = u16::from_be_bytes([header[4], header[5]]) as usize;
    if pdu_length < 2 {
        return Err(ModbusError::MalformedResponse(
            "Invalid MBAP length: missing unit identifier or PDU".to_string(),
        ));
    }

    let body_len = pdu_length - 1;
    let total_length = MBAP_HEADER_LEN + body_len;
    if total_length > MAX_MODBUS_TCP_FRAME {
        return Err(ModbusError::MalformedResponse(format!(
            "Frame exceeds maximum frame size: {total_length} > {MAX_MODBUS_TCP_FRAME}"
        )));
    }

    Ok(body_len)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn body_len_excludes_unit_id() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x11];
        let body_len = body_len_from_header(header).unwrap();
        assert_eq!(body_len, 5);
    }

    #[test]
    fn body_len_rejects_zero_length() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x11];
        let error = body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    #[test]
    fn body_len_minimum_valid() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x11];
        let body_len = body_len_from_header(header).unwrap();
        assert_eq!(body_len, 1);
    }

    #[test]
    fn body_len_maximum_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0xFE, 0x11];
        let body_len = body_len_from_header(header).unwrap();
        assert_eq!(body_len, 253);
    }

    #[test]
    fn body_len_rejects_oversized_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x11];
        let error = body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("exceeds maximum frame size"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    #[test]
    fn body_len_single_byte_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x11];
        let body_len = body_len_from_header(header).unwrap();
        assert_eq!(body_len, 2);
    }

    #[test]
    fn body_len_rejects_empty_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x11];
        let error = body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    /// Server direction: the header that precedes a request PDU is parsed by the
    /// exact same code path as a response header, so pin the request fixtures the
    /// TCP server actually sees.
    #[test]
    fn body_len_covers_server_direction_request_headers() {
        let fixtures = [
            // Read holding registers: fc + address + quantity = 5 PDU bytes.
            ([0x00u8, 0x01, 0x00, 0x00, 0x00, 0x06, 0x01], 5usize),
            // Write single coil, unit id 0 (broadcast-style addressing).
            ([0x12, 0x34, 0x00, 0x00, 0x00, 0x06, 0x00], 5),
            // Write multiple registers of two registers: 6 + 4 PDU bytes.
            ([0xFF, 0xFF, 0x00, 0x00, 0x00, 0x0B, 0xFF], 10),
            // Largest legal request PDU (253 bytes).
            ([0x00, 0x01, 0x00, 0x00, 0x00, 0xFE, 0x01], 253),
        ];
        for (header, expected) in fixtures {
            assert_eq!(
                body_len_from_header(header).unwrap(),
                expected,
                "header {header:02X?}"
            );
        }
    }

    /// The header the server writes must be readable by the same helper, for
    /// every response the server can produce.
    #[test]
    fn body_len_matches_the_adu_builder_in_both_directions() {
        use crate::tcp::adu::build_modbus_tcp_adu_from_pdu_bytes;

        for pdu in [
            vec![0x83u8, 0x02],                 // exception
            vec![0x03, 0x02, 0x12, 0x34],       // one register
            vec![0x0F, 0x00, 0x10, 0x00, 0x03], // write ack
            vec![0x01, 0x02, 0xFF, 0x01],       // packed coils
            std::iter::once(0x03)
                .chain(std::iter::once(250))
                .chain(std::iter::repeat_n(0, 250))
                .collect(), // max PDU
        ] {
            let frame = build_modbus_tcp_adu_from_pdu_bytes(0x0001, 0x11, &pdu).unwrap();
            let header: [u8; MBAP_HEADER_LEN] = frame[..MBAP_HEADER_LEN].try_into().unwrap();
            assert_eq!(body_len_from_header(header).unwrap(), pdu.len());
            assert_eq!(&frame[MBAP_HEADER_LEN..], pdu.as_slice());
        }
    }
}
