# Sprint: Little-Endian (Word-Order) Support for Wide Values

## Goal

Add support for **little-endian word order** when decoding multi-register
(32/64-bit) application values, while keeping the
Modbus-standard big-endian wire encoding as the default. This lets users
interoperate with devices and gateways that pack 32/64-bit values across
consecutive registers least-significant-word first.

## Background

Modbus specifies **big-endian** for every 16-bit field on the wire (addresses,
quantities, and register values). ferrobus currently hard-codes this:

- `src/request.rs` serializes with `u16::to_be_bytes()`.
- `src/response.rs` parses with `u16::from_be_bytes(...)`.
- `examples/modbus_cli.rs` documents register values as "sent big-endian".

The gap: 32-bit and 64-bit application values are packed into 2 or 4
consecutive registers, and vendors disagree on whether the most-significant
word comes first (big-endian word order) or last (little-endian word order).
ferrobus has no helper to decode these wide values, and no way to pick word
order.

The MBAP header and protocol-level fields (transaction ID, protocol ID, length,
addresses, quantities) and the individual register byte order MUST remain
big-endian regardless of configuration — only the **word order** of
multi-register scalars is configurable.

## Scope

### In scope

- A `WordOrder` configuration (big-endian default, plus little-endian).
- Typed decode helpers for 32-bit and 64-bit values (`u32`, `i32`, `f32`,
  `u64`, `i64`, `f64`) from register vectors honoring word order.
  (Wiring wide values into writes is out of scope — only reads decode wide values.)
- Keeping all protocol/MBAP fields and per-register byte order big-endian.
- Documentation and a CLI flag.

### Out of scope

- **Per-register byte-order swapping** (byte-swap *within* a single 16-bit
  register). Individual registers stay big-endian on the wire. This is rare and
  can be added later if a concrete device needs it.
- Changing the default wire behavior (stays spec-compliant big-endian).
- RTU/ASCII transports (still TCP only).
- Automatic device auto-detection of word order.

## Design sketch

```rust
/// Word order for scalar values spanning multiple 16-bit registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordOrder {
    /// Most-significant word first (Modbus default).
    #[default]
    BigEndian,
    /// Least-significant word first.
    LittleEndian,
}
```

Decode helpers operate on the `&[u16]` produced by the existing
`ReadHoldingRegisters` / `ReadInputRegisters` responses, so the wire codec is
untouched. Each register is still interpreted big-endian; only the order in
which words are combined follows `WordOrder`.

Word-combination examples (registers are already host `u16`s):

- 32-bit, big-endian word order: `[0x1234, 0x5678] -> 0x1234_5678`
- 32-bit, little-endian word order: `[0x1234, 0x5678] -> 0x5678_1234`
- 64-bit little-endian **reverses all four 16-bit words** (never bytes within a
  word): `[0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD] -> 0xDDDD_CCCC_BBBB_AAAA`
  (big-endian is `0xAAAA_BBBB_CCCC_DDDD`).

Signatures:

```rust
impl WordOrder {
    pub fn decode_u32(self, regs: [u16; 2]) -> u32;
    pub fn decode_i32(self, regs: [u16; 2]) -> i32;
    pub fn decode_f32(self, regs: [u16; 2]) -> f32;
    pub fn decode_u64(self, regs: [u16; 4]) -> u64;
    pub fn decode_i64(self, regs: [u16; 4]) -> i64;
    pub fn decode_f64(self, regs: [u16; 4]) -> f64;
}
```

Keeping this as a decode layer over register slices (rather than threading a
word-order flag through the wire codec) is the lowest-risk
approach: the MBAP codec and PDU serialization stay spec-compliant and
untouched.

### Block decode API shape

Block helpers are exposed as inherent methods on `WordOrder` that take register
slices and return decoded value vectors. They validate that the input length is
an exact multiple of the scalar width:

```rust
impl WordOrder {
    pub fn decode_u32_block(self, regs: &[u16]) -> Result<Vec<u32>, WordOrderError>;
    pub fn decode_f32_block(self, regs: &[u16]) -> Result<Vec<f32>, WordOrderError>;
    // ... i32/u64/i64/f64 block variants
}
```

Word order is applied **per scalar chunk**, not across the whole slice. Each
fixed-width group of registers (2 for 32-bit, 4 for 64-bit) is combined
independently using the selected `WordOrder`; chunk order is preserved.

