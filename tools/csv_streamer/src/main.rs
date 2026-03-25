use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use gen_model::CsvModeDef;
use serialport::SerialPort;

const DEFAULT_MODEL_PATH: &str = "models/device_model.json";
const SERIAL_BAUD: u32 = 115_200;

// Stream protocol frame format (little-endian):
//   byte 0: opcode
//   byte 1..2: payload length (u16 LE)
//   byte 3..: payload bytes
// Request opcodes are low values, responses/acks use the high bit (0x80+).
#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum HostOp {
    HelloReq = 0x01,
    Feed = 0x02,
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum DeviceOp {
    HelloResp = 0x81,
    FeedAck = 0x82,
    Error = 0xFF,
}

impl TryFrom<u8> for DeviceOp {
    type Error = u8;

    fn try_from(value: u8) -> core::result::Result<Self, u8> {
        match value {
            x if x == Self::HelloResp as u8 => Ok(Self::HelloResp),
            x if x == Self::FeedAck as u8 => Ok(Self::FeedAck),
            x if x == Self::Error as u8 => Ok(Self::Error),
            _ => Err(value),
        }
    }
}

const STREAM_PROTO_VERSION: u8 = 1;

const FEED_CHUNK_BYTES: usize = 48;
const PREFILL_NUMERATOR: usize = 3;
const PREFILL_DENOMINATOR: usize = 4;
const LOW_DATA_MARK_NUMERATOR: usize = 1;
const LOW_DATA_MARK_DENOMINATOR: usize = 2;
const DEBUG_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const PROBE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct HostSource {
    addr: u8,
    samples: Vec<u8>,
}

#[derive(Debug, Copy, Clone)]
struct DeviceStreamDescriptor {
    stream_id: u8,
    addr: u8,
    capacity: usize,
}

#[derive(Debug)]
struct StreamState {
    stream_id: u8,
    addr: u8,
    capacity: usize,
    free: usize,
    samples: Vec<u8>,
    cursor: usize,
    feed_calls: u64,
    probe_calls: u64,
    accepted_total: u64,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("usage: csv_streamer <serial_port> [model_json]");
        std::process::exit(2);
    }

    let serial_port = &args[1];
    let model_path = if args.len() == 3 {
        PathBuf::from(&args[2])
    } else {
        PathBuf::from(DEFAULT_MODEL_PATH)
    };

    let host_sources = load_host_sources(&model_path)?;
    ensure!(
        !host_sources.is_empty(),
        "no host_stream registers found in {}",
        model_path.display()
    );

    let mut port = serialport::new(serial_port, SERIAL_BAUD)
        .timeout(Duration::from_millis(20))
        .open()
        .with_context(|| format!("opening serial port {serial_port}"))?;

    // Protocol startup sequence:
    // 1. HELLO_REQ asks firmware which host streams exist.
    // 2. HELLO_RESP returns protocol version and each stream descriptor.
    // 3. FEED/FEED_ACK messages maintain stream fill level.
    let descriptors = negotiate_stream_descriptors(&mut *port)?;

    let mut descriptors_by_addr = BTreeMap::new();
    for descriptor in descriptors {
        let prev = descriptors_by_addr.insert(descriptor.addr, descriptor);
        ensure!(
            prev.is_none(),
            "duplicate stream descriptor for addr 0x{:02X}",
            descriptor.addr
        );
    }

    ensure!(
        descriptors_by_addr.len() == host_sources.len(),
        "model has {} host_stream registers but firmware reports {}",
        host_sources.len(),
        descriptors_by_addr.len()
    );

    let mut streams = Vec::new();
    for source in host_sources {
        let descriptor = descriptors_by_addr
            .get(&source.addr)
            .copied()
            .ok_or_else(|| {
                anyhow!(
                    "firmware missing stream descriptor for register 0x{:02X}",
                    source.addr
                )
            })?;

        streams.push(StreamState {
            stream_id: descriptor.stream_id,
            addr: source.addr,
            capacity: descriptor.capacity,
            free: descriptor.capacity,
            samples: source.samples,
            cursor: 0,
            feed_calls: 0,
            probe_calls: 0,
            accepted_total: 0,
        });
    }

    streams.sort_by_key(|stream| stream.stream_id);

    for stream in &mut streams {
        let target_level = stream.capacity * PREFILL_NUMERATOR / PREFILL_DENOMINATOR;
        while fill_level(stream) < target_level {
            if stream.free == 0 {
                break;
            }

            let chunk = FEED_CHUNK_BYTES.min(stream.free);
            let accepted = feed_chunk(&mut *port, stream, chunk)?;
            if accepted == 0 {
                break;
            }
        }
    }

    eprintln!(
        "csv_streamer attached to {} ({} stream(s) active)",
        serial_port,
        streams.len()
    );
    for stream in &streams {
        eprintln!(
            "  id={} addr=0x{:02X} capacity={} samples={}",
            stream.stream_id,
            stream.addr,
            stream.capacity,
            stream.samples.len()
        );
    }

    let mut next_debug_report = Instant::now() + DEBUG_REPORT_INTERVAL;
    let mut next_probe_refresh = Instant::now();

    loop {
        let mut fed_any = false;

        let probe_due = Instant::now() >= next_probe_refresh;
        if probe_due {
            while Instant::now() >= next_probe_refresh {
                next_probe_refresh += PROBE_INTERVAL;
            }
        }

        for stream in &mut streams {
            if probe_due {
                // Poll current free space from device before refill decisions.
                // Device consumption is asynchronous, so host-side `free` must be refreshed.
                feed_chunk(&mut *port, stream, 0)?;
            }

            let low_data_mark =
                stream.capacity * LOW_DATA_MARK_NUMERATOR / LOW_DATA_MARK_DENOMINATOR;
            if fill_level(stream) > low_data_mark {
                continue;
            }

            if stream.free == 0 {
                continue;
            }

            let chunk = FEED_CHUNK_BYTES.min(stream.free);
            if chunk == 0 {
                continue;
            }

            let accepted = feed_chunk(&mut *port, stream, chunk)?;
            if accepted > 0 {
                fed_any = true;
            }
        }

        if fed_any {
            thread::sleep(Duration::from_millis(1));
        } else {
            thread::sleep(Duration::from_millis(2));
        }

        if Instant::now() >= next_debug_report {
            eprintln!("--- csv_streamer debug ---");
            for stream in &streams {
                eprintln!(
                    "  id={} addr=0x{:02X} fill={}/{} free={} feed_calls={} probe_calls={} accepted_total={} cursor={}",
                    stream.stream_id,
                    stream.addr,
                    fill_level(stream),
                    stream.capacity,
                    stream.free,
                    stream.feed_calls,
                    stream.probe_calls,
                    stream.accepted_total,
                    stream.cursor
                );
            }

            while Instant::now() >= next_debug_report {
                next_debug_report += DEBUG_REPORT_INTERVAL;
            }
        }
    }
}

