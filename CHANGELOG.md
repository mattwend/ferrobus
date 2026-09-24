<!--
SPDX-License-Identifier: MIT
Copyright (c) 2025 ferrobus contributors
-->

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the crate is below `1.0.0`, breaking changes are released in minor versions.

## [Unreleased]

### Added

- `ModbusTcpConnection::read_holding_registers`, `write_single_register`, and
  `write_multiple_registers` convenience methods for callers that only need the
  typed payload from the common request shapes.
- `ferrobus::WordOrder` for decoding consecutive registers into `u32`, `i32`,
  `f32`, `u64`, `i64`, and `f64`, including block decode helpers that validate
  register counts. Modbus wire byte order is unaffected: word order only controls
  how already-decoded 16-bit registers are combined.
- CLI support for wide reads through `--value-type`/`--as` and
  `--word-order {big,little}`.
- Connection lifecycle on the live `ModbusTcpConnection` handle:
  `connect().await` and `disconnect().await` open and close the actor's socket
  without replacing the actor.
- `ConnectionStatus` with a monotonic `generation` counter, observed through
  `ModbusTcpConnection::status()` and `ModbusTcpConnection::watch_status()`. A
  changed generation means the socket was replaced, which a boolean connection
  flag cannot express.
- `ModbusTcpSocket::spawn` for callers that must own a live handle before the
  peer is reachable. It validates, spawns the actor, and returns without
  resolving the host or dialing; the returned handle reports
  `{ connected: false, generation: 0 }`.
- `ModbusTcpServer`, `BoundModbusTcpServer`, and `ModbusTcpServerTimeouts`.
- `server::ModbusServer`, `InMemoryStore`, and `StoreError`.
- `RequestParseError` and `ModbusRequest::try_from(&[u8])`.
- `FunctionCode`, `InvalidFunctionCode`, and `ExceptionCode`.
- `ModbusResponse::serialize`.
- `ModbusError::BindError`.
- Server and custom-server examples that build with default features.

### Changed

- **Breaking:** `ModbusTcpSocket` is now the only entry point for constructing a
  `ModbusTcpConnection`. Replace `ModbusTcpConnection::connect(host, port, unit_id).await`
  with `ModbusTcpSocket::new(host, port, unit_id).connect().await`.
- **Breaking:** `ModbusTcpConnection::connect` is now the live-handle method
  `connect(&self)`, which opens the socket of the actor a handle already owns. It
  is idempotent and never spawns a second actor.
- **Breaking:** `ModbusResponse::Exception` and `ModbusError::ExceptionResponse`
  now carry `function_code: FunctionCode` and `code: ExceptionCode` instead of
  raw `u8` fields. Pattern-match the typed values and use
  `u8::from(function_code)` when the raw function code is needed.
- Client sockets set `TCP_NODELAY`. Modbus frames are small, so Nagle plus
  delayed ACK added latency to every request/response exchange.

### Removed

- **Breaking:** `ModbusTcpConnection::open`, the interim name the
  default-configuration constructor carried after the lifecycle rename. It was
  never part of a release; construct through `ModbusTcpSocket` instead.

### Fixed

- Reject malformed padding in Modbus responses instead of accepting the frame.
- Queued requests whose caller stopped waiting (dropped future, elapsed caller
  deadline, cancelled task) are now discarded instead of transmitted. Previously
  cancellation was only observed for requests already on the wire, so a caller
  that gave up while its request was queued could still have it executed by the
  device — a duplicate-write hazard once the caller retried.

## [0.1.0] - 2026-06-21

### Added

- Initial release: typed `ModbusRequest` and `ModbusResponse` PDUs for the common
  function codes, and an actor-based Modbus TCP transport with bounded
  backpressure, a configurable in-flight window, per-phase timeouts, same-call
  retry of transient failures and gateway-busy exceptions, and response
  validation for transaction ID, protocol ID, unit ID, and echoed payloads.
- Optional `modbus_cli` example behind the `cli` feature.

[Unreleased]: https://github.com/mattwend/ferrobus/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mattwend/ferrobus/releases/tag/v0.1.0