- Little-endian `decode_u32_block([0x1111, 0x2222, 0x3333, 0x4444])`
  `-> [0x2222_1111, 0x4444_3333]` (each pair reversed, pairs stay in order).
- Big-endian `decode_u32_block([0x1111, 0x2222, 0x3333, 0x4444])`
  `-> [0x1111_2222, 0x3333_4444]`.

An empty input is a valid multiple of every scalar width, so
`decode_*_block(&[])` returns `Ok(vec![])`.

Bad lengths return a dedicated error rather than panicking:

```rust
/// Error returned when a register block cannot be decoded into wide scalars.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WordOrderError {
    /// The register count is not an exact multiple of the scalar width.
    #[error("register block length {len} is not a multiple of {width} (for {type_name})")]
    InvalidBlockLength {
        /// Number of registers supplied.
        len: usize,
        /// Registers per scalar (2 for 32-bit, 4 for 64-bit).
        width: usize,
        /// Target scalar type name, e.g. `"f32"`.
        type_name: &'static str,
    },
}
```

`WordOrderError` is a standalone error type (not folded into `ModbusError`) so
the codec layer stays independent of the transport error surface.

### Float semantics

- Decoding reinterprets IEEE-754 bit patterns via `f32::from_bits` (and the
  `f64` equivalent); no numeric conversion or rounding.
- `NaN` payloads, infinities, and `-0.0` are preserved bit-for-bit.

## Tasks

### 1. Core type (`src/word_order.rs`)
- [ ] Add `WordOrder` enum with `Default` (big-endian).
- [ ] Export from `lib.rs`.
- [ ] Do **not** derive `clap::ValueEnum` on `WordOrder` — `clap` is an
      optional, CLI-only dependency and the library must not depend on it.
      The CLI maps its own `{big,little}` `ValueEnum` (or a `FromStr`) into
      `ferrobus::WordOrder`.
- [ ] Unit tests for defaults and `Debug`/`Clone`/`Copy`/`PartialEq`.

### 2. Multi-register scalar codec
- [ ] Implement `decode_u32/i32/f32` (2 registers).
- [ ] Implement `decode_u64/i64/f64` (4 registers).
- [ ] Use `from_bits` for floats (no numeric conversion).
- [ ] Table-driven tests covering both word orders against known vectors.

### 3. Block decode helpers
- [ ] Add `WordOrder::decode_*_block(&[u16]) -> Result<Vec<T>, WordOrderError>`
      for each wide type.
- [ ] Add the standalone `WordOrderError` type; return
      `InvalidBlockLength` (never panic) when the register count is not an
      exact multiple of the scalar width.
- [ ] Empty input returns `Ok(vec![])`; add a test for the empty-slice case.
- [ ] Export `WordOrderError` from `lib.rs`; document all variants.

### 4. CLI support (`examples/modbus_cli.rs`)
- [ ] Split register-read args from coil/discrete-read args. `ReadArgs` is
      currently shared by all four read subcommands; introduce a separate
      `RegisterReadArgs` (address, quantity, `--value-type`, `--word-order`)
      for `holding`/`input` so the wide-value flags cannot appear on
      `coils`/`discrete`. This is what makes the "rejected on coils/discrete"
      requirement a compile-time/parse-time guarantee rather than a runtime
      check.
- [ ] Add `--value-type {u16,u32,i32,f32,u64,i64,f64}` (default `u16` =
      today's behavior), with `--as` as a visible alias. Attach it only to the
      holding/input register read subcommands, not to coils/discrete inputs.
- [ ] Add `--word-order {big,little}` (default `big`) on the same register
      read subcommands (not global), so it cannot be mistaken for changing
      wire encoding on writes/coils. With `--value-type u16` the flag has no
      effect (single-register values have no word order); accept it silently
      and note in help text that it is ignored for `u16`.
- [ ] `QUANTITY` stays a raw register count. When `--value-type` is wider than
      `u16`, require it to be an exact multiple of the scalar width (2 for
      32-bit, 4 for 64-bit) and error clearly otherwise. `--value-type` and
      `QUANTITY` are separate args, so the multiple check cannot be a single
      clap value-parser; validate it at request build time (before
      connecting).
- [ ] Thread the `value_type` and `word_order` selection through to response
      printing (not just request building), so `print_response` has the decode
      config available. `build_request` currently returns only a
      `ModbusRequest`; return (or pass alongside) a small display plan
      carrying the wide-value formatting choice.
- [ ] Combine registers per `--value-type` using `--word-order` before printing.
- [ ] Define `--output` interaction for wide values: `hex` prints the raw
      IEEE/two's-complement bit pattern (`0x...`, fixed width matching the
      type) and `decimal` prints the natural rendering (signed for `i*`, float
      text for `f32/f64`). `u16` behavior is unchanged. Hex examples:
      `i32 -1 -> 0xFFFFFFFF`, `u32 4660 -> 0x00001234`,
      `f32 1.0 -> 0x3F800000` (8 hex digits), `f64 -> 16 hex digits`.