fn fill_level(stream: &StreamState) -> usize {
    stream.capacity.saturating_sub(stream.free)
}

fn load_host_sources(model_path: &Path) -> Result<Vec<HostSource>> {
    let model_text = fs::read_to_string(model_path)
        .with_context(|| format!("reading model {}", model_path.display()))?;
    let model = gen_model::parse_model(&model_text)
        .with_context(|| format!("parsing model {}", model_path.display()))?;

    let base = model_path.parent().unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();

    for reg in &model.registers {
        let Some(csv) = &reg.csv else {
            continue;
        };

        if !matches!(csv.mode, CsvModeDef::HostStream) {
            continue;
        }

        let path = base.join(&csv.path);
        let samples = gen_model::load_csv_samples(&path)
            .with_context(|| format!("loading host_stream CSV for register 0x{:02X}", reg.addr))?;

        out.push(HostSource {
            addr: reg.addr as u8,
            samples,
        });
    }

    out.sort_by_key(|source| source.addr);
    Ok(out)
}

// HELLO exchange:
// - Host sends HELLO_REQ with empty payload.
// - Device responds with HELLO_RESP payload: [proto_version: u8, stream_count: u8, stream descriptors...]
// - Each descriptor is 4 bytes: [stream_id, register_addr, capacity_le_u16].
fn negotiate_stream_descriptors(port: &mut dyn SerialPort) -> Result<Vec<DeviceStreamDescriptor>> {
    send_frame(port, HostOp::HelloReq, &[])?;
    let payload = read_until_frame(port, DeviceOp::HelloResp, Duration::from_secs(2))?;

    ensure!(payload.len() >= 2, "HELLO_RESP payload too short");
    ensure!(
        payload[0] == STREAM_PROTO_VERSION,
        "protocol version mismatch: device={} host={}",
        payload[0],
        STREAM_PROTO_VERSION
    );

    let count = payload[1] as usize;
    let expected_len = 2 + count * 4;
    ensure!(
        payload.len() == expected_len,
        "HELLO_RESP length mismatch: got {}, expected {}",
        payload.len(),
        expected_len
    );

    let mut out = Vec::with_capacity(count);
    let mut offset = 2usize;

    for _ in 0..count {
        let stream_id = payload[offset];
        let addr = payload[offset + 1];
        let capacity = u16::from_le_bytes([payload[offset + 2], payload[offset + 3]]) as usize;
        ensure!(capacity > 0, "stream {} has zero capacity", stream_id);
        out.push(DeviceStreamDescriptor {
            stream_id,
            addr,
            capacity,
        });
        offset += 4;
    }

    Ok(out)
}

