// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! MBAP header constants and response frame length validation.

use crate::error::ModbusError;

pub(crate) const MBAP_HEADER_LEN: usize = 7;
pub(crate) const MAX_MODBUS_TCP_FRAME: usize = 260;

/// Returns the response body length encoded by an MBAP header.
///
/// # Arguments
/// * `header` - Seven-byte MBAP header read from the TCP stream.
///
/// # Returns
/// Returns the number of bytes following the MBAP header, excluding the unit identifier.
///
/// # Errors
/// Returns [`ModbusError::MalformedResponse`] when the MBAP length is invalid or oversized.
pub(crate) fn response_body_len_from_header(
    header: [u8; MBAP_HEADER_LEN],
) -> Result<usize, ModbusError> {
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
            "Response exceeds maximum frame size: {total_length} > {MAX_MODBUS_TCP_FRAME}"
        )));
    }

    Ok(body_len)
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn response_body_len_excludes_unit_id() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x11];
        let body_len = response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 5);
    }

    #[test]
    fn response_body_len_rejects_zero_length() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x11];
        let error = response_body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    #[test]
    fn response_body_len_minimum_valid() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x11];
        let body_len = response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 1);
    }

    #[test]
    fn response_body_len_maximum_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0xFE, 0x11];
        let body_len = response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 253);
    }

    #[test]
    fn response_body_len_rejects_oversized_frame() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x11];
        let error = response_body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("exceeds maximum frame size"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }

    #[test]
    fn response_body_len_single_byte_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x11];
        let body_len = response_body_len_from_header(header).unwrap();
        assert_eq!(body_len, 2);
    }

    #[test]
    fn response_body_len_rejects_empty_pdu() {
        let header = [0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x11];
        let error = response_body_len_from_header(header).unwrap_err();
        match error {
            ModbusError::MalformedResponse(message) => {
                assert!(message.contains("Invalid MBAP length"));
            }
            other => panic!("Expected MalformedResponse, got {other:?}"),
        }
    }
}
