use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub const MAX_HOST_STREAM_REGS: usize = 9;
pub const HOST_STREAM_BUFFER_CAPACITY: usize = 2048;
pub const EMBEDDED_CSV_BUDGET_BYTES: usize = 32768;
pub const REGISTER_ADDRESS_SPACE_SIZE: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceModel {
    pub device_name: String,
    pub i2c_address_7bit: u8,
    pub i2c_internal_pullups: bool,
    pub i2c_respond_to_general_call: bool,
    pub default_fill: u8,
    pub auto_increment: bool,
    pub registers: Vec<RegisterDef>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RegisterDef {
    pub addr: u16,
    pub default: u8,
    pub access: Access,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub csv: Option<CsvSourceDef>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct CsvSourceDef {
    pub path: String,
    pub mode: CsvModeDef,
}

#[derive(Debug, Deserialize, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CsvModeDef {
    Embedded,
    HostStream,
}

#[derive(Debug, Deserialize, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Ro,
    Rw,
}

#[derive(Debug, Clone)]
struct ResolvedCsvSpec {
    addr: u16,
    mode: CsvModeDef,
    data: Vec<u8>,
}

fn validate(model: &DeviceModel) -> Result<()> {
    ensure!(
        (0x08..=0x77).contains(&model.i2c_address_7bit),
        "i2c_address_7bit must be in 0x08..=0x77 (0x00..=0x07 and 0x78..=0x7F are reserved)"
    );
    ensure!(
        model.registers.len() <= REGISTER_ADDRESS_SPACE_SIZE,
        "register count {} exceeds 8-bit address space capacity {}",
        model.registers.len(),
        REGISTER_ADDRESS_SPACE_SIZE
    );

    let mut seen = [false; REGISTER_ADDRESS_SPACE_SIZE];
    let mut host_stream_count = 0usize;

    for reg in &model.registers {
        ensure!(
            reg.addr < REGISTER_ADDRESS_SPACE_SIZE as u16,
            "register addr {} is outside 8-bit address space (0..=255)",
            reg.addr,
        );
        let idx = reg.addr as usize;
        if seen[idx] {
            bail!("duplicate register addr {}", reg.addr);
        }
        seen[idx] = true;

        if let Some(csv) = &reg.csv {
            ensure!(
                matches!(reg.access, Access::Ro),
                "register addr {} uses CSV but is not read-only",
                reg.addr
            );
            ensure!(
                !csv.path.trim().is_empty(),
                "register addr {} has empty csv.path",
                reg.addr
            );
            if matches!(csv.mode, CsvModeDef::HostStream) {
                host_stream_count += 1;
            }
        }
    }

    ensure!(
        host_stream_count <= MAX_HOST_STREAM_REGS,
        "host_stream register count {} exceeds limit {}",
        host_stream_count,
        MAX_HOST_STREAM_REGS
    );

    Ok(())
}

pub fn parse_model(model_json: &str) -> Result<DeviceModel> {
    let model: DeviceModel = serde_json::from_str(model_json).context("parsing model JSON")?;
    validate(&model)?;
    Ok(model)
}

pub fn parse_csv_samples(csv_text: &str, source_name: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();

    for (lineno, raw_line) in csv_text.lines().enumerate() {
        let line_no = lineno + 1;
        let no_comment = raw_line.split('#').next().unwrap_or("");
        let trimmed = no_comment.trim();
        if trimmed.is_empty() {
            continue;
        }

        let first_col = trimmed.split(',').next().unwrap_or("").trim();
        if first_col.is_empty() {
            continue;
        }

        let value = if let Some(hex) = first_col
            .strip_prefix("0x")
            .or_else(|| first_col.strip_prefix("0X"))
        {
            u16::from_str_radix(hex, 16).with_context(|| {
                format!(
                    "invalid hex byte '{}' at {}:{}",
                    first_col, source_name, line_no
                )
            })?
        } else {
            first_col.parse::<u16>().with_context(|| {
                format!(
                    "invalid decimal byte '{}' at {}:{}",
                    first_col, source_name, line_no
                )
            })?
        };

        ensure!(
            value <= 0xFF,
            "value {} out of range at {}:{} (must be 0..255)",
            value,
            source_name,
            line_no
        );

        out.push(value as u8);
    }

    ensure!(
        !out.is_empty(),
        "{} does not contain any usable CSV samples",
        source_name
    );

    Ok(out)
}

