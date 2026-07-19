// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrobus contributors

#![allow(missing_docs, clippy::match_same_arms, clippy::uninlined_format_args)]

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::error::Error;
use std::process;
use tracing::info;
use tracing_subscriber::{filter::EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use ferrobus::ModbusError;
use ferrobus::ModbusRequest;
use ferrobus::ModbusResponse;
use ferrobus::WordOrder;
use ferrobus::tcp::ModbusTcpSocket;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "modbus_cli")]
#[command(about = "Modbus TCP client for the ferrobus library")]
#[command(long_about = "\
Connects to a Modbus TCP device and issues one request at a time.\n\
\n\
Use `read` for non-mutating operations and `write` for state-changing operations.\n\
Results are printed to stdout; errors and exceptions go to stderr.")]
#[command(after_help = "\
Examples:\n  \
modbus_cli read holding --address 192.168.1.10 100 4\n  \
modbus_cli read holding 100 2 --as f32 --word-order little\n  \
modbus_cli read coils 0 16\n  \
modbus_cli write coil 12 on\n  \
modbus_cli write register --output hex 200 4660\n  \
modbus_cli write coils 16 1 0 1 1\n  \
modbus_cli write registers 300 10 20 30\n  \
modbus_cli -a 192.168.0.89 write register 5004 6000\n\
\n\
Environment:\n  \
LOG_LEVEL   Set tracing verbosity (e.g. LOG_LEVEL=debug)")]
struct Cli {
    #[arg(
        short = 'a',
        long = "address",
        global = true,
        default_value = "127.0.0.1",
        help = "Target device host name or IP address"
    )]
    host: String,

    #[arg(
        short = 'p',
        long,
        global = true,
        default_value = "502",
        help = "Modbus TCP port"
    )]
    port: u16,

    #[arg(
        short = 'u',
        long,
        global = true,
        default_value = "1",
        help = "Modbus unit identifier (1-247)"
    )]
    unit_id: u8,

    #[arg(
        short = 't',
        long,
        global = true,
        default_value = "1",
        help = "Initial transaction identifier"
    )]
    transaction_id: u16,

    #[arg(
        short = 'o',
        long,
        global = true,
        default_value = "decimal",
        help = "Output format for register values [decimal, hex]"
    )]
    output: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Read coils, discrete inputs, or registers (non-mutating)
    #[command(
        about = "Read coils, discrete inputs, or registers from the device",
        long_about = "Issues a non-mutating Modbus read request. The device state is not changed."
    )]
    Read {
        #[command(subcommand)]
        operation: ReadOperation,
    },

    /// Write coils or registers (mutating)
    #[command(
        about = "Write coils or registers to the device",
        long_about = "Issues a mutating Modbus write request. The target device state will change."
    )]
    Write {
        #[command(subcommand)]
        operation: WriteOperation,
    },
}

#[derive(Subcommand, Debug)]
enum ReadOperation {
    /// Read one or more coils (function code 0x01)
    #[command(
        about = "Read coils starting at <ADDRESS> (FC 0x01)",
        after_help = "Example: modbus_cli read coils 0 16"
    )]
    Coils(ReadArgs),

    /// Read one or more discrete inputs (function code 0x02)
    #[command(
        about = "Read discrete inputs starting at <ADDRESS> (FC 0x02)",
        after_help = "Example: modbus_cli read discrete 100 8"
    )]
    Discrete(ReadArgs),

    /// Read one or more holding registers (function code 0x03)
    #[command(
        about = "Read holding registers starting at <ADDRESS> (FC 0x03)",
        long_about = "Read holding registers. Values are returned as u16 by default. \
                      Use --value-type/--as to decode read results as wide values; \
                      --word-order changes only multi-register word order, not wire byte order. \
                      Wide-value writes are intentionally not supported by this example.",
        after_help = "Example: modbus_cli read holding 40001 2 --as f32 --word-order little"
    )]
    Holding(RegisterReadArgs),

    /// Read one or more input registers (function code 0x04)
    #[command(
        about = "Read input registers starting at <ADDRESS> (FC 0x04)",
        long_about = "Read input registers. Values are returned as u16 by default. \
                      Use --value-type/--as to decode read results as wide values; \
                      --word-order changes only multi-register word order, not wire byte order. \
                      Wide-value writes are intentionally not supported by this example.",
        after_help = "Example: modbus_cli read input 30001 4 --value-type f64"
    )]
    Input(RegisterReadArgs),
}

#[derive(Args, Debug)]
struct ReadArgs {
    /// Starting coil/input address (0-65535)
    #[arg(value_name = "ADDRESS")]
    address: u16,

