# tiny-mb

[![CI](https://github.com/mattwend/tiny-mb/actions/workflows/ci.yml/badge.svg)](https://github.com/mattwend/tiny-mb/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/mattwend/tiny-mb/branch/v0.x.x/graph/badge.svg)](https://codecov.io/gh/mattwend/tiny-mb)

Small Rust Modbus library with typed request/response PDUs and a reusable Modbus TCP transport.

## Features

- Typed `ModbusRequest` and `ModbusResponse` enums for the common Modbus function codes
- Request serialization and response parsing for:
  - read coils
  - read discrete inputs
  - read holding registers
  - read input registers
  - write single coil
  - write single register
  - write multiple coils
  - write multiple registers
- Modbus TCP transport with:
  - lazy connect or explicit `connect()`
  - reusable actor-owned connections with bounded request-channel backpressure
  - configurable in-flight window for slow gateway-backed buses
  - retry of transient I/O failures and gateway-busy exception responses
  - per-phase timeouts for connect, write, queue wait, and on-wire response wait
  - request/response validation for transaction ID, protocol ID, unit ID, and echoed payloads
- Support for talking to multiple unit IDs through one Modbus TCP connection handle
- Optional CLI example behind the `cli` feature

## Prerequisites

- Rust 1.85 or newer (MSRV), matching the crate's Rust 2024 edition.

## Installation

Add the crate to your project:

```toml
[dependencies]
tiny-mb = "0.1.0"
```

## Library usage

Create a typed request and send it over Modbus TCP:

```rust
use tiny_mb::tcp::ModbusTcpConnection;
use tiny_mb::{ModbusRequest, ModbusResponse};

#[tokio::main]
async fn main() -> Result<(), tiny_mb::ModbusError> {
    let connection = ModbusTcpConnection::new("127.0.0.1", 502, 1, 1);

    let response = connection
        .send_message(&ModbusRequest::ReadHoldingRegisters {
            starting_address: 100,
            quantity: 2,
        })
        .await?;

    match response {
        ModbusResponse::ReadHoldingRegisters { registers } => {
            println!("registers: {registers:?}");
        }
        other => {
            println!("unexpected response: {other:?}");
        }
    }

    Ok(())
}
```

The TCP client accepts host names or numeric IP addresses, opens connections lazily, retries
short-lived I/O failures, and can reuse the same socket across multiple requests. Internally,
cloned handles send commands over a bounded channel to one actor task that owns the socket and
MBAP codec; this provides backpressure under
burst load. Cloned handles, including those returned by `with_unit_id`, may issue requests
concurrently; responses are matched by MBAP transaction ID. Writes are serialized by the actor and
a cancellation in one caller cannot interrupt a partially written Modbus TCP frame.

Use `send_message_with_unit_id` or `with_unit_id(...)` when talking to multiple devices behind one
Modbus TCP gateway.

## Timeouts and retries

`ModbusTcpConnection::new(...)` uses sensible defaults for connect, write, and read timeouts.
The write timeout covers both sending the frame and flushing the socket. If you need custom limits,
construct the connection with `with_timeouts(...)`.

```rust
use std::time::Duration;

use tiny_mb::tcp::{ModbusTcpConnection, ModbusTcpFlowControl, ModbusTcpRetry, ModbusTcpTimeouts};

let connection = ModbusTcpConnection::with_config(
    "localhost",
    502,
    1,
    1,
    ModbusTcpTimeouts {
        connect_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        response_timeout: Duration::from_secs(2),
    },
    ModbusTcpFlowControl::serial_gateway(),
)
.with_retry(Some(ModbusTcpRetry {
    initial_delay: Duration::from_millis(100),
    max_elapsed: Duration::from_secs(1),
    ..ModbusTcpRetry::default()
}));
```

Flow control is actor-owned: `ModbusTcpFlowControl::max_in_flight` caps requests on the wire,
and `max_queue_depth` is the bounded backlog that applies backpressure to callers. Use
`ModbusTcpFlowControl::serial_gateway()` for RTU gateways that drain one serial bus sequentially.
Queue wait is bounded by `queue_timeout`; the `response_timeout` clock starts only after the actor
successfully writes the frame to the socket. A single response timeout now fails that transaction,
quarantines its transaction ID to avoid late-response aliasing, and keeps the TCP socket open.

By default, transient TCP connect/write/read/queue failures and gateway-busy exception responses
(`0x05`, `0x06`, `0x0A`, `0x0B`) are retried with exponential backoff starting at 500 ms,
multiplied by 1.5, with jitter, and bounded only by `max_elapsed` (2 s). Set
`max_times` to cap the number of retries as well. To disable same-call retry, pass
`with_retry(None)`; the first transient error is returned to the caller for that call, but the
connection state is still invalidated and the next call reconnects lazily.

## Error handling

Transport and protocol failures are reported with cloneable `ModbusError` values, including:

- connect, write, and read errors/timeouts
  - I/O error variants carry `Arc<std::io::Error>`; pattern matching still lets callers bind the error and call `err.kind()` through `Arc` deref.
- malformed or invalid responses
- Modbus exception responses
- transaction ID, protocol ID, and unit ID mismatches
- request/response mismatches
- request validation errors
- invalid retry configuration as `ValidationError` (not retried)

## CLI example

The repository includes a `modbus_cli` example for interactive Modbus TCP reads and writes across
all supported function codes.

Enable the `cli` feature when building or running it so the extra CLI-only dependencies are not
pulled into library-only builds.

Show the CLI help:

```bash
cargo run --features cli --example modbus_cli -- --help
```

The CLI is organized as nested read/write subcommands:

```text
modbus_cli [OPTIONS] read <coils|discrete|holding|input> <ADDRESS> <QUANTITY>
modbus_cli [OPTIONS] write <coil|register|coils|registers> <ADDRESS> <VALUE...>
```

Examples:

```bash
cargo run --features cli --example modbus_cli -- read coils 0 8
cargo run --features cli --example modbus_cli -- --address 192.168.1.10 read holding 100 4
cargo run --features cli --example modbus_cli -- write coil 12 on
cargo run --features cli --example modbus_cli -- --output hex write register 200 4660
cargo run --features cli --example modbus_cli -- write coils 16 1 0 1 1
cargo run --features cli --example modbus_cli -- write registers 300 10 20 30
cargo run --features cli --example modbus_cli -- -a 192.168.0.89 write register 5004 6000
```

The CLI accepts:

- coil values as `1`, `0`, `true`, `false`, `on`, or `off`
- register output in `decimal` or `hex`
- global connection options such as `--address`, `--port`, `--unit-id`, and `--transaction-id`
