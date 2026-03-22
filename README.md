# morphIIC initial prototype

RP2040 firmware emulates an I2C register-mapped device using Embassy I2C slave. Register behavior is generated from JSON.

## Wiring

### RP2040 (morphIIC)

- I2C1 SCL: `GP3`
- I2C1 SDA: `GP2`
- USB: host connection for commands through serial terminal (`dump`, `reset_i2c`, `clear`, `help`)

## Device model

Test model: `models/device_model.json`.

MVP hard rule enforced by generator:

- `addr_width_bits` must be `8`.

## Build RP2040 firmware

From repo root (`morphIIC/`):

```bash
cargo run -p gen_model -- models/device_model.json firmware/src/model.rs
cargo build -p morphiic-firmware --release --target thumbv6m-none-eabi
```

`firmware/build.rs` also regenerates `firmware/src/model.rs` automatically on build.

## Flash RP2040

Put RP2040 into boot mode by holding reset button and pressing boot button.

From `firmware/`:

```bash
cargo run --release
```

Alternative: copy generated UF2 manually in mass-storage mode.

## Known limitations

- Single-address emulation only (no multi-address).
- No runtime model download; model is compile-time generated from JSON.
- No CSV streaming/UI.
- Register space is 8-bit addressed (0..255) in this MVP.