    /// Number of items to read (1-2000)
    #[arg(value_name = "QUANTITY")]
    quantity: u16,
}

#[derive(Args, Debug)]
struct RegisterReadArgs {
    /// Starting register address (0-65535)
    #[arg(value_name = "ADDRESS")]
    address: u16,

    /// Raw register count to read (1-125). For wide value types this must be a multiple of the register width.
    #[arg(value_name = "QUANTITY")]
    quantity: u16,

    /// Interpret read registers as this scalar type; wide-value writes are out of scope.
    #[arg(long = "value-type", visible_alias = "as", default_value = "u16")]
    value_type: ValueType,

    /// Word order for multi-register values only; ignored for u16. Registers remain big-endian on the wire.
    #[arg(long = "word-order", default_value = "big")]
    word_order: CliWordOrder,
}

#[derive(Subcommand, Debug)]
enum WriteOperation {
    /// Write a single coil (function code 0x05)
    #[command(
        about = "Write a single coil at <ADDRESS> (FC 0x05)",
        long_about = "Accepted coil values: 1, true, on (energize) or 0, false, off (de-energize).\n\
                       The wire encoding uses 0xFF00 for ON and 0x0000 for OFF.",
        after_help = "Example: modbus_cli write coil 12 on"
    )]
    Coil(WriteSingleCoilArgs),

    /// Write a single holding register (function code 0x06)
    #[command(
        about = "Write a single register at <ADDRESS> (FC 0x06)",
        long_about = "The value is an unsigned 16-bit integer (0-65535).\n\
                       It is sent big-endian on the wire.",
        after_help = "Example: modbus_cli write register 200 4660"
    )]
    Register(WriteSingleRegisterArgs),

    /// Write multiple coils (function code 0x0F)
    #[command(
        about = "Write multiple coils starting at <ADDRESS> (FC 0x0F)",
        long_about = "Each value is a coil state: 1/true/on or 0/false/off.\n\
                       Values are applied starting at <ADDRESS> in order.",
        after_help = "Example: modbus_cli write coils 16 1 0 1 1"
    )]
    Coils(WriteMultipleCoilsArgs),

    /// Write multiple holding registers (function code 0x10)
    #[command(
        about = "Write multiple registers starting at <ADDRESS> (FC 0x10)",
        long_about = "Each value is an unsigned 16-bit integer (0-65535).\n\
                       Values are written starting at <ADDRESS> in order.",
        after_help = "Example: modbus_cli write registers 300 10 20 30"
    )]
    Registers(WriteMultipleRegistersArgs),
}

#[derive(Args, Debug)]
struct WriteSingleCoilArgs {
    /// Coil address (0-65535)
    #[arg(value_name = "ADDRESS")]
    address: u16,

    /// Coil state: 1, true, on (energize) or 0, false, off (de-energize)
    #[arg(value_name = "VALUE")]
    value: String,
}

#[derive(Args, Debug)]
struct WriteSingleRegisterArgs {
    /// Register address (0-65535)
    #[arg(value_name = "ADDRESS")]
    address: u16,

    /// Unsigned 16-bit value (0-65535)
    #[arg(value_name = "VALUE")]
    value: String,
}

#[derive(Args, Debug)]
struct WriteMultipleCoilsArgs {
    /// Starting coil address (0-65535)
    #[arg(value_name = "ADDRESS")]
    address: u16,

    /// One or more coil values: 1/true/on or 0/false/off
    #[arg(value_name = "VALUES", required = true, num_args = 1..)]
    values: Vec<String>,
}

#[derive(Args, Debug)]
struct WriteMultipleRegistersArgs {
    /// Starting register address (0-65535)
    #[arg(value_name = "ADDRESS")]
    address: u16,

