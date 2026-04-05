# Tiny rust modbus library

Small Modbus library with request/response serialization and Modbus TCP transport.

## Library usage

Create a typed request and send it over Modbus TCP:

```rust
use std::net::{IpAddr, Ipv4Addr};

use tiny_mb::ModbusRequest;
use tiny_mb::ModbusResponse;
use tiny_mb::tcp::ModbusTcpConnection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let connection = ModbusTcpConnection::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 502, 1, 1);

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

The TCP client opens connections lazily, retries short-lived I/O failures, and can reuse the
same socket across multiple requests. Use `send_message_with_unit_id` when talking to multiple
devices behind one Modbus TCP gateway.

## Examples

The repository includes a `modbus_cli` example for interactive Modbus TCP reads and writes across all supported function codes.
Enable the `cli` feature when building or running it so the extra CLI-only dependencies are not pulled into library-only builds.

Show the CLI help:

```bash
cargo run --features cli --example modbus_cli -- --help
```

The command format is:

```text
modbus_cli [OPTIONS] <FUNCTION> <START_ADDRESS> <QUANTITY_OR_VALUES>...
```

- Read functions expect a quantity argument.
- Single-write functions expect one value.
- Multi-write functions accept one or more payload values.

Run the example against a local or remote Modbus TCP device:

```bash
cargo run --features cli --example modbus_cli -- read_coils 0 8
cargo run --features cli --example modbus_cli -- --address 192.168.1.10 read_holding 100 4
cargo run --features cli --example modbus_cli -- write_coil 12 on
cargo run --features cli --example modbus_cli -- --output hex write_register 200 4660
cargo run --features cli --example modbus_cli -- write_coils 16 1 0 1 1
cargo run --features cli --example modbus_cli -- write_registers 300 10 20 30
```
