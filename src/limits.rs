// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Protocol-defined PDU limits and function codes, shared by request and response codecs.

pub(crate) const READ_COILS: u8 = 0x01;
pub(crate) const READ_DISCRETE_INPUTS: u8 = 0x02;
pub(crate) const READ_HOLDING_REGISTERS: u8 = 0x03;
pub(crate) const READ_INPUT_REGISTERS: u8 = 0x04;
pub(crate) const WRITE_SINGLE_COIL: u8 = 0x05;
pub(crate) const WRITE_SINGLE_REGISTER: u8 = 0x06;
pub(crate) const WRITE_MULTIPLE_COILS: u8 = 0x0F;
pub(crate) const WRITE_MULTIPLE_REGISTERS: u8 = 0x10;

pub(crate) const MAX_READ_COILS: u16 = 2000;
pub(crate) const MAX_READ_DISCRETE_INPUTS: u16 = 2000;
pub(crate) const MAX_READ_HOLDING_REGISTERS: u16 = 125;
pub(crate) const MAX_READ_INPUT_REGISTERS: u16 = 125;
pub(crate) const MAX_WRITE_MULTIPLE_COILS: u16 = 1968;
pub(crate) const MAX_WRITE_MULTIPLE_REGISTERS: u16 = 123;