    /// One or more unsigned 16-bit values (0-65535)
    #[arg(value_name = "VALUES", required = true, num_args = 1..)]
    values: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum OutputFormat {
    #[default]
    #[clap(name = "decimal")]
    Decimal,
    #[clap(name = "hex")]
    Hex,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum ValueType {
    #[default]
    #[clap(name = "u16")]
    U16,
    #[clap(name = "u32")]
    U32,
    #[clap(name = "i32")]
    I32,
    #[clap(name = "f32")]
    F32,
    #[clap(name = "u64")]
    U64,
    #[clap(name = "i64")]
    I64,
    #[clap(name = "f64")]
    F64,
}

impl ValueType {
    fn register_width(self) -> u16 {
        match self {
            ValueType::U16 => 1,
            ValueType::U32 | ValueType::I32 | ValueType::F32 => 2,
            ValueType::U64 | ValueType::I64 | ValueType::F64 => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ValueType::U16 => "u16",
            ValueType::U32 => "u32",
            ValueType::I32 => "i32",
            ValueType::F32 => "f32",
            ValueType::U64 => "u64",
            ValueType::I64 => "i64",
            ValueType::F64 => "f64",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum CliWordOrder {
    #[default]
    #[clap(name = "big")]
    Big,
    #[clap(name = "little")]
    Little,
}

impl From<CliWordOrder> for WordOrder {
    fn from(value: CliWordOrder) -> Self {
        match value {
            CliWordOrder::Big => WordOrder::BigEndian,
            CliWordOrder::Little => WordOrder::LittleEndian,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayPlan {
    Default,
    RegisterRead {
        value_type: ValueType,
        word_order: WordOrder,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedRequest {
    request: ModbusRequest,
    display_plan: DisplayPlan,
}

// ---------------------------------------------------------------------------
// Display name helpers
// ---------------------------------------------------------------------------

/// Human-readable name for a Modbus function, used in echo and exception output.
fn function_display_name(code: u8) -> &'static str {
    match code {
        0x01 => "ReadCoils",
        0x02 => "ReadDiscreteInputs",
        0x03 => "ReadHoldingRegisters",
        0x04 => "ReadInputRegisters",
        0x05 => "WriteSingleCoil",
        0x06 => "WriteSingleRegister",
        0x0F => "WriteMultipleCoils",
        0x10 => "WriteMultipleRegisters",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Accepts common operator-friendly coil spellings for write commands.
fn parse_coil_value(s: &str) -> Result<bool, String> {
    match s {
        "1" | "true" | "on" => Ok(true),
        "0" | "false" | "off" => Ok(false),
        _ => Err(format!(
            "invalid coil value: '{s}' — expected 1/true/on or 0/false/off"
        )),
    }
}

fn parse_coil_values(values: &[String]) -> Result<Vec<bool>, String> {
    values.iter().map(|s| parse_coil_value(s)).collect()
}

/// Validates register values before they are sent in write requests.
fn parse_register_value(s: &str) -> Result<u16, String> {
    s.parse::<u16>()
        .map_err(|_| format!("invalid register value: '{s}' — expected unsigned integer 0-65535"))
}

fn parse_register_values(values: &[String]) -> Result<Vec<u16>, String> {
    values.iter().map(|s| parse_register_value(s)).collect()
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_register(value: u16, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{value:04X}"),
    }
}

fn format_u32(value: u32, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{value:08X}"),
    }
}

fn format_i32(value: i32, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{:08X}", u32::from_ne_bytes(value.to_ne_bytes())),
    }
}

fn format_f32(value: f32, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{:08X}", value.to_bits()),
    }
}

fn format_u64(value: u64, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{value:016X}"),
    }
}

fn format_i64(value: i64, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{:016X}", u64::from_ne_bytes(value.to_ne_bytes())),
    }
}

fn format_f64(value: f64, format: OutputFormat) -> String {
    match format {
        OutputFormat::Decimal => value.to_string(),
        OutputFormat::Hex => format!("0x{:016X}", value.to_bits()),
    }
}

fn formatted_register_values(
    registers: &[u16],
    display_plan: DisplayPlan,
    output_format: OutputFormat,
) -> Result<Vec<String>, String> {
    match display_plan {
        DisplayPlan::Default
        | DisplayPlan::RegisterRead {
            value_type: ValueType::U16,
            ..
        } => Ok(registers
            .iter()
            .map(|reg| format_register(*reg, output_format))
            .collect()),
        DisplayPlan::RegisterRead {
            value_type,
            word_order,
        } => match value_type {
            ValueType::U16 => Ok(registers
                .iter()
                .map(|reg| format_register(*reg, output_format))
                .collect()),
            ValueType::U32 => word_order
                .decode_u32_block(registers)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| format_u32(value, output_format))
                        .collect()
                })
                .map_err(|error| error.to_string()),
            ValueType::I32 => word_order
                .decode_i32_block(registers)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| format_i32(value, output_format))
                        .collect()
                })
                .map_err(|error| error.to_string()),
            ValueType::F32 => word_order
                .decode_f32_block(registers)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| format_f32(value, output_format))
                        .collect()
                })
                .map_err(|error| error.to_string()),
            ValueType::U64 => word_order
                .decode_u64_block(registers)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| format_u64(value, output_format))
                        .collect()
                })
                .map_err(|error| error.to_string()),
            ValueType::I64 => word_order
                .decode_i64_block(registers)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| format_i64(value, output_format))
                        .collect()
                })
                .map_err(|error| error.to_string()),
            ValueType::F64 => word_order
                .decode_f64_block(registers)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| format_f64(value, output_format))
                        .collect()
                })
                .map_err(|error| error.to_string()),
        },
    }
}

