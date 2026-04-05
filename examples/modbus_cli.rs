// SPDX-License-Identifier: MIT
// Copyright (c) 2025 tiny-mb contributors

use clap::{Parser, ValueEnum};
use std::error::Error;
use std::net::IpAddr;
use tracing::info;
use tracing_subscriber::{filter::EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use tiny_mb::ModbusError;
use tiny_mb::ModbusRequest;
use tiny_mb::ModbusResponse;
use tiny_mb::tcp::ModbusTcpConnection;

#[derive(Parser, Debug)]
#[command(name = "modbus_cli")]
#[command(about = "Modbus TCP client for tiny-mb library")]
#[command(long_about = "Connects to a Modbus TCP device and issues one request at a time.")]
#[command(after_help = "Examples:\n  \
modbus_cli --address 192.168.1.10 read_holding 100 4\n  \
modbus_cli write_coil 12 on\n  \
modbus_cli --output hex write_register 200 4660\n  \
modbus_cli write_coils 16 1 0 1 1\n  \
modbus_cli write_registers 300 10 20 30\n  \
modbus_cli -a 192.168.0.89 write_register 5004 6000")]
struct Cli {
    #[arg(
        short = 'a',
        long,
        default_value = "127.0.0.1",
        help = "Target device IP address"
    )]
    address: IpAddr,

    #[arg(short = 'p', long, default_value = "502", help = "Modbus TCP port")]
    port: u16,

    #[arg(
        short = 'u',
        long,
        default_value = "1",
        help = "Modbus unit identifier"
    )]
    unit_id: u8,

    #[arg(
        short = 't',
        long,
        default_value = "1",
        help = "Transaction identifier"
    )]
    transaction_id: u16,

    #[arg(
        short = 'o',
        long,
        default_value = "decimal",
        help = "Output format for register values"
    )]
    output: OutputFormat,

    #[arg(help = "Modbus function to execute")]
    function: Function,

    #[arg(
        value_name = "START_ADDRESS",
        help = "Starting register/coil address for the operation"
    )]
    start_address: u16,

    #[arg(
        value_name = "QUANTITY_OR_VALUES",
        help = "Read quantity or write payload values, depending on the function"
    )]
    values: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Function {
    #[clap(name = "read_coils")]
    ReadCoils,
    #[clap(name = "read_discrete")]
    ReadDiscreteInputs,
    #[clap(name = "read_holding")]
    ReadHoldingRegisters,
    #[clap(name = "read_input")]
    ReadInputRegisters,
    #[clap(name = "write_coil")]
    WriteSingleCoil,
    #[clap(name = "write_register")]
    WriteSingleRegister,
    #[clap(name = "write_coils")]
    WriteMultipleCoils,
    #[clap(name = "write_registers")]
    WriteMultipleRegisters,
}