pub fn load_csv_samples(path: &Path) -> Result<Vec<u8>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_csv_samples(&text, &path.display().to_string())
}

pub fn resolve_csv_paths(model: &DeviceModel, model_path: &Path) -> Vec<PathBuf> {
    let base = model_path.parent().unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();
    for reg in &model.registers {
        if let Some(csv) = &reg.csv {
            out.push(base.join(&csv.path));
        }
    }
    out
}

fn resolve_csv_specs(model: &DeviceModel, model_path: &Path) -> Result<Vec<ResolvedCsvSpec>> {
    let mut specs = Vec::new();
    for reg in &model.registers {
        let Some(csv) = &reg.csv else {
            continue;
        };

        let path = model_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&csv.path);
        let samples = load_csv_samples(&path).with_context(|| {
            format!(
                "loading CSV for register 0x{:02X} from {}",
                reg.addr,
                path.display()
            )
        })?;

        specs.push(ResolvedCsvSpec {
            addr: reg.addr,
            mode: csv.mode,
            data: if matches!(csv.mode, CsvModeDef::Embedded) {
                samples
            } else {
                Vec::new()
            },
        });
    }

    specs.sort_by_key(|spec| spec.addr);
    enforce_embedded_csv_budget(&specs)?;
    Ok(specs)
}

fn enforce_embedded_csv_budget(csv_specs: &[ResolvedCsvSpec]) -> Result<()> {
    let budget = EMBEDDED_CSV_BUDGET_BYTES;
    let mut total = 0usize;
    let mut per_register = Vec::new();

    for spec in csv_specs {
        if !matches!(spec.mode, CsvModeDef::Embedded) {
            continue;
        }

        let len = spec.data.len();
        total = total.checked_add(len).with_context(|| {
            format!(
                "embedded CSV size overflow while summing register 0x{:02X}",
                spec.addr
            )
        })?;
        per_register.push((spec.addr, len));
    }

    if total <= budget {
        return Ok(());
    }

    let over = total - budget;
    let breakdown = per_register
        .into_iter()
        .map(|(addr, len)| format!("0x{:02X}:{}B", addr, len))
        .collect::<Vec<_>>()
        .join(", ");

    bail!(
        "embedded CSV payload exceeds budget: {} bytes total (budget {} bytes, over by {} bytes). Per-register embedded sizes: {}. Reduce embedded CSV sizes or switch registers to host_stream.",
        total,
        budget,
        over,
        breakdown
    );
}

fn render_i16_array(out: &mut String, values: &[i16]) {
    let _ = writeln!(out, "[");
    for chunk in values.chunks(16) {
        let _ = write!(out, "    ");
        for value in chunk {
            let _ = write!(out, "{}, ", value);
        }
        let _ = writeln!(out);
    }
    let _ = write!(out, "]");
}

fn render_u8_array(out: &mut String, values: &[u8]) {
    let _ = writeln!(out, "[");
    for chunk in values.chunks(16) {
        let _ = write!(out, "    ");
        for value in chunk {
            let _ = write!(out, "0x{:02X}, ", value);
        }
        let _ = writeln!(out);
    }
    let _ = write!(out, "]");
}

fn render_csv_data_const(out: &mut String, idx: usize, values: &[u8]) {
    let _ = writeln!(out, "const CSV_DATA_{}: &[u8] = &", idx);
    render_u8_array(out, values);
    let _ = writeln!(out, ";");
}

