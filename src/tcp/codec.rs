// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Modbus TCP MBAP codec used by the connection actor.

use std::io;

use bytes::BytesMut;
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

use crate::error::ModbusError;
use crate::tcp::frame::{MBAP_HEADER_LEN, response_body_len_from_header};

/// Error emitted by the MBAP codec.
#[derive(Debug, Error)]
pub(crate) enum MbapCodecError {
    /// The framed socket returned an I/O error.
    #[error("socket I/O error: {0}")]
    Io(#[from] io::Error),

    /// The codec detected a Modbus protocol error while decoding a frame.
    #[error(transparent)]
    Modbus(#[from] ModbusError),
}

/// Codec for complete Modbus TCP ADU frames.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MbapCodec;

impl Decoder for MbapCodec {
    type Item = Vec<u8>;
    type Error = MbapCodecError;

    /// Decodes one full Modbus TCP response frame from the input buffer.
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < MBAP_HEADER_LEN {
            return Ok(None);
        }

        let mut header = [0_u8; MBAP_HEADER_LEN];
        header.copy_from_slice(&src[..MBAP_HEADER_LEN]);
        let body_len = response_body_len_from_header(header)?;
        let frame_len = MBAP_HEADER_LEN + body_len;
        if src.len() < frame_len {
            return Ok(None);
        }

        Ok(Some(src.split_to(frame_len).to_vec()))
    }
}

impl Encoder<Vec<u8>> for MbapCodec {
    type Error = MbapCodecError;

    /// Encodes an already-built Modbus TCP ADU into the output buffer.
    fn encode(&mut self, item: Vec<u8>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.reserve(item.len());
        dst.extend_from_slice(&item);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decode_buffers_partial_frames_until_complete() {
        let mut codec = MbapCodec;
        let mut source = BytesMut::from(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x11][..]);

        assert!(codec.decode(&mut source).unwrap().is_none());

        source.extend_from_slice(&[0x01, 0x02]);
        let frame = codec.decode(&mut source).unwrap().unwrap();

        assert_eq!(
            frame,
            vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x11, 0x01, 0x02]
        );
        assert!(source.is_empty());
    }

    #[test]
    fn decode_rejects_malformed_mbap_length() {
        let mut codec = MbapCodec;
        let mut source = BytesMut::from(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x11][..]);

        let error = codec.decode(&mut source).unwrap_err();

        assert!(matches!(
            error,
            MbapCodecError::Modbus(ModbusError::MalformedResponse(_))
        ));
    }

    #[test]
    fn encode_appends_already_built_adu() {
        let mut codec = MbapCodec;
        let mut destination = BytesMut::new();

        codec
            .encode(vec![0x00, 0x01, 0x00, 0x00], &mut destination)
            .unwrap();

        assert_eq!(&destination[..], &[0x00, 0x01, 0x00, 0x00]);
    }
}
