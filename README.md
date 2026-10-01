# ferrobus

[![CI](https://github.com/mattwend/ferrobus/actions/workflows/ci.yml/badge.svg)](https://github.com/mattwend/ferrobus/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/mattwend/ferrobus/branch/v0.x.x/graph/badge.svg)](https://codecov.io/gh/mattwend/ferrobus)

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
  - two-phase `ModbusTcpSocket` → `ModbusTcpConnection` construction
  - eager, fallible first TCP connect followed by lazy reconnect after transport failures
  - `ModbusTcpSocket::spawn` for a live handle built without dialing, when the caller decides
    when the first connect happens
  - connection lifecycle on the live handle: `connect`, `disconnect`, and a subscribable
    `ConnectionStatus` with a monotonic generation counter
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
ferrobus = "0.1.0"
```

Releases and breaking changes are recorded in [CHANGELOG.md](CHANGELOG.md).

## Library usage

Create a typed request and send it over Modbus TCP:

```rust
use ferrobus::tcp::ModbusTcpSocket;
use ferrobus::{ModbusRequest, ModbusResponse};

#[tokio::main]
async fn main() -> Result<(), ferrobus::ModbusError> {
    let connection = ModbusTcpSocket::new("127.0.0.1", 502, 1).connect().await?;

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

The TCP client accepts host names or numeric IP addresses. `ModbusTcpSocket::connect(...)`
is async and fallible: it opens the first TCP socket eagerly, then the live actor handle retries
short-lived I/O failures and reconnects lazily after transport teardown. Internally, cloned
handles send commands over a bounded channel to one actor task that owns the socket and MBAP codec;
this provides backpressure under burst load. Cloned handles, including those returned by
`with_unit_id`, may issue requests concurrently; responses are matched by MBAP transaction ID.
Writes are serialized by the actor and
a cancellation in one caller cannot interrupt a partially written Modbus TCP frame.

Use `send_message_with_unit_id` or `with_unit_id(...)` when talking to multiple devices behind one
Modbus TCP gateway.

## Constructing a connection

`ModbusTcpSocket` is the only entry point. It holds construction-time settings and offers two
consuming methods that both spawn the background actor and return the same cloneable
`ModbusTcpConnection` handle; only the timing of the first dial differs.

```rust,no_run
use ferrobus::tcp::ModbusTcpSocket;

# async fn eager() -> Result<(), ferrobus::ModbusError> {
// Eager: dials before returning, so it fails when the peer is unreachable.
let connection = ModbusTcpSocket::new("127.0.0.1", 502, 1).connect().await?;
# Ok(())
# }

# async fn lifecycle_managed() -> Result<(), ferrobus::ModbusError> {
// Lifecycle-managed: actor and handle exist before the first dial.
let connection = ModbusTcpSocket::new("127.0.0.1", 502, 1).spawn()?;
let mut status = connection.watch_status();

// Later, when lifecycle policy permits dialing:
connection.connect().await?;
# Ok(())
# }
```

`connect().await` is defined as `spawn()?` followed by the handle's `connect().await`, so both
paths share validation, actor construction, and dial behavior. The difference that matters when
the peer is not yet reachable: the eager form returns nothing on a failed first dial, discarding
the actor it just spawned, while `spawn()` lets callers keep the handle, subscribe to its status,
and retry through the same actor. `spawn()` must be called from within a Tokio runtime; it does
not resolve the host and does not dial.

## Connection lifecycle

The live `ModbusTcpConnection` handle owns the connection lifecycle. It is a handle to one
background actor that owns the socket; clones and `with_unit_id(...)` derivations share that actor.

- `connect().await` opens the actor's socket. It is idempotent: if a socket is already open, the
  actor acknowledges without dialing again. It reuses the existing actor, so no second actor is
  spawned.
- `disconnect().await` drops the current socket and fails pending requests, but keeps the actor
  alive.
- `status()` returns a `ConnectionStatus` snapshot; `watch_status()` returns a
  `tokio::sync::watch::Receiver<ConnectionStatus>` so callers can await transitions instead of
  polling. `is_connected().await` remains available as `status().connected`.

Reconnection is on-demand: any read/write/EOF failure tears the socket down, and the next request
to reach the actor opens a fresh one.

### Generation contract

`ConnectionStatus::generation` counts successful socket establishments on one actor:

| event | status |
| --- | --- |
| before the first connect | `{ connected: false, generation: 0 }` |
| after the first successful connect | `{ connected: true, generation: 1 }` |
| after a teardown | `{ connected: false, generation: 1 }` |
| after a failed connect attempt | unchanged |
| after the next successful connect | `{ connected: true, generation: 2 }` |

The counter never decreases, so a changed `generation` means the socket was replaced — a `bool`
connection flag collapses `true → false → true` and cannot express this.

If your application negotiates device state over the connection (word order, scaling calibration,
arming a device-side watchdog), cache the generation alongside that state and re-negotiate when it
moves — the device may have rebooted underneath you:

```rust,no_run
use ferrobus::tcp::ModbusTcpConnection;

struct Session {
    connection: ModbusTcpConnection,
    negotiated_at: u64,
}

impl Session {
    async fn ensure_negotiated(&mut self) -> Result<(), ferrobus::ModbusError> {
        let status = self.connection.status();
        if !status.connected {
            self.connection.connect().await?;
        }
        let generation = self.connection.status().generation;
        if generation != self.negotiated_at {
            // the socket was replaced: re-run device negotiation here
            self.negotiated_at = generation;
        }
        Ok(())
    }
}
```

To react to transitions instead of polling:

```rust,no_run
use ferrobus::tcp::ModbusTcpConnection;

# async fn run(connection: ModbusTcpConnection) {
let mut status = connection.watch_status();
while status.changed().await.is_ok() {
    let current = *status.borrow_and_update();
    println!("connected={} generation={}", current.connected, current.generation);
}
# }
```

## Word order for wide values

Modbus protocol fields, MBAP headers, addresses, quantities, and each individual 16-bit register
are always encoded big-endian on the wire. `ferrobus::WordOrder` only controls how already-decoded
consecutive registers are combined into 32-bit and 64-bit application values.

```rust
use ferrobus::WordOrder;

let registers = [0x0000, 0x3F80];
let value = WordOrder::LittleEndian.decode_f32(registers);
assert_eq!(value.to_bits(), 1.0f32.to_bits());
```

Use `WordOrder::BigEndian` (the default) for most-significant-word-first devices and
`WordOrder::LittleEndian` for least-significant-word-first devices. Decode helpers are available for
`u32`, `i32`, `f32`, `u64`, `i64`, and `f64`, including block decode methods that validate register
counts.

## Timeouts and retries

`ModbusTcpSocket::new(...)` uses sensible defaults for connect, write, and response timeouts.
The write timeout covers both sending the frame and flushing the socket. If you need custom limits,
configure the socket before calling `connect().await` or `spawn()`.

```rust
use std::time::Duration;

use ferrobus::tcp::{ModbusTcpFlowControl, ModbusTcpRetry, ModbusTcpSocket, ModbusTcpTimeouts};

let connection = ModbusTcpSocket::new("localhost", 502, 1)
    .with_timeouts(ModbusTcpTimeouts {
        connect_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        response_timeout: Duration::from_secs(2),
    })
    .with_flow_control(ModbusTcpFlowControl::serial_gateway())
    .with_retry(Some(ModbusTcpRetry {
        initial_delay: Duration::from_millis(100),
        max_elapsed: Duration::from_secs(1),
        ..ModbusTcpRetry::default()
    }))
    .connect()
    .await?;
```

`ModbusTcpFlowControl::max_in_flight` caps requests on the wire, and `max_queue_depth` is the
bounded backlog that applies backpressure to callers. Invalid flow-control settings return
`ModbusError::ValidationError` from both construction paths; use
`ModbusTcpFlowControl::serial_gateway()` for RTU gateways that drain one serial bus sequentially.

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
cargo run --features cli --example modbus_cli -- read holding 100 2 --as f32 --word-order little
cargo run --features cli --example modbus_cli -- write coil 12 on
cargo run --features cli --example modbus_cli -- --output hex write register 200 4660
cargo run --features cli --example modbus_cli -- write coils 16 1 0 1 1
cargo run --features cli --example modbus_cli -- write registers 300 10 20 30
cargo run --features cli --example modbus_cli -- -a 192.168.0.89 write register 5004 6000
```

The CLI accepts:

- coil values as `1`, `0`, `true`, `false`, `on`, or `off`
- register output in `decimal` or `hex`
- holding/input register reads can decode `u32`, `i32`, `f32`, `u64`, `i64`, or `f64` via
  `--value-type`/`--as`; `--word-order {big,little}` applies only to these wide read values and
  does not change Modbus wire byte order
- global connection options such as `--address`, `--port`, `--unit-id`, and `--transaction-id`