fn generate_model_rs(model: &DeviceModel, csv_specs: &[ResolvedCsvSpec]) -> String {
    let mut out = String::new();
    let embedded_csv_total_bytes: usize = csv_specs
        .iter()
        .filter(|spec| matches!(spec.mode, CsvModeDef::Embedded))
        .map(|spec| spec.data.len())
        .sum();

    let mut registers = model.registers.clone();
    registers.sort_by_key(|r| r.addr);

    let address_space_size = REGISTER_ADDRESS_SPACE_SIZE;
    let mut csv_index_by_addr = vec![-1i16; address_space_size];
    let mut host_index_by_addr = vec![-1i16; address_space_size];
    let mut host_stream_addrs = Vec::new();

    for (csv_idx, spec) in csv_specs.iter().enumerate() {
        csv_index_by_addr[spec.addr as usize] = csv_idx as i16;
        if matches!(spec.mode, CsvModeDef::HostStream) {
            host_index_by_addr[spec.addr as usize] = host_stream_addrs.len() as i16;
            host_stream_addrs.push(spec.addr as u8);
        }
    }

    let _ = writeln!(
        out,
        "// Generated by tools/gen_model. Do not edit manually."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "#![allow(dead_code)]");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "pub const DEVICE_NAME: &str = {:?};",
        model.device_name
    );
    let _ = writeln!(
        out,
        "pub const I2C_ADDRESS_7BIT: u8 = 0x{:02X};",
        model.i2c_address_7bit
    );
    let _ = writeln!(
        out,
        "pub const I2C_INTERNAL_PULLUPS: bool = {};",
        model.i2c_internal_pullups
    );
    let _ = writeln!(
        out,
        "pub const I2C_RESPOND_TO_GENERAL_CALL: bool = {};",
        model.i2c_respond_to_general_call
    );
    let _ = writeln!(out, "pub const REG_COUNT: usize = {};", address_space_size);
    let _ = writeln!(
        out,
        "pub const DEFAULT_FILL: u8 = 0x{:02X};",
        model.default_fill
    );
    let _ = writeln!(
        out,
        "pub const AUTO_INCREMENT: bool = {};",
        model.auto_increment
    );
    let _ = writeln!(
        out,
        "pub const MAX_HOST_STREAM_REGS: usize = {};",
        MAX_HOST_STREAM_REGS
    );
    let _ = writeln!(
        out,
        "pub const HOST_STREAM_BUFFER_CAPACITY: usize = {};",
        HOST_STREAM_BUFFER_CAPACITY
    );
    let _ = writeln!(
        out,
        "pub const EMBEDDED_CSV_BUDGET_BYTES: usize = {};",
        EMBEDDED_CSV_BUDGET_BYTES
    );
    let _ = writeln!(
        out,
        "pub const EMBEDDED_CSV_TOTAL_BYTES: usize = {};",
        embedded_csv_total_bytes
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "#[derive(Copy, Clone, Debug, Eq, PartialEq)]");
    let _ = writeln!(out, "pub enum Access {{");
    let _ = writeln!(out, "    Ro,");
    let _ = writeln!(out, "    Rw,");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "#[derive(Copy, Clone, Debug, Eq, PartialEq)]");
    let _ = writeln!(out, "pub enum CsvMode {{");
    let _ = writeln!(out, "    Embedded,");
    let _ = writeln!(out, "    HostStream,");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "#[derive(Copy, Clone, Debug, Eq, PartialEq)]");
    let _ = writeln!(out, "pub struct RegisterSpec {{");
    let _ = writeln!(out, "    pub addr: u8,");
    let _ = writeln!(out, "    pub default: u8,");
    let _ = writeln!(out, "    pub access: Access,");
    let _ = writeln!(out, "    pub name: &'static str,");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "#[derive(Copy, Clone, Debug, Eq, PartialEq)]");
    let _ = writeln!(out, "pub struct CsvRegisterSpec {{");
    let _ = writeln!(out, "    pub addr: u8,");
    let _ = writeln!(out, "    pub mode: CsvMode,");
    let _ = writeln!(out, "    pub data: &'static [u8],");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
    let _ = writeln!(out, "pub const REGISTERS: &[RegisterSpec] = &[");

    for reg in registers {
        let access = match reg.access {
            Access::Ro => "Access::Ro",
            Access::Rw => "Access::Rw",
        };
        let _ = writeln!(
            out,
            "    RegisterSpec {{ addr: 0x{:02X}, default: 0x{:02X}, access: {}, name: {:?} }},",
            reg.addr, reg.default, access, reg.name,
        );
    }

    let _ = writeln!(out, "];");
    let _ = writeln!(out);

    for (idx, spec) in csv_specs.iter().enumerate() {
        if matches!(spec.mode, CsvModeDef::Embedded) {
            render_csv_data_const(&mut out, idx, &spec.data);
            let _ = writeln!(out);
        }
    }

    let _ = writeln!(out, "pub const CSV_REGISTERS: &[CsvRegisterSpec] = &[");
    for (idx, spec) in csv_specs.iter().enumerate() {
        let (mode, data_ref) = match spec.mode {
            CsvModeDef::Embedded => ("CsvMode::Embedded", format!("CSV_DATA_{}", idx)),
            CsvModeDef::HostStream => ("CsvMode::HostStream", "&[]".to_string()),
        };
        let _ = writeln!(
            out,
            "    CsvRegisterSpec {{ addr: 0x{:02X}, mode: {}, data: {} }},",
            spec.addr, mode, data_ref
        );
    }
    let _ = writeln!(out, "];");
    let _ = writeln!(out);

    let _ = write!(out, "pub const CSV_INDEX_BY_ADDR: [i16; REG_COUNT] = ");
    render_i16_array(&mut out, &csv_index_by_addr);
    let _ = writeln!(out, ";");
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "pub const HOST_STREAM_COUNT: usize = {};",
        host_stream_addrs.len()
    );
    let _ = write!(
        out,
        "pub const HOST_STREAM_ADDRS: [u8; HOST_STREAM_COUNT] = "
    );
    render_u8_array(&mut out, &host_stream_addrs);
    let _ = writeln!(out, ";");
    let _ = writeln!(out);

    let _ = write!(
        out,
        "pub const HOST_STREAM_INDEX_BY_ADDR: [i16; REG_COUNT] = "
    );
    render_i16_array(&mut out, &host_index_by_addr);
    let _ = writeln!(out, ";");

    out
}