fn feed_chunk(
    port: &mut dyn SerialPort,
    stream: &mut StreamState,
    chunk_len: usize,
) -> Result<usize> {
    ensure!(chunk_len <= stream.free, "feed chunk exceeds free space");

    // FEED payload format: [stream_id, sample0, sample1, ...].
    // stream_id maps to a host_stream register from HELLO_RESP.
    let mut payload = Vec::with_capacity(1 + chunk_len);
    payload.push(stream.stream_id);

    for _ in 0..chunk_len {
        payload.push(stream.samples[stream.cursor]);
        stream.cursor = (stream.cursor + 1) % stream.samples.len();
    }

    send_frame(port, HostOp::Feed, &payload)?;
    let ack = read_until_frame(port, DeviceOp::FeedAck, Duration::from_secs(2))?;

    // FEED_ACK payload format: [stream_id, accepted_le_u16, free_le_u16].
    // accepted may be less than requested when the firmware buffer is near full.
    ensure!(ack.len() == 5, "FEED_ACK payload must be 5 bytes");
    ensure!(
        ack[0] == stream.stream_id,
        "FEED_ACK stream id mismatch: got {}, expected {}",
        ack[0],
        stream.stream_id
    );

    let accepted = u16::from_le_bytes([ack[1], ack[2]]) as usize;
    let free = u16::from_le_bytes([ack[3], ack[4]]) as usize;

    ensure!(
        accepted <= chunk_len,
        "device accepted {} bytes, but {} were sent",
        accepted,
        chunk_len
    );

    let rejected = chunk_len.saturating_sub(accepted);
    rewind_cursor(stream, rejected);

    stream.free = free;

    if chunk_len == 0 {
        stream.probe_calls = stream.probe_calls.saturating_add(1);
        return Ok(accepted);
    }

    stream.feed_calls = stream.feed_calls.saturating_add(1);
    stream.accepted_total = stream.accepted_total.saturating_add(accepted as u64);

    if accepted == 0 {
        eprintln!(
            "stream {} FEED_ACK accepted=0 (requested={}, free={})",
            stream.stream_id, chunk_len, free
        );
    }

    Ok(accepted)
}

fn rewind_cursor(stream: &mut StreamState, count: usize) {
    if count == 0 {
        return;
    }

    let len = stream.samples.len();
    let rewind = count % len;
    if rewind == 0 {
        return;
    }

    stream.cursor = (stream.cursor + len - rewind) % len;
}

// Serialize a protocol frame as: [opcode, payload_len_le_u16, payload...].
fn send_frame(port: &mut dyn SerialPort, opcode: HostOp, payload: &[u8]) -> Result<()> {
    ensure!(payload.len() <= u16::MAX as usize, "payload too large");

    let len = payload.len() as u16;
    let header = [opcode as u8, (len & 0xFF) as u8, (len >> 8) as u8];

    port.write_all(&header).context("writing frame header")?;
    port.write_all(payload).context("writing frame payload")?;
    port.flush().context("flushing serial writes")?;

    Ok(())
}

// Read frames until the expected response opcode arrives.
// Protocol errors are asynchronous and can be returned while waiting for any response.
fn read_until_frame(
    port: &mut dyn SerialPort,
    expected_opcode: DeviceOp,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;

    loop {
        let (opcode_raw, payload) = read_frame(port, deadline)?;
        let Ok(opcode) = DeviceOp::try_from(opcode_raw) else {
            continue;
        };

        if opcode == expected_opcode {
            return Ok(payload);
        }

        if opcode == DeviceOp::Error {
            let code = payload.first().copied().unwrap_or(0xFF);
            bail!("device returned error code {}", code);
        }
    }
}

// Parse exactly one framed message from serial using the shared 3-byte header format.
fn read_frame(port: &mut dyn SerialPort, deadline: Instant) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 3];
    read_exact_until(port, &mut header, deadline)?;

    let payload_len = u16::from_le_bytes([header[1], header[2]]) as usize;
    let mut payload = vec![0u8; payload_len];
    read_exact_until(port, &mut payload, deadline)?;

    Ok((header[0], payload))
}

fn read_exact_until(
    port: &mut dyn SerialPort,
    mut out: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    while !out.is_empty() {
        match port.read(out) {
            Ok(0) => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for serial data");
                }
            }
            Ok(n) => {
                let (_, rest) = out.split_at_mut(n);
                out = rest;
            }
            Err(err)
                if err.kind() == ErrorKind::TimedOut || err.kind() == ErrorKind::WouldBlock =>
            {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for serial data");
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("reading serial frame"),
        }
    }

    Ok(())
}
