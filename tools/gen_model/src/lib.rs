use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub const MAX_HOST_STREAM_REGS: usize = 9;
pub const HOST_STREAM_BUFFER_CAPACITY: usize = 2048;
pub const EMBEDDED_CSV_BUDGET_BYTES: usize = 32768;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceModel {
    pub device_name: String,
    pub i2c_address_7bit: u8,
    pub addr_width_bits: u8,
    pub reg_count: u16,
    pub default_fill: u8,
    pub auto_increment: bool,
    pub registers: Vec<RegisterDef>,
}

#[derive(Debug, Deserialize, Clone)]
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
        model.addr_width_bits == 8,
        "MVP requires addr_width_bits == 8, got {}",
        model.addr_width_bits
    );
    ensure!(
        model.i2c_address_7bit <= 0x7f,
        "i2c_address_7bit must be <= 0x7f"
    );
    ensure!(model.reg_count > 0, "reg_count must be > 0");
    ensure!(
        model.reg_count <= 256,
        "reg_count must be <= 256 for 8-bit addresses"
    );

    let mut seen = [false; 256];
    let mut host_stream_count = 0usize;

    for reg in &model.registers {
        ensure!(
            reg.addr < model.reg_count,
            "register addr {} is outside reg_count {}",
            reg.addr,
            model.reg_count
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

    let reg_count = model.reg_count as usize;
    let mut csv_index_by_addr = vec![-1i16; reg_count];
    let mut host_index_by_addr = vec![-1i16; reg_count];
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
        "pub const ADDR_WIDTH_BITS: u8 = {};",
        model.addr_width_bits
    );
    let _ = writeln!(out, "pub const REG_COUNT: usize = {};", model.reg_count);
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