pub fn generate_from_paths(input_path: &Path, output_path: &Path) -> Result<()> {
    let input = fs::read_to_string(input_path)
        .with_context(|| format!("reading {}", input_path.display()))?;
    let model = parse_model(&input)?;
    let csv_specs = resolve_csv_specs(&model, input_path)?;
    let generated = generate_model_rs(&model, &csv_specs);
    fs::write(output_path, generated)
        .with_context(|| format!("writing {}", output_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use anyhow::Result;
    use proptest::prelude::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    fn find_or_panic(string: &str, sub_string: &str) -> usize {
        string
            .find(sub_string)
            .unwrap_or_else(|| panic!("expected to find {sub_string:?} in generated output"))
    }

    #[test]
    fn parse_model_accepts_boundary_variants() -> Result<()> {
        let min_model = json!({
            "device_name": "min",
            "i2c_address_7bit": 0x08,
            "default_fill": 0,
            "auto_increment": false,
            "i2c_internal_pullups": false,
            "i2c_respond_to_general_call": false,
            "registers": [{
                "addr": 0,
                "default": 1,
                "access": "ro",
                "name": "ONLY"
            }]
        })
        .to_string();
        parse_model(&min_model)?;

        let mut registers = Vec::new();
        for idx in 0u16..9u16 {
            registers.push(json!({
                "addr": idx,
                "default": (idx as u8),
                "access": "ro",
                "name": format!("HS{}", idx),
                "csv": {"path": format!("csv/hs_{}.csv", idx), "mode": "host_stream"}
            }));
        }
        registers.push(json!({
            "addr": 20,
            "default": 0xA0,
            "access": "ro",
            "name": "EMB",
            "csv": {"path": "csv/embedded.csv", "mode": "embedded"}
        }));
        registers.push(json!({
            "addr": 200,
            "default": 0x55,
            "access": "rw",
            "name": "RW",
        }));

        let max_model = json!({
            "device_name": "max",
            "i2c_address_7bit": 0x77,
            "default_fill": 0xff,
            "auto_increment": true,
            "i2c_internal_pullups": true,
            "i2c_respond_to_general_call": true,
            "registers": registers
        })
        .to_string();

        parse_model(&max_model)?;
        Ok(())
    }

    #[test]
    fn parse_model_accepts_sparse_register_addresses() -> Result<()> {
        let sparse = json!({
            "device_name": "sparse",
            "i2c_address_7bit": 0x42,
            "default_fill": 0,
            "auto_increment": false,
            "i2c_internal_pullups": true,
            "i2c_respond_to_general_call": false,
            "registers": [
                {"addr": 0x00, "default": 0x11, "access": "ro", "name": "LOW"},
                {"addr": 0x32, "default": 0x22, "access": "rw", "name": "HIGH"},
            ]
        })
        .to_string();

        parse_model(&sparse)?;
        Ok(())
    }

    #[test]
    fn parse_model_rejects_invalid_shape_and_constraints() {
        let invalid_cases = [
            (
                "bad_i2c_address_out_of_7bit_range",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x80,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": []
                    })
                .to_string(),
                "i2c_address_7bit must be in 0x08..=0x77",
            ),
            (
                "bad_i2c_address_reserved_low",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x00,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": []
                    })
                .to_string(),
                "i2c_address_7bit must be in 0x08..=0x77",
            ),
            (
                "bad_i2c_address_reserved_high",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x7f,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": []
                    })
                .to_string(),
                "i2c_address_7bit must be in 0x08..=0x77",
            ),
            (
                "out_of_range_register",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x42,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": [{
                            "addr": 256,
                            "default": 0,
                            "access": "ro",
                            "name": "X"
                        }]
                    })
                .to_string(),
                "outside 8-bit address space",
            ),
            (
                "duplicate_address",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x42,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": [
                            {"addr": 2, "default": 0, "access": "ro", "name": "A"},
                            {"addr": 2, "default": 1, "access": "rw", "name": "B"}
                        ]
                    })
                .to_string(),
                "duplicate register addr",
            ),
            (
                "csv_on_rw",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x42,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": [{
                            "addr": 2,
                            "default": 0,
                            "access": "rw",
                            "name": "CSV_RW",
                            "csv": {"path": "x.csv", "mode": "embedded"}
                        }]
                    })
                .to_string(),
                "uses CSV but is not read-only",
            ),
            (
                "empty_csv_path",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x42,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": [{
                            "addr": 2,
                            "default": 0,
                            "access": "ro",
                            "name": "CSV_EMPTY",
                            "csv": {"path": " ", "mode": "embedded"}
                        }]
                    })
                .to_string(),
                "empty csv.path",
            ),
            (
                "too_many_host_stream_registers",
                json!({
                "device_name": "bad",
                "i2c_address_7bit": 0x42,
                "default_fill": 0,
                "auto_increment": true,
                "i2c_internal_pullups": true,
                "i2c_respond_to_general_call": true,
                        "registers": (0u16..10u16).map(|addr| json!({
                            "addr": addr,
                            "default": 0,
                            "access": "ro",
                            "name": format!("HS{}", addr),
                            "csv": {"path": format!("{}.csv", addr), "mode": "host_stream"}
                        })).collect::<Vec<_>>()
                    })
                .to_string(),
                "exceeds limit",
            ),
        ];

        for (name, model, expected) in invalid_cases {
            let err = parse_model(&model).unwrap_err().to_string();
            assert!(
                err.contains(expected),
                "{name}: expected error containing {expected:?}, got: {err}"
            );
        }

        let unknown_field = r#"{
        "device_name": "bad",
        "i2c_address_7bit": 66,
        "default_fill": 0,
        "auto_increment": true,
        "i2c_internal_pullups": true,
        "i2c_respond_to_general_call": true,
            "registers": [],
            "unknown_field": 1
        }"#;
        let err = parse_model(unknown_field).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("unknown field"));

        let missing_field = r#"{
            "device_name": "bad",
            "i2c_address_7bit": 66,
            "default_fill": 0,
            "auto_increment": true,
            "i2c_respond_to_general_call": true,
            "registers": []
        }"#;
        let err = parse_model(missing_field).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("missing field"));

        let invalid_enum = r#"{
            "device_name": "bad",
            "i2c_address_7bit": 66,
            "default_fill": 0,
            "auto_increment": true,
        "i2c_internal_pullups": true,
        "i2c_respond_to_general_call": true,
            "registers": [{
                "addr": 0,
                "default": 0,
                "access": "write_only",
                "name": "BAD_ACCESS"
            }]
        }"#;
        let err = parse_model(invalid_enum).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("unknown variant"));
    }

    #[test]
    fn parse_csv_samples_parses_expected_formats() -> Result<()> {
        let csv = r#"
            # comment only
            42
            0x2a
            0X2A,ignored_column

            255 # inline comment
            0x00
        "#;
        let parsed = parse_csv_samples(csv, "sample.csv")?;
        assert_eq!(parsed, vec![42, 42, 42, 255, 0]);
        Ok(())
    }

    #[test]
    fn parse_csv_samples_rejects_invalid_content() {
        let invalid_cases = [
            ("invalid_decimal", "abc\n", "invalid decimal byte"),
            ("invalid_hex", "0xGG\n", "invalid hex byte"),
            ("out_of_range", "256\n", "out of range"),
            (
                "empty_after_filtering",
                "  # only comments\n\n",
                "does not contain",
            ),
        ];

        for (name, csv, expected) in invalid_cases {
            let err = parse_csv_samples(csv, "bad.csv").unwrap_err().to_string();
            assert!(
                err.contains(expected),
                "{name}: expected {expected:?}, got: {err}"
            );
        }
    }

    #[test]
    fn generate_fw_model_sorts_and_builds_indices() -> Result<()> {
        let dir = TempDir::new()?;
        let model_path = dir.path().join("models").join("device_model.json");
        let embedded_csv = dir.path().join("models").join("csv").join("embedded.csv");
        let host_csv = dir.path().join("models").join("csv").join("host.csv");

        write_file(&embedded_csv, "0xA0\n0xA1\n0xA2\n")?;
        write_file(&host_csv, "10\n20\n30\n")?;

        let model = json!({
            "device_name": "deterministic",
            "i2c_address_7bit": 0x42,
            "default_fill": 0xFF,
            "auto_increment": true,
            "i2c_internal_pullups": true,
            "i2c_respond_to_general_call": true,
            "registers": [
                {
                    "addr": 32,
                    "default": 0xA0,
                    "access": "ro",
                    "name": "REG_EMBEDDED",
                    "csv": {"path": "csv/embedded.csv", "mode": "embedded"}
                },
                {
                    "addr": 1,
                    "default": 0x34,
                    "access": "ro",
                    "name": "REG_ID1"
                },
                {
                    "addr": 17,
                    "default": 0xA1,
                    "access": "ro",
                    "name": "REG_HOST",
                    "csv": {"path": "csv/host.csv", "mode": "host_stream"}
                },
                {
                    "addr": 16,
                    "default": 0,
                    "access": "rw",
                    "name": "REG_RW"
                }
            ]
        })
        .to_string();
        write_file(&model_path, &model)?;

        let parsed = parse_model(&model)?;
        let csv_specs = resolve_csv_specs(&parsed, &model_path)?;
        let generated = generate_model_rs(&parsed, &csv_specs);

        let reg_1 = find_or_panic(&generated, "RegisterSpec { addr: 0x01");
        let reg_16 = find_or_panic(&generated, "RegisterSpec { addr: 0x10");
        let reg_17 = find_or_panic(&generated, "RegisterSpec { addr: 0x11");
        let reg_32 = find_or_panic(&generated, "RegisterSpec { addr: 0x20");
        assert!(reg_1 < reg_16 && reg_16 < reg_17 && reg_17 < reg_32);

        assert!(generated.contains("pub const I2C_INTERNAL_PULLUPS: bool = true;"));
        assert!(generated.contains("pub const I2C_RESPOND_TO_GENERAL_CALL: bool = true;"));
        assert!(generated.contains("pub const HOST_STREAM_COUNT: usize = 1;"));
        assert!(
            generated
                .contains("CsvRegisterSpec { addr: 0x11, mode: CsvMode::HostStream, data: &[] },")
        );
        assert!(generated.contains(
            "CsvRegisterSpec { addr: 0x20, mode: CsvMode::Embedded, data: CSV_DATA_1 },"
        ));
        assert!(generated.contains("const CSV_DATA_1: &[u8] = &"));
        assert!(!generated.contains("const CSV_DATA_0: &[u8] = &"));
        assert!(generated.contains("pub const HOST_STREAM_ADDRS: [u8; HOST_STREAM_COUNT] = ["));
        assert!(generated.contains("    0x11, "));
        Ok(())
    }

    #[test]
    fn generate_from_paths_enforces_embedded_budget() -> Result<()> {
        let dir = TempDir::new()?;
        let model_path = dir.path().join("models").join("device_model.json");
        let csv_path = dir.path().join("models").join("csv").join("huge.csv");
        let output_path = dir.path().join("out").join("model.rs");

        let mut huge_csv = String::new();
        for idx in 0..(EMBEDDED_CSV_BUDGET_BYTES + 1) {
            let _ = writeln!(huge_csv, "{}", idx % 256);
        }
        write_file(&csv_path, &huge_csv)?;

        let model = json!({
            "device_name": "budget",
            "i2c_address_7bit": 0x42,
            "default_fill": 0,
            "auto_increment": true,
            "i2c_internal_pullups": true,
            "i2c_respond_to_general_call": true,
            "registers": [{
                "addr": 32,
                "default": 0,
                "access": "ro",
                "name": "HUGE",
                "csv": {"path": "csv/huge.csv", "mode": "embedded"}
            }]
        })
        .to_string();
        write_file(&model_path, &model)?;

        let err = generate_from_paths(&model_path, &output_path)
            .expect_err("expected embedded CSV budget overflow to fail")
            .to_string();
        assert!(err.contains("embedded CSV payload exceeds budget"));
        Ok(())
    }

    proptest! {
        #[test]
        fn prop_random_valid_models_parse(
            i2c in 0x08u8..=0x77,
            default_fill in any::<u8>(),
            auto_increment in any::<bool>(),
            i2c_internal_pullups in any::<bool>(),
            i2c_respond_to_general_call in any::<bool>(),
            addrs in prop::collection::btree_set(0u16..=255, 1..24),
            access_bits in prop::collection::vec(any::<bool>(), 0..24),
            defaults in prop::collection::vec(any::<u8>(), 0..24),
        ) {
            let mut registers = Vec::new();
            for (idx, addr) in addrs.iter().copied().enumerate() {
                let access = if access_bits.get(idx).copied().unwrap_or(false) {
                    "rw"
                } else {
                    "ro"
                };
                let default = defaults.get(idx).copied().unwrap_or(0);
                registers.push(json!({
                    "addr": addr,
                    "default": default,
                    "access": access,
                    "name": format!("R{}", idx),
                }));
            }

            let model = json!({
                "device_name": "prop_valid",
                "i2c_address_7bit": i2c,
                "default_fill": default_fill,
                "auto_increment": auto_increment,
                "i2c_internal_pullups": i2c_internal_pullups,
                "i2c_respond_to_general_call": i2c_respond_to_general_call,
                "registers": registers
            })
            .to_string();

            prop_assert!(parse_model(&model).is_ok());
        }
    }

    proptest! {
        #[test]
        fn prop_targeted_invalid_duplicate_rejected(
            addr in 0u16..=255,
            default_a in any::<u8>(),
            default_b in any::<u8>(),
        ) {
            let model = json!({
                "device_name": "prop_invalid",
                "i2c_address_7bit": 0x42,
                "default_fill": 0,
                "auto_increment": true,
            "i2c_internal_pullups": true,
            "i2c_respond_to_general_call": true,
                "registers": [
                    {"addr": addr, "default": default_a, "access": "ro", "name": "A"},
                    {"addr": addr, "default": default_b, "access": "rw", "name": "B"}
                ]
            })
            .to_string();

            let err = parse_model(&model).expect_err("duplicate register address should fail");
            prop_assert!(err.to_string().contains("duplicate register addr"));
        }
    }

    proptest! {
        #[test]
        fn prop_csv_round_trip_randomized(
            entries in prop::collection::vec(
                (any::<u8>(), any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()),
                1..128
            )
        ) {
            let mut csv = String::new();
            let mut expected = Vec::new();

            for (idx, (value, use_hex, uppercase, with_extra_col, with_inline_comment)) in entries.iter().copied().enumerate() {
                if idx % 7 == 0 {
                    csv.push_str("# standalone comment line\n\n");
                }

                let mut token = if use_hex {
                    if uppercase {
                        format!("0X{value:02X}")
                    } else {
                        format!("0x{value:02x}")
                    }
                } else {
                    value.to_string()
                };

                if with_extra_col {
                    token.push_str(",999,unused");
                }
                if with_inline_comment {
                    token.push_str(" # inline");
                }

                csv.push_str(&token);
                csv.push('\n');
                expected.push(value);
            }

            let parsed = parse_csv_samples(&csv, "prop.csv").expect("property CSV should parse");
            prop_assert_eq!(parsed, expected);
        }
    }
}