- [ ] Wide-value CLI **writes are out of scope** for this sprint; only reads
      decode wide values. Note this in help text.
- [ ] Update help text (remove the hard "big-endian only" wording; document
      that word order applies only to wide values, registers stay big-endian).
- [ ] CLI parse/build tests, including the `--as` alias, the non-multiple
      `QUANTITY` build-time error, `--word-order` accepted-but-ignored for
      `u16`, and that `--value-type`/`--word-order` are rejected on
      coils/discrete reads (parse-time, via the separate `RegisterReadArgs`).

### 5. Documentation
- [ ] README: new "Word order for wide values" section with a 32-bit float
      example.
- [ ] Clarify that wire/MBAP fields and per-register byte order are always
      big-endian; only multi-register word order is configurable.
- [ ] Rustdoc examples on `WordOrder`.
- [ ] Document every new public item (crate denies missing docs): each
      `decode_*` scalar and block helper, all `WordOrder` and `WordOrderError`
      variants, and every error field.

### 6. Quality gates
- [ ] `cargo test --all-features` green (lib + example + integration). The
      `modbus_cli` example is gated behind the `cli` feature, so plain
      `cargo test` will not compile/run its tests.
- [ ] `cargo clippy --all-targets --all-features` clean (crate denies missing docs).
- [ ] Verify no change to existing wire-format tests (regression guard).

## CLI naming rationale

The read commands print register values; the new flag controls how those raw
16-bit registers are recombined into wider scalars before printing.

- **`--value-type`** (primary) — Domain Modbus tools (`mbpoll`/`modpoll`) use a
  "type" concept (`-t 4:float`, `-t 4:int`) and `od` uses `-t/--format` for the
  interpret-as type. Plain `--type`/`-t` is unavailable and ambiguous here:
  `-t`/`--transaction-id` is already a global flag, the register *kind*
  (coils/holding/input) is already selected by the subcommand, and `--output`
  already owns display formatting (decimal/hex). `--value-type` disambiguates
  all three.
- **`--as`** (visible alias) — reads naturally at the call site
  (`read holding 100 2 --as f32`) and mirrors the "type:float" intent of the
  domain tooling. Kept as an alias so users can pick whichever reads better.
- **`--word-order {big,little}`** — `od` establishes `--endian={big|little}`
  for ordering; `--word-order` is the more precise term for Modbus, since only
  the word order (not per-register byte order) is configurable.

## Acceptance criteria

- Default behavior is byte-for-byte identical to today (big-endian wire,
  `u16`-per-register output).
- Users can decode `u32/i32/f32/u64/i64/f64` across registers with either word
  order.
- Block helpers return `WordOrderError::InvalidBlockLength` (never panic) when
  the register count is not a multiple of the scalar width; an empty slice
  decodes to an empty result without error.
- Float decode preserves bit patterns (`NaN`, infinities, `-0.0`).
- MBAP header, addresses, quantities, and per-register byte order remain
  big-endian in all modes.
- CLI can read holding registers and print them as little-endian-word `f32`
  via `--value-type f32 --word-order little` (or the `--as` alias), with
  `QUANTITY` interpreted as a raw register count that must be even for 32-bit
  types. `--word-order` is accepted but ignored for `--value-type u16`, and
  `--value-type`/`--word-order` are rejected on coil/discrete reads.
- New public APIs are documented (crate uses `#![deny(missing_docs)]`).

## Risks / notes

- Keep the codec layer separate from transport to avoid regressions in the
  MBAP/PDU path.
- If a concrete device later needs per-register byte swapping, extend with a
  separate `ByteOrder` type; do not overload `WordOrder`.

## Estimate

~2–4 days for one engineer, tests included.
