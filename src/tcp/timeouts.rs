// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tinymb contributors

//! Timeout configuration for Modbus TCP operations.

use std::time::Duration;

pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-operation time limits used by [`crate::tcp::ModbusTcpConnection`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModbusTcpTimeouts {
    /// Maximum time allowed to establish a TCP connection.
    pub connect_timeout: Duration,
    /// Maximum time allowed to write one Modbus TCP frame.
    pub write_timeout: Duration,
    /// Maximum time allowed to read one Modbus TCP response.
    pub read_timeout: Duration,
}

impl Default for ModbusTcpTimeouts {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_timeouts_match_previous_behavior() {
        let timeouts = ModbusTcpTimeouts::default();

        assert_eq!(timeouts.connect_timeout, Duration::from_secs(5));
        assert_eq!(timeouts.write_timeout, Duration::from_secs(5));
        assert_eq!(timeouts.read_timeout, Duration::from_secs(5));
    }
}
