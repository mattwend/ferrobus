// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

//! Protocol-defined PDU limits, shared by request and response codecs.

pub(crate) const MAX_READ_COILS: u16 = 2000;
pub(crate) const MAX_READ_DISCRETE_INPUTS: u16 = 2000;
pub(crate) const MAX_READ_HOLDING_REGISTERS: u16 = 125;
pub(crate) const MAX_READ_INPUT_REGISTERS: u16 = 125;
pub(crate) const MAX_WRITE_MULTIPLE_COILS: u16 = 1968;
pub(crate) const MAX_WRITE_MULTIPLE_REGISTERS: u16 = 123;
