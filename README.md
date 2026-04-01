# morphIIC initial prototype

RP2040 firmware emulates an I2C register-mapped device using Embassy I2C slave. Register behavior is generated from JSON.

## Wiring

### RP2040 (morphIIC)

- I2C1 SCL: `GP3`
- I2C1 SDA: `GP2`
- USB: host connection for CLI and stream feeder (`dump`, `reset_i2c`, `clear`, `stream_status`, `help`)

## Device model

Primary model: `models/device_model.json`.

Hard rule enforced by generator:

- `addr_width_bits` must be `8`.

Per-register CSV sources are optional:

- `csv.mode = "embedded"`: samples are compiled into firmware and wrap on EOF.
- `csv.mode = "host_stream"`: samples are fed at runtime over USB stream channel.
- Embedded CSV data has a cumulative byte budget across all `embedded` registers:
  - hard-set budget: `32768` bytes (`32 KiB`)
  - generator fails early if the cumulative embedded payload exceeds budget, with per-register byte breakdown

Example register entry:

```json
{
  "addr": 32,
  "default": 0,
  "access": "ro",
  "name": "ACCEL_X",
  "csv": {
    "path": "imu/accel_x.csv",
    "mode": "host_stream"
  }
}
```

CSV parsing rules:

- First column is used.
- Decimal (`42`) and hex (`0x2A`) are supported.
- Empty lines and `#` comments are ignored.

## Build RP2040 firmware

From repo root (`morphIIC/`):

```bash
cargo run -p gen_model -- models/device_model.json firmware/src/model.rs
cargo build -p morphiic-firmware --release --target thumbv6m-none-eabi
```

`firmware/build.rs` also regenerates `firmware/src/model.rs` automatically on build and tracks referenced CSV files as rebuild inputs.

## Flash RP2040

Put RP2040 into boot mode by holding reset button and pressing boot button.

From `firmware/`:

```bash
cargo run --release
```

Alternative: copy generated UF2 manually in mass-storage mode.

## Host stream feeder

Build and run:

```bash
cargo run -p csv_streamer -- /dev/ttyACM0 models/device_model.json
```

- Uses the second USB CDC channel.
- Sends `HELLO` to discover stream slots.
- Prefills each stream buffer to 75% by default.
- Keeps buffers above 50% data mark.
- CSV playback wraps on EOF.

## Current limitations

- Single-address emulation only (no multi-address).
- No runtime model hot-swap; model is still compile-time generated from JSON.
- Register space is 8-bit addressed (0..255).
