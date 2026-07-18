// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Helpers for combining Modbus registers into wider scalar values.
//!
//! Modbus registers are always encoded as big-endian 16-bit values on the
//! wire. [`WordOrder`] controls only how consecutive host-order `u16`
//! registers are combined into 32-bit and 64-bit application values.

/// Word order for scalar values spanning multiple 16-bit registers.
///
/// # Examples
///
/// ```
/// use ferrobus::WordOrder;
///
/// let regs = [0x1234, 0x5678];
/// assert_eq!(WordOrder::BigEndian.decode_u32(regs), 0x1234_5678);
/// assert_eq!(WordOrder::LittleEndian.decode_u32(regs), 0x5678_1234);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordOrder {
    /// Most-significant word first (Modbus default for multi-register values).
    #[default]
    BigEndian,

    /// Least-significant word first.
    LittleEndian,
}

/// Error returned when a register block cannot be decoded into wide scalars.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WordOrderError {
    /// The register count is not an exact multiple of the scalar width.
    #[error("register block length {len} is not a multiple of {width} (for {type_name})")]
    InvalidBlockLength {
        /// Number of registers supplied.
        len: usize,

        /// Registers per scalar (2 for 32-bit, 4 for 64-bit).
        width: usize,

        /// Target scalar type name, e.g. `"f32"`.
        type_name: &'static str,
    },
}

impl WordOrder {
    /// Decodes an unsigned 32-bit integer from two 16-bit registers.
    ///
    /// The individual registers are already host-order `u16` values; this only
    /// controls whether the first register is the most-significant or
    /// least-significant word.
    #[must_use]
    pub fn decode_u32(self, regs: [u16; 2]) -> u32 {
        let [hi, lo] = self.order_32(regs);
        (u32::from(hi) << 16) | u32::from(lo)
    }