impl Function {
    fn cli_name(self) -> &'static str {
        match self {
            Function::ReadCoils => "read_coils",
            Function::ReadDiscreteInputs => "read_discrete",
            Function::ReadHoldingRegisters => "read_holding",
            Function::ReadInputRegisters => "read_input",
            Function::WriteSingleCoil => "write_coil",
            Function::WriteSingleRegister => "write_register",
            Function::WriteMultipleCoils => "write_coils",
            Function::WriteMultipleRegisters => "write_registers",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Function::ReadCoils => "ReadCoils",
            Function::ReadDiscreteInputs => "ReadDiscreteInputs",
            Function::ReadHoldingRegisters => "ReadHoldingRegisters",
            Function::ReadInputRegisters => "ReadInputRegisters",
            Function::WriteSingleCoil => "WriteSingleCoil",
            Function::WriteSingleRegister => "WriteSingleRegister",
            Function::WriteMultipleCoils => "WriteMultipleCoils",
            Function::WriteMultipleRegisters => "WriteMultipleRegisters",
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0x01 => Some(Function::ReadCoils),
            0x02 => Some(Function::ReadDiscreteInputs),
            0x03 => Some(Function::ReadHoldingRegisters),
            0x04 => Some(Function::ReadInputRegisters),
            0x05 => Some(Function::WriteSingleCoil),
            0x06 => Some(Function::WriteSingleRegister),
            0x0F => Some(Function::WriteMultipleCoils),
            0x10 => Some(Function::WriteMultipleRegisters),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum OutputFormat {
    #[default]
    #[clap(name = "decimal")]
    Decimal,
    #[clap(name = "hex")]
    Hex,
}

/// Accepts common operator-friendly coil spellings for write commands.
fn parse_coil_value(s: &str) -> Result<bool, String> {
    match s {
        "1" | "true" | "on" => Ok(true),
        "0" | "false" | "off" => Ok(false),
        _ => Err(format!("invalid coil value: {s}, expected 0 or 1")),
    }
}

fn parse_coil_values(values: &[String]) -> Result<Vec<bool>, String> {
    values.iter().map(|s| parse_coil_value(s)).collect()
}

/// Validates register values before they are sent in write requests.
fn parse_register_value(s: &str) -> Result<u16, String> {
    s.parse::<u16>()
        .map_err(|_| format!("invalid register value: {s}, expected u16"))
}

fn parse_register_values(values: &[String]) -> Result<Vec<u16>, String> {
    values.iter().map(|s| parse_register_value(s)).collect()
}

/// Enforces the read-command convention of `<start_address> <quantity>`.
fn parse_quantity(values: &[String], function: Function) -> Result<u16, String> {
    let quantity = values.first().ok_or(format!(
        "{} requires <quantity> argument",
        function.cli_name()
    ))?;
    quantity
        .parse::<u16>()
        .map_err(|_| format!("invalid quantity: {quantity}"))
}

fn format_register(value: u16, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{value:04X}"),
    }
}

fn format_coil(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

/// Formats exception responses for operators diagnosing bad addresses or unsupported functions.
fn format_exception(function: u8, code: u8) -> String {
    let function_name = Function::from_code(function)
        .map(Function::display_name)
        .unwrap_or("Unknown");
    let exception_name = match code {
        0x01 => "IllegalFunction",
        0x02 => "IllegalDataAddress",
        0x03 => "IllegalDataValue",
        0x04 => "ServerFailure",
        0x05 => "Acknowledge",
        0x06 => "ServerBusy",
        0x08 => "MemoryParityError",
        0x0A => "GatewayPathUnavailable",
        0x0B => "GatewayTargetDeviceFailedToRespond",
        _ => "Unknown",
    };
    format!("Exception({function_name}, code={code} ({exception_name}))")
}

/// Translates CLI arguments into the typed request enum expected by the library.
///
/// Arguments:
/// - `function`: Selected Modbus function.
/// - `address`: Starting register or coil address for the operation.
/// - `values`: Remaining CLI values, interpreted as a quantity or write payload.
///
/// Returns:
/// - `Ok(ModbusRequest)` when the arguments match the selected function.
/// - `Err(String)` when required values are missing or invalid.
fn build_request(
    function: Function,
    address: u16,
    values: &[String],
) -> Result<ModbusRequest, String> {
    match function {
        Function::ReadCoils => {
            let quantity = parse_quantity(values, function)?;
            Ok(ModbusRequest::ReadCoils {
                starting_address: address,
                quantity,
            })
        }
        Function::ReadDiscreteInputs => {
            let quantity = parse_quantity(values, function)?;
            Ok(ModbusRequest::ReadDiscreteInputs {
                starting_address: address,
                quantity,
            })
        }
        Function::ReadHoldingRegisters => {
            let quantity = parse_quantity(values, function)?;
            Ok(ModbusRequest::ReadHoldingRegisters {
                starting_address: address,
                quantity,
            })
        }
        Function::ReadInputRegisters => {
            let quantity = parse_quantity(values, function)?;
            Ok(ModbusRequest::ReadInputRegisters {
                starting_address: address,
                quantity,
            })
        }
        Function::WriteSingleCoil => {
            let value = values
                .first()
                .ok_or("write_coil requires <value> argument (0 or 1)")?;
            let value = parse_coil_value(value)?;
            Ok(ModbusRequest::WriteSingleCoil { address, value })
        }
        Function::WriteSingleRegister => {
            let value = values
                .first()
                .ok_or("write_register requires <value> argument")?;
            let value = parse_register_value(value)?;
            Ok(ModbusRequest::WriteSingleRegister { address, value })
        }
        Function::WriteMultipleCoils => {
            let values = parse_coil_values(values)?;
            if values.is_empty() {
                return Err("write_coils requires at least one value (0 or 1)".to_string());
            }
            Ok(ModbusRequest::WriteMultipleCoils {
                starting_address: address,
                values,
            })
        }
        Function::WriteMultipleRegisters => {
            let values = parse_register_values(values)?;
            if values.is_empty() {
                return Err("write_registers requires at least one value".to_string());
            }
            Ok(ModbusRequest::WriteMultipleRegisters {
                starting_address: address,
                values,
            })
        }
    }
}

/// Formats responses for interactive use and simple shell pipelines.
///
/// Arguments:
/// - `response`: Parsed Modbus response returned by the library.
/// - `output_format`: Output style for register values.
///
/// Returns:
/// - Nothing. Output is written to stdout or stderr.
fn print_response(response: ModbusResponse, output_format: OutputFormat) {
    match response {
        ModbusResponse::ReadCoils { coils } => {
            for coil in coils {
                println!("{}", format_coil(coil));
            }
        }
        ModbusResponse::ReadDiscreteInputs { inputs } => {
            for input in inputs {
                println!("{}", format_coil(input));
            }
        }
        ModbusResponse::ReadHoldingRegisters { registers } => {
            for reg in registers {
                println!("{}", format_register(reg, output_format));
            }
        }
        ModbusResponse::ReadInputRegisters { registers } => {
            for reg in registers {
                println!("{}", format_register(reg, output_format));
            }
        }
        ModbusResponse::WriteSingleCoil { address, value } => {
            println!("address={}, value={}", address, format_coil(value));
        }
        ModbusResponse::WriteSingleRegister { address, value } => {
            println!(
                "address={}, value={}",
                address,
                format_register(value, output_format)
            );
        }
        ModbusResponse::WriteMultipleCoils {
            starting_address,
            quantity,
        } => {
            println!(
                "starting_address={}, quantity={}",
                starting_address, quantity
            );
        }
        ModbusResponse::WriteMultipleRegisters {
            starting_address,
            quantity,
        } => {
            println!(
                "starting_address={}, quantity={}",
                starting_address, quantity
            );
        }
        ModbusResponse::Exception { function, code } => {
            eprintln!("{}", format_exception(function, code));
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::registry()
        .with(EnvFilter::from_env("LOG_LEVEL"))
        .with(fmt::layer().with_target(false))
        .init();

    let cli = Cli::parse();

    let request = build_request(cli.function, cli.start_address, &cli.values)
        .map_err(std::io::Error::other)?;

    info!(
        "Connecting to {}:{} (unit_id={}, transaction_id={})",
        cli.address, cli.port, cli.unit_id, cli.transaction_id
    );

    let connection =
        ModbusTcpConnection::new(cli.address, cli.port, cli.unit_id, cli.transaction_id);

    connection.connect().await?;

    match connection.send_message(&request).await {
        Ok(response) => print_response(response, cli.output),
        Err(ModbusError::ExceptionResponse { function, code }) => {
            eprintln!("{}", format_exception(function, code));
        }
        Err(error) => return Err(error.into()),
    }

    Ok(())
}
