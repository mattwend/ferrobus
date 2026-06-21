// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Timeout configuration for Modbus TCP operations.

use std::time::Duration;

pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-operation time limits used by [`crate::tcp::ModbusTcpConnection`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModbusTcpTimeouts {
    /// Maximum time allowed to establish a TCP connection.
    pub connect_timeout: Duration,
    /// Maximum time allowed to write one Modbus TCP frame.
    pub write_timeout: Duration,
    /// Maximum time allowed to receive one Modbus TCP response after its frame is written.
    pub response_timeout: Duration,
}

impl Default for ModbusTcpTimeouts {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_timeouts_are_five_seconds() {
        let timeouts = ModbusTcpTimeouts::default();

        assert_eq!(timeouts.connect_timeout, Duration::from_secs(5));
        assert_eq!(timeouts.write_timeout, Duration::from_secs(5));
        assert_eq!(timeouts.response_timeout, Duration::from_secs(5));
    }
}