    /// Decodes a signed 32-bit integer from two 16-bit registers.
    #[must_use]
    pub fn decode_i32(self, regs: [u16; 2]) -> i32 {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "intentionally reinterpret u32 bits as signed"
        )]
        {
            self.decode_u32(regs) as i32
        }
    }

    /// Decodes an IEEE-754 32-bit float from two 16-bit registers.
    ///
    /// The bit pattern is preserved exactly with [`f32::from_bits`].
    #[must_use]
    pub fn decode_f32(self, regs: [u16; 2]) -> f32 {
        f32::from_bits(self.decode_u32(regs))
    }

    /// Decodes an unsigned 64-bit integer from four 16-bit registers.
    #[must_use]
    pub fn decode_u64(self, regs: [u16; 4]) -> u64 {
        let [w0, w1, w2, w3] = self.order_64(regs);
        (u64::from(w0) << 48) | (u64::from(w1) << 32) | (u64::from(w2) << 16) | u64::from(w3)
    }

    /// Decodes a signed 64-bit integer from four 16-bit registers.
    #[must_use]
    pub fn decode_i64(self, regs: [u16; 4]) -> i64 {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "intentionally reinterpret u64 bits as signed"
        )]
        {
            self.decode_u64(regs) as i64
        }
    }

    /// Decodes an IEEE-754 64-bit float from four 16-bit registers.
    ///
    /// The bit pattern is preserved exactly with [`f64::from_bits`].
    #[must_use]
    pub fn decode_f64(self, regs: [u16; 4]) -> f64 {
        f64::from_bits(self.decode_u64(regs))
    }

    /// Decodes a block of registers into unsigned 32-bit integers.
    ///
    /// The input length must be a multiple of two registers. Word order is
    /// applied independently to each two-register scalar.
    ///
    /// # Errors
    ///
    /// Returns [`WordOrderError::InvalidBlockLength`] if `regs.len()` is not a
    /// multiple of two.
    pub fn decode_u32_block(self, regs: &[u16]) -> Result<Vec<u32>, WordOrderError> {
        self.validate_block(regs.len(), 2, "u32")?;
        let mut values = Vec::with_capacity(regs.len() / 2);
        for chunk in regs.chunks_exact(2) {
            values.push(self.decode_u32([chunk[0], chunk[1]]));
        }
        Ok(values)
    }

    /// Decodes a block of registers into signed 32-bit integers.
    ///
    /// The input length must be a multiple of two registers. Word order is
    /// applied independently to each two-register scalar.
    ///
    /// # Errors
    ///
    /// Returns [`WordOrderError::InvalidBlockLength`] if `regs.len()` is not a
    /// multiple of two.
    pub fn decode_i32_block(self, regs: &[u16]) -> Result<Vec<i32>, WordOrderError> {
        self.validate_block(regs.len(), 2, "i32")?;
        let mut values = Vec::with_capacity(regs.len() / 2);
        for chunk in regs.chunks_exact(2) {
            values.push(self.decode_i32([chunk[0], chunk[1]]));
        }
        Ok(values)
    }

    /// Decodes a block of registers into IEEE-754 32-bit floats.
    ///
    /// The input length must be a multiple of two registers. Word order is
    /// applied independently to each two-register scalar.
    ///
    /// # Errors
    ///
    /// Returns [`WordOrderError::InvalidBlockLength`] if `regs.len()` is not a
    /// multiple of two.
    pub fn decode_f32_block(self, regs: &[u16]) -> Result<Vec<f32>, WordOrderError> {
        self.validate_block(regs.len(), 2, "f32")?;
        let mut values = Vec::with_capacity(regs.len() / 2);
        for chunk in regs.chunks_exact(2) {
            values.push(self.decode_f32([chunk[0], chunk[1]]));
        }
        Ok(values)
    }

    /// Decodes a block of registers into unsigned 64-bit integers.
    ///
    /// The input length must be a multiple of four registers. Word order is
    /// applied independently to each four-register scalar.
    ///
    /// # Errors
    ///
    /// Returns [`WordOrderError::InvalidBlockLength`] if `regs.len()` is not a
    /// multiple of four.
    pub fn decode_u64_block(self, regs: &[u16]) -> Result<Vec<u64>, WordOrderError> {
        self.validate_block(regs.len(), 4, "u64")?;
        let mut values = Vec::with_capacity(regs.len() / 4);
        for chunk in regs.chunks_exact(4) {
            values.push(self.decode_u64([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(values)
    }

    /// Decodes a block of registers into signed 64-bit integers.
    ///
    /// The input length must be a multiple of four registers. Word order is
    /// applied independently to each four-register scalar.
    ///
    /// # Errors
    ///
    /// Returns [`WordOrderError::InvalidBlockLength`] if `regs.len()` is not a
    /// multiple of four.
    pub fn decode_i64_block(self, regs: &[u16]) -> Result<Vec<i64>, WordOrderError> {
        self.validate_block(regs.len(), 4, "i64")?;
        let mut values = Vec::with_capacity(regs.len() / 4);
        for chunk in regs.chunks_exact(4) {
            values.push(self.decode_i64([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(values)
    }

    /// Decodes a block of registers into IEEE-754 64-bit floats.
    ///
    /// The input length must be a multiple of four registers. Word order is
    /// applied independently to each four-register scalar.
    ///
    /// # Errors
    ///
    /// Returns [`WordOrderError::InvalidBlockLength`] if `regs.len()` is not a
    /// multiple of four.
    pub fn decode_f64_block(self, regs: &[u16]) -> Result<Vec<f64>, WordOrderError> {
        self.validate_block(regs.len(), 4, "f64")?;
        let mut values = Vec::with_capacity(regs.len() / 4);
        for chunk in regs.chunks_exact(4) {
            values.push(self.decode_f64([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(values)
    }

    fn order_32(self, regs: [u16; 2]) -> [u16; 2] {
        match self {
            WordOrder::BigEndian => regs,
            WordOrder::LittleEndian => [regs[1], regs[0]],
        }
    }

    fn order_64(self, regs: [u16; 4]) -> [u16; 4] {
        match self {
            WordOrder::BigEndian => regs,
            WordOrder::LittleEndian => [regs[3], regs[2], regs[1], regs[0]],
        }
    }

    fn validate_block(
        self,
        len: usize,
        width: usize,
        type_name: &'static str,
    ) -> Result<(), WordOrderError> {
        let _ = self;
        if len % width == 0 {
            Ok(())
        } else {
            Err(WordOrderError::InvalidBlockLength {
                len,
                width,
                type_name,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WordOrder, WordOrderError};

    #[test]
    fn default_is_big_endian_and_traits_work() {
        let default = WordOrder::default();
        let cloned = default;
        assert_eq!(default, WordOrder::BigEndian);
        assert_eq!(cloned, default);
        assert_eq!(format!("{default:?}"), "BigEndian");
    }

    #[test]
    fn decodes_known_32_bit_vectors() {
        let regs = [0x1234, 0x5678];
        assert_eq!(WordOrder::BigEndian.decode_u32(regs), 0x1234_5678);
        assert_eq!(WordOrder::LittleEndian.decode_u32(regs), 0x5678_1234);
        assert_eq!(WordOrder::BigEndian.decode_i32([0xFFFF, 0xFFFF]), -1);
        assert_eq!(
            WordOrder::LittleEndian.decode_i32([0x0000, 0x8000]),
            i32::MIN
        );
        assert_eq!(
            WordOrder::BigEndian.decode_f32([0x3F80, 0x0000]).to_bits(),
            1.0f32.to_bits()
        );
        assert_eq!(
            WordOrder::LittleEndian
                .decode_f32([0x0000, 0x3F80])
                .to_bits(),
            1.0f32.to_bits()
        );
    }

    #[test]
    fn decodes_known_64_bit_vectors() {
        let regs = [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD];
        assert_eq!(WordOrder::BigEndian.decode_u64(regs), 0xAAAA_BBBB_CCCC_DDDD);
        assert_eq!(
            WordOrder::LittleEndian.decode_u64(regs),
            0xDDDD_CCCC_BBBB_AAAA
        );
        assert_eq!(
            WordOrder::BigEndian.decode_i64([0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF]),
            -1
        );
        assert_eq!(
            WordOrder::LittleEndian
                .decode_f64([0x0000, 0x0000, 0x0000, 0x3FF0])
                .to_bits(),
            1.0f64.to_bits()
        );
    }

    #[test]
    fn block_decoding_preserves_chunk_order() {
        let regs = [0x1111, 0x2222, 0x3333, 0x4444];
        assert_eq!(
            WordOrder::LittleEndian.decode_u32_block(&regs),
            Ok(vec![0x2222_1111, 0x4444_3333])
        );
        assert_eq!(
            WordOrder::BigEndian.decode_u32_block(&regs),
            Ok(vec![0x1111_2222, 0x3333_4444])
        );
    }

    #[test]
    fn block_helpers_accept_empty_slices() {
        assert_eq!(WordOrder::BigEndian.decode_u32_block(&[]), Ok(Vec::new()));
        assert_eq!(
            WordOrder::LittleEndian.decode_f64_block(&[]),
            Ok(Vec::new())
        );
    }

    #[test]
    fn block_decoding_rejects_bad_lengths() {
        assert_eq!(
            WordOrder::BigEndian.decode_f32_block(&[0x0000]),
            Err(WordOrderError::InvalidBlockLength {
                len: 1,
                width: 2,
                type_name: "f32",
            })
        );
        assert_eq!(
            WordOrder::LittleEndian.decode_i64_block(&[0x0000, 0x0001]),
            Err(WordOrderError::InvalidBlockLength {
                len: 2,
                width: 4,
                type_name: "i64",
            })
        );
    }

    #[test]
    fn all_block_variants_decode() {
        let order = WordOrder::LittleEndian;
        assert_eq!(order.decode_i32_block(&[0xFFFF, 0xFFFF]), Ok(vec![-1]));
        assert_eq!(
            order.decode_f32_block(&[0x0000, 0x3F80]).map(bits32),
            Ok(vec![1.0f32.to_bits()])
        );
        assert_eq!(
            order.decode_u64_block(&[4, 3, 2, 1]),
            Ok(vec![0x0001_0002_0003_0004])
        );
        assert_eq!(order.decode_i64_block(&[0xFFFF; 4]), Ok(vec![-1]));
        assert_eq!(
            order
                .decode_f64_block(&[0x0000, 0x0000, 0x0000, 0x3FF0])
                .map(bits64),
            Ok(vec![1.0f64.to_bits()])
        );
    }

    fn bits32(values: Vec<f32>) -> Vec<u32> {
        values.into_iter().map(f32::to_bits).collect()
    }

    fn bits64(values: Vec<f64>) -> Vec<u64> {
        values.into_iter().map(f64::to_bits).collect()
    }
}