fn format_coil(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

/// Formats exception responses for operators diagnosing bad addresses or unsupported functions.
fn format_exception(function: u8, code: u8) -> String {
    let func = function_display_name(function & 0x7F);
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
    format!("Exception: {func}, code={code} ({exception_name})")
}

// ---------------------------------------------------------------------------
// Request description — echoes what was requested
// ---------------------------------------------------------------------------

/// Returns a concise, human-readable description of the request that was issued.
fn describe_request(request: &ModbusRequest) -> String {
    match request {
        ModbusRequest::ReadCoils {
            starting_address,
            quantity,
        } => format!("ReadCoils address={starting_address} quantity={quantity}"),
        ModbusRequest::ReadDiscreteInputs {
            starting_address,
            quantity,
        } => format!("ReadDiscreteInputs address={starting_address} quantity={quantity}"),
        ModbusRequest::ReadHoldingRegisters {
            starting_address,
            quantity,
        } => format!("ReadHoldingRegisters address={starting_address} quantity={quantity}"),
        ModbusRequest::ReadInputRegisters {
            starting_address,
            quantity,
        } => format!("ReadInputRegisters address={starting_address} quantity={quantity}"),
        ModbusRequest::WriteSingleCoil { address, value } => {
            format!(
                "WriteSingleCoil address={address} value={}",
                format_coil(*value)
            )
        }
        ModbusRequest::WriteSingleRegister { address, value } => {
            format!("WriteSingleRegister address={address} value={value}")
        }
        ModbusRequest::WriteMultipleCoils {
            starting_address,
            values,
        } => format!(
            "WriteMultipleCoils address={starting_address} quantity={}",
            values.len()
        ),
        ModbusRequest::WriteMultipleRegisters {
            starting_address,
            values,
        } => format!(
            "WriteMultipleRegisters address={starting_address} quantity={}",
            values.len()
        ),
    }
}

// ---------------------------------------------------------------------------
// Request builder
// ---------------------------------------------------------------------------

/// Translates CLI subcommands into the typed request enum expected by the library.
fn build_request(command: &Command) -> Result<PlannedRequest, String> {
    match command {
        Command::Read { operation } => match operation {
            ReadOperation::Coils(args) => Ok(PlannedRequest {
                request: ModbusRequest::ReadCoils {
                    starting_address: args.address,
                    quantity: args.quantity,
                },
                display_plan: DisplayPlan::Default,
            }),
            ReadOperation::Discrete(args) => Ok(PlannedRequest {
                request: ModbusRequest::ReadDiscreteInputs {
                    starting_address: args.address,
                    quantity: args.quantity,
                },
                display_plan: DisplayPlan::Default,
            }),
            ReadOperation::Holding(args) => build_register_read_request(true, args),
            ReadOperation::Input(args) => build_register_read_request(false, args),
        },
        Command::Write { operation } => match operation {
            WriteOperation::Coil(args) => {
                let value = parse_coil_value(&args.value)?;
                Ok(PlannedRequest {
                    request: ModbusRequest::WriteSingleCoil {
                        address: args.address,
                        value,
                    },
                    display_plan: DisplayPlan::Default,
                })
            }
            WriteOperation::Register(args) => {
                let value = parse_register_value(&args.value)?;
                Ok(PlannedRequest {
                    request: ModbusRequest::WriteSingleRegister {
                        address: args.address,
                        value,
                    },
                    display_plan: DisplayPlan::Default,
                })
            }
            WriteOperation::Coils(args) => {
                let values = parse_coil_values(&args.values)?;
                Ok(PlannedRequest {
                    request: ModbusRequest::WriteMultipleCoils {
                        starting_address: args.address,
                        values,
                    },
                    display_plan: DisplayPlan::Default,
                })
            }
            WriteOperation::Registers(args) => {
                let values = parse_register_values(&args.values)?;
                Ok(PlannedRequest {
                    request: ModbusRequest::WriteMultipleRegisters {
                        starting_address: args.address,
                        values,
                    },
                    display_plan: DisplayPlan::Default,
                })
            }
        },
    }
}

fn build_register_read_request(
    holding: bool,
    args: &RegisterReadArgs,
) -> Result<PlannedRequest, String> {
    let width = args.value_type.register_width();
    if args.quantity % width != 0 {
        return Err(format!(
            "quantity {} is not a multiple of {} registers for {}",
            args.quantity,
            width,
            args.value_type.label()
        ));
    }

    let request = if holding {
        ModbusRequest::ReadHoldingRegisters {
            starting_address: args.address,
            quantity: args.quantity,
        }
    } else {
        ModbusRequest::ReadInputRegisters {
            starting_address: args.address,
            quantity: args.quantity,
        }
    };

    Ok(PlannedRequest {
        request,
        display_plan: DisplayPlan::RegisterRead {
            value_type: args.value_type,
            word_order: WordOrder::from(args.word_order),
        },
    })
}

// ---------------------------------------------------------------------------
// Response printing
// ---------------------------------------------------------------------------

/// Formats responses for interactive use and simple shell pipelines.
fn print_response(
    request: &ModbusRequest,
    response: ModbusResponse,
    output_format: OutputFormat,
    display_plan: DisplayPlan,
) -> Result<(), String> {
    // Echo the requested action so the operator can confirm what was issued.
    eprintln!("Request: {}", describe_request(request));

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
        ModbusResponse::ReadHoldingRegisters { registers }
        | ModbusResponse::ReadInputRegisters { registers } => {
            for value in formatted_register_values(&registers, display_plan, output_format)? {
                println!("{value}");
            }
        }
        ModbusResponse::WriteSingleCoil { address, value } => {
            println!("OK: address={}, value={}", address, format_coil(value));
        }
        ModbusResponse::WriteSingleRegister { address, value } => {
            println!(
                "OK: address={}, value={}",
                address,
                format_register(value, output_format)
            );
        }
        ModbusResponse::WriteMultipleCoils {
            starting_address,
            quantity,
        } => {
            println!(
                "OK: starting_address={}, quantity={}",
                starting_address, quantity
            );
        }
        ModbusResponse::WriteMultipleRegisters {
            starting_address,
            quantity,
        } => {
            println!(
                "OK: starting_address={}, quantity={}",
                starting_address, quantity
            );
        }
        ModbusResponse::Exception { function, code } => {
            eprintln!("{}", format_exception(function, code));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Error formatting — phase-aware timeout hints
// ---------------------------------------------------------------------------

/// Formats a `ModbusError` with phase-specific guidance so the operator
/// knows which stage failed and what to try next.
fn format_error(error: &ModbusError, host: &str, port: u16) -> String {
    match error {
        ModbusError::ConnectError(e) => {
            format!(
                "Connect failed ({host}:{port}): {e}\n\
                 Hint: verify the device is reachable and the port is correct."
            )
        }
        ModbusError::ConnectTimeout => {
            format!(
                "Connect timed out ({host}:{port}).\n\
                 Hint: check network connectivity and firewall rules."
            )
        }
        ModbusError::WriteError(e) => {
            format!(
                "Write failed ({host}:{port}): {e}\n\
                 Hint: the TCP session may have been closed by the device. Retry the request."
            )
        }
        ModbusError::WriteTimeout => {
            format!(
                "Write timed out ({host}:{port}).\n\
                 Hint: the device may be unresponsive. Check the connection and retry."
            )
        }
        ModbusError::ReadError(e) => {
            format!(
                "Read failed ({host}:{port}): {e}\n\
                 Hint: the device may have dropped the connection after the request was sent."
            )
        }
        ModbusError::ReadTimeout => {
            format!(
                "Read timed out ({host}:{port}).\n\
                 Hint: the device accepted the connection but did not respond in time. \
                 Verify the unit ID and function are supported."
            )
        }
        ModbusError::ExceptionResponse { function, code } => format_exception(*function, *code),
        other => format!("Error: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::registry()
        .with(EnvFilter::from_env("LOG_LEVEL"))
        .with(fmt::layer().with_target(false))
        .init();

    let cli = Cli::parse();

    let planned = match build_request(&cli.command) {
        Ok(r) => r,
        Err(message) => {
            eprintln!("Error: {message}");
            process::exit(2);
        }
    };
    let request = planned.request;
    let display_plan = planned.display_plan;

    info!(
        "Connecting to {}:{} (unit_id={}, transaction_id={})",
        cli.host, cli.port, cli.unit_id, cli.transaction_id
    );
    info!("{}", describe_request(&request));

    let connection = ModbusTcpSocket::new(cli.host.clone(), cli.port, cli.unit_id)
        .with_initial_transaction_id(cli.transaction_id)
        .connect()
        .await
        .map_err(|e| format_error(&e, &cli.host, cli.port))?;

    match connection.send_message(&request).await {
        Ok(response) => {
            if let Err(message) = print_response(&request, response, cli.output, display_plan) {
                eprintln!("Error: {message}");
                process::exit(1);
            }
        }
        Err(ModbusError::ExceptionResponse { function, code }) => {
            eprintln!("Request: {}", describe_request(&request));
            eprintln!("{}", format_exception(function, code));
            process::exit(1);
        }
        Err(error) => {
            eprintln!("Request: {}", describe_request(&request));
            eprintln!("{}", format_error(&error, &cli.host, cli.port));
            process::exit(1);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Coil parsing -------------------------------------------------------

    #[test]
    fn parse_coil_accepts_on_spellings() {
        assert!(parse_coil_value("1").unwrap());
        assert!(parse_coil_value("true").unwrap());
        assert!(parse_coil_value("on").unwrap());
    }

    #[test]
    fn parse_coil_accepts_off_spellings() {
        assert!(!parse_coil_value("0").unwrap());
        assert!(!parse_coil_value("false").unwrap());
        assert!(!parse_coil_value("off").unwrap());
    }

    #[test]
    fn parse_coil_rejects_invalid() {
        assert!(parse_coil_value("yes").is_err());
        assert!(parse_coil_value("2").is_err());
        assert!(parse_coil_value("").is_err());
    }

    // -- Register parsing ---------------------------------------------------

    #[test]
    fn parse_register_valid() {
        assert_eq!(parse_register_value("0").unwrap(), 0);
        assert_eq!(parse_register_value("65535").unwrap(), 65535);
        assert_eq!(parse_register_value("4660").unwrap(), 4660);
    }

    #[test]
    fn parse_register_rejects_overflow() {
        assert!(parse_register_value("65536").is_err());
        assert!(parse_register_value("-1").is_err());
        assert!(parse_register_value("abc").is_err());
    }

    // -- Request building ---------------------------------------------------

    #[test]
    fn build_read_coils_request() {
        let cmd = Command::Read {
            operation: ReadOperation::Coils(ReadArgs {
                address: 100,
                quantity: 8,
            }),
        };
        let request = build_request(&cmd).unwrap().request;
        assert_eq!(
            request,
            ModbusRequest::ReadCoils {
                starting_address: 100,
                quantity: 8
            }
        );
    }

    #[test]
    fn build_read_discrete_request() {
        let cmd = Command::Read {
            operation: ReadOperation::Discrete(ReadArgs {
                address: 0,
                quantity: 16,
            }),
        };
        let request = build_request(&cmd).unwrap().request;
        assert_eq!(
            request,
            ModbusRequest::ReadDiscreteInputs {
                starting_address: 0,
                quantity: 16
            }
        );
    }

    #[test]
    fn build_read_holding_request() {
        let cmd = Command::Read {
            operation: ReadOperation::Holding(RegisterReadArgs {
                address: 40001,
                quantity: 10,
                value_type: ValueType::U16,
                word_order: CliWordOrder::Big,
            }),
        };
        let planned = build_request(&cmd).unwrap();
        assert_eq!(
            planned.request,
            ModbusRequest::ReadHoldingRegisters {
                starting_address: 40001,
                quantity: 10
            }
        );
        assert_eq!(
            planned.display_plan,
            DisplayPlan::RegisterRead {
                value_type: ValueType::U16,
                word_order: WordOrder::BigEndian,
            }
        );
    }

    #[test]
    fn build_read_input_request() {
        let cmd = Command::Read {
            operation: ReadOperation::Input(RegisterReadArgs {
                address: 30001,
                quantity: 5,
                value_type: ValueType::U16,
                word_order: CliWordOrder::Big,
            }),
        };
        let request = build_request(&cmd).unwrap().request;
        assert_eq!(
            request,
            ModbusRequest::ReadInputRegisters {
                starting_address: 30001,
                quantity: 5
            }
        );
    }

    #[test]
    fn build_write_single_coil_request() {
        let cmd = Command::Write {
            operation: WriteOperation::Coil(WriteSingleCoilArgs {
                address: 12,
                value: "on".to_string(),
            }),
        };
        let request = build_request(&cmd).unwrap().request;
        assert_eq!(
            request,
            ModbusRequest::WriteSingleCoil {
                address: 12,
                value: true
            }
        );
    }

    #[test]
    fn build_write_single_register_request() {
        let cmd = Command::Write {
            operation: WriteOperation::Register(WriteSingleRegisterArgs {
                address: 200,
                value: "4660".to_string(),
            }),
        };
        let request = build_request(&cmd).unwrap().request;
        assert_eq!(
            request,
            ModbusRequest::WriteSingleRegister {
                address: 200,
                value: 4660
            }
        );
    }

    #[test]
    fn build_write_multiple_coils_request() {
        let cmd = Command::Write {
            operation: WriteOperation::Coils(WriteMultipleCoilsArgs {
                address: 16,
                values: vec![
                    "1".to_string(),
                    "0".to_string(),
                    "true".to_string(),
                    "off".to_string(),
                ],
            }),
        };
        let request = build_request(&cmd).unwrap().request;
        assert_eq!(
            request,
            ModbusRequest::WriteMultipleCoils {
                starting_address: 16,
                values: vec![true, false, true, false]
            }
        );
    }

    #[test]
    fn build_write_multiple_registers_request() {
        let cmd = Command::Write {
            operation: WriteOperation::Registers(WriteMultipleRegistersArgs {
                address: 300,
                values: vec!["10".to_string(), "20".to_string(), "30".to_string()],
            }),
        };
        let request = build_request(&cmd).unwrap().request;
        assert_eq!(
            request,
            ModbusRequest::WriteMultipleRegisters {
                starting_address: 300,
                values: vec![10, 20, 30]
            }
        );
    }

    #[test]
    fn build_register_read_rejects_non_multiple_quantity() {
        let cmd = Command::Read {
            operation: ReadOperation::Holding(RegisterReadArgs {
                address: 100,
                quantity: 3,
                value_type: ValueType::F32,
                word_order: CliWordOrder::Little,
            }),
        };

        let result = build_request(&cmd);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .is_some_and(|message| message.contains("multiple of 2"))
        );
    }

    #[test]
    fn build_register_read_accepts_wide_value_plan() {
        let cmd = Command::Read {
            operation: ReadOperation::Input(RegisterReadArgs {
                address: 100,
                quantity: 4,
                value_type: ValueType::F64,
                word_order: CliWordOrder::Little,
            }),
        };

        let planned = build_request(&cmd).unwrap();
        assert_eq!(
            planned.display_plan,
            DisplayPlan::RegisterRead {
                value_type: ValueType::F64,
                word_order: WordOrder::LittleEndian,
            }
        );
    }

    #[test]
    fn build_write_single_coil_rejects_bad_value() {
        let cmd = Command::Write {
            operation: WriteOperation::Coil(WriteSingleCoilArgs {
                address: 0,
                value: "yes".to_string(),
            }),
        };
        assert!(build_request(&cmd).is_err());
    }

    #[test]
    fn build_write_single_register_rejects_overflow() {
        let cmd = Command::Write {
            operation: WriteOperation::Register(WriteSingleRegisterArgs {
                address: 0,
                value: "99999".to_string(),
            }),
        };
        assert!(build_request(&cmd).is_err());
    }

    // -- Describe request ---------------------------------------------------

    #[test]
    fn describe_request_read_coils() {
        let request = ModbusRequest::ReadCoils {
            starting_address: 100,
            quantity: 8,
        };
        let desc = describe_request(&request);
        assert!(desc.contains("ReadCoils"));
        assert!(desc.contains("100"));
        assert!(desc.contains("8"));
    }

    #[test]
    fn describe_request_write_single_coil() {
        let request = ModbusRequest::WriteSingleCoil {
            address: 12,
            value: true,
        };
        let desc = describe_request(&request);
        assert!(desc.contains("WriteSingleCoil"));
        assert!(desc.contains("12"));
        assert!(desc.contains("1"));
    }

    #[test]
    fn describe_request_write_multiple_registers() {
        let request = ModbusRequest::WriteMultipleRegisters {
            starting_address: 300,
            values: vec![10, 20, 30],
        };
        let desc = describe_request(&request);
        assert!(desc.contains("WriteMultipleRegisters"));
        assert!(desc.contains("300"));
        assert!(desc.contains("3"));
    }

    // -- Exception formatting -----------------------------------------------

    #[test]
    fn format_exception_known_codes() {
        let msg = format_exception(0x81, 0x02);
        assert!(msg.contains("ReadCoils"));
        assert!(msg.contains("IllegalDataAddress"));
    }

    #[test]
    fn format_exception_unknown_function() {
        let msg = format_exception(0xF0, 0x01);
        assert!(msg.contains("Unknown"));
        assert!(msg.contains("IllegalFunction"));
    }

    // -- Error formatting ---------------------------------------------------

    #[test]
    fn format_error_connect_timeout_includes_hint() {
        let msg = format_error(&ModbusError::ConnectTimeout, "192.168.1.10", 502);
        assert!(msg.contains("Connect timed out"));
        assert!(msg.contains("192.168.1.10:502"));
        assert!(msg.contains("Hint"));
    }

    #[test]
    fn format_error_read_timeout_includes_hint() {
        let msg = format_error(&ModbusError::ReadTimeout, "10.0.0.1", 502);
        assert!(msg.contains("Read timed out"));
        assert!(msg.contains("10.0.0.1:502"));
        assert!(msg.contains("unit ID"));
    }

    #[test]
    fn format_error_write_timeout_includes_hint() {
        let msg = format_error(&ModbusError::WriteTimeout, "10.0.0.1", 502);
        assert!(msg.contains("Write timed out"));
        assert!(msg.contains("Hint"));
    }

    // -- Output formatting --------------------------------------------------

    #[test]
    fn format_register_decimal() {
        assert_eq!(format_register(4660, OutputFormat::Decimal), "4660");
    }

    #[test]
    fn format_register_hex() {
        assert_eq!(format_register(4660, OutputFormat::Hex), "0x1234");
    }

    #[test]
    fn format_wide_values_decimal_and_hex() {
        assert_eq!(format_i32(-1, OutputFormat::Hex), "0xFFFFFFFF");
        assert_eq!(format_u32(4660, OutputFormat::Hex), "0x00001234");
        assert_eq!(format_f32(1.0, OutputFormat::Hex), "0x3F800000");
        assert_eq!(format_f64(1.0, OutputFormat::Hex), "0x3FF0000000000000");
        assert_eq!(format_i32(-1, OutputFormat::Decimal), "-1");
    }

    #[test]
    fn formatted_register_values_honors_little_endian_word_order() {
        let values = formatted_register_values(
            &[0x0000, 0x3F80],
            DisplayPlan::RegisterRead {
                value_type: ValueType::F32,
                word_order: WordOrder::LittleEndian,
            },
            OutputFormat::Hex,
        );

        assert_eq!(values, Ok(vec!["0x3F800000".to_string()]));
    }

    #[test]
    fn word_order_is_accepted_but_ignored_for_u16() {
        let values = formatted_register_values(
            &[0x1234, 0x5678],
            DisplayPlan::RegisterRead {
                value_type: ValueType::U16,
                word_order: WordOrder::LittleEndian,
            },
            OutputFormat::Hex,
        );

        assert_eq!(values, Ok(vec!["0x1234".to_string(), "0x5678".to_string()]));
    }

    #[test]
    fn format_coil_values() {
        assert_eq!(format_coil(true), "1");
        assert_eq!(format_coil(false), "0");
    }

    // -- Function display name ----------------------------------------------

    #[test]
    fn function_display_name_known() {
        assert_eq!(function_display_name(0x01), "ReadCoils");
        assert_eq!(function_display_name(0x06), "WriteSingleRegister");
        assert_eq!(function_display_name(0x10), "WriteMultipleRegisters");
    }

    #[test]
    fn function_display_name_unknown() {
        assert_eq!(function_display_name(0xFF), "Unknown");
    }

    #[test]
    fn parse_cli_global_address_does_not_conflict_with_register_address() {
        let cli = Cli::parse_from([
            "modbus_cli",
            "-a",
            "192.168.0.89",
            "write",
            "register",
            "5004",
            "16000",
        ]);

        assert_eq!(cli.host, "192.168.0.89");
        match cli.command {
            Command::Write { operation } => match operation {
                WriteOperation::Register(args) => {
                    assert_eq!(args.address, 5004);
                    assert_eq!(args.value, "16000");
                }
                other => panic!("expected write register command, got {other:?}"),
            },
            other => panic!("expected write command, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_accepts_as_alias_and_word_order_on_holding() {
        let cli = Cli::parse_from([
            "modbus_cli",
            "read",
            "holding",
            "100",
            "2",
            "--as",
            "f32",
            "--word-order",
            "little",
        ]);

        match cli.command {
            Command::Read { operation } => match operation {
                ReadOperation::Holding(args) => {
                    assert_eq!(args.address, 100);
                    assert_eq!(args.quantity, 2);
                    assert_eq!(args.value_type, ValueType::F32);
                    assert_eq!(args.word_order, CliWordOrder::Little);
                }
                other => panic!("expected read holding command, got {other:?}"),
            },
            other => panic!("expected read command, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_accepts_value_type_on_input() {
        let cli = Cli::parse_from([
            "modbus_cli",
            "read",
            "input",
            "30001",
            "4",
            "--value-type",
            "u64",
        ]);

        match cli.command {
            Command::Read { operation } => match operation {
                ReadOperation::Input(args) => assert_eq!(args.value_type, ValueType::U64),
                other => panic!("expected read input command, got {other:?}"),
            },
            other => panic!("expected read command, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_rejects_wide_value_flags_on_coils_and_discrete() {
        assert!(
            Cli::try_parse_from([
                "modbus_cli",
                "read",
                "coils",
                "0",
                "8",
                "--value-type",
                "f32"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "modbus_cli",
                "read",
                "discrete",
                "0",
                "8",
                "--word-order",
                "little",
            ])
            .is_err()
        );
    }
}
