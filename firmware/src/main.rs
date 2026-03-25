#![no_std]
#![no_main]

mod log;
mod model;
mod regfile;
mod streams;

use core::fmt::Write as _;
use core::str;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_rp::{
    bind_interrupts, i2c,
    i2c::AbortReason,
    i2c_slave,
    peripherals::{I2C1, PIO0, USB},
    pio::{InterruptHandler as PioInterruptHandler, Pio},
    pio_programs::ws2812::{Grb, PioWs2812, PioWs2812Program},
    usb::{Driver, InterruptHandler as UsbInterruptHandler},
};
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, signal::Signal};
use embassy_time::Timer;
use embassy_usb::{
    Builder, Config as UsbConfig,
    class::cdc_acm::{CdcAcmClass, State as CdcAcmState},
    driver::EndpointError,
};
use heapless::String;
use smart_leds::RGB8;
use static_cell::StaticCell;

use crate::streams::{FeedError, HostStreamDescriptor, HostStreamStatus, ReadEffect};

use {defmt_rtt as _, panic_probe as _};

// Bogus VID and PID.
// TODO: Look into open source options like: https://pid.codes/
const USB_VENDOR_ID: u16 = 0x1111;
const USB_PRODUCT_ID: u16 = 0x2222;
const USB_MAX_PACKET_SIZE: u16 = 64; // bytes
const I2C_READ_CHUNK: usize = 1; // bytes; workaround for repeated LeftoverBytes path under single-byte master reads
const HEARTBEAT_ON_MS: u64 = 200;
const HEARTBEAT_OFF_MS: u64 = 800;

const STREAM_PROTO_VERSION: u8 = 1;
const STREAM_FRAME_HEADER_BYTES: usize = 3;
const STREAM_RX_ACCUM_BYTES: usize = 512;

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum HostOp {
    HelloReq = 0x01,
    Feed = 0x02,
    ResetStreams = 0x03,
}

impl TryFrom<u8> for HostOp {
    type Error = u8;

    fn try_from(value: u8) -> core::result::Result<Self, u8> {
        match value {
            x if x == Self::HelloReq as u8 => Ok(Self::HelloReq),
            x if x == Self::Feed as u8 => Ok(Self::Feed),
            x if x == Self::ResetStreams as u8 => Ok(Self::ResetStreams),
            _ => Err(value),
        }
    }
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum DeviceOp {
    HelloResp = 0x81,
    FeedAck = 0x82,
    ResetAck = 0x83,
    Error = 0xFF,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum StreamError {
    BadFrame = 1,
    BadPayload = 2,
    InvalidStreamId = 3,
    UnknownOpcode = 4,
}

static I2C_RESET_SIGNAL: Signal<ThreadModeRawMutex, ()> = Signal::new();

static USB_CONFIG_DESCRIPTOR: StaticCell<[u8; 512]> = StaticCell::new();
static USB_BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static USB_MSOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static USB_CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
static USB_CLI_CDC_STATE: StaticCell<CdcAcmState<'static>> = StaticCell::new();
static USB_STREAM_CDC_STATE: StaticCell<CdcAcmState<'static>> = StaticCell::new();
static LOG_SNAPSHOT: StaticCell<[log::Event; log::RING_CAPACITY]> = StaticCell::new();

bind_interrupts!(struct Irqs {
    I2C1_IRQ => i2c::InterruptHandler<I2C1>;
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

type UsbDriver = Driver<'static, USB>;
type HeartbeatLed = PioWs2812<'static, PIO0, 0, 1, Grb>;

#[embassy_executor::task]
async fn usb_device_task(mut device: embassy_usb::UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}

#[embassy_executor::task]
async fn usb_cli_task(mut class: CdcAcmClass<'static, UsbDriver>) -> ! {
    let snapshot = LOG_SNAPSHOT.init([log::Event::empty(); log::RING_CAPACITY]);
    let mut rx_packet = [0u8; USB_MAX_PACKET_SIZE as usize];
    let mut cmd_buf = [0u8; 64];
    let mut cmd_len: usize;

    loop {
        class.wait_connection().await;
        let _ = usb_write_line(
            &mut class,
            "morphIIC ready. Commands: dump, reset_i2c, clear, stream_status, help",
        )
        .await;

        cmd_len = 0;

        loop {
            let packet_len = match class.read_packet(&mut rx_packet).await {
                Ok(n) => n,
                Err(_) => break,
            };

            for byte in &rx_packet[..packet_len] {
                if *byte == b'\n' || *byte == b'\r' {
                    if cmd_len == 0 {
                        continue;
                    }

                    handle_cli_command(&cmd_buf[..cmd_len], snapshot, &mut class).await;
                    cmd_len = 0;
                    continue;
                }

                if cmd_len < cmd_buf.len() {
                    cmd_buf[cmd_len] = *byte;
                    cmd_len += 1;
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn usb_stream_task(mut class: CdcAcmClass<'static, UsbDriver>) -> ! {
    let mut rx_packet = [0u8; USB_MAX_PACKET_SIZE as usize];
    let mut accum = [0u8; STREAM_RX_ACCUM_BYTES];
    let mut accum_len: usize;

    loop {
        class.wait_connection().await;
        accum_len = 0;

        'connection: loop {
            let packet_len = match class.read_packet(&mut rx_packet).await {
                Ok(n) => n,
                Err(_) => break 'connection,
            };

            if packet_len == 0 {
                continue;
            }

            if accum_len + packet_len > accum.len() {
                accum_len = 0;
                if stream_send_error(&mut class, StreamError::BadFrame)
                    .await
                    .is_err()
                {
                    break 'connection;
                }
                continue;
            }

            accum[accum_len..accum_len + packet_len].copy_from_slice(&rx_packet[..packet_len]);
            accum_len += packet_len;

            loop {
                if accum_len < STREAM_FRAME_HEADER_BYTES {
                    break;
                }

                let payload_len = u16::from_le_bytes([accum[1], accum[2]]) as usize;
                let frame_len = STREAM_FRAME_HEADER_BYTES + payload_len;

                if frame_len > accum.len() {
                    accum_len = 0;
                    if stream_send_error(&mut class, StreamError::BadFrame)
                        .await
                        .is_err()
                    {
                        break 'connection;
                    }
                    break;
                }

                if accum_len < frame_len {
                    break;
                }

                if process_stream_frame(
                    accum[0],
                    &accum[STREAM_FRAME_HEADER_BYTES..frame_len],
                    &mut class,
                )
                .await
                .is_err()
                {
                    break 'connection;
                }

                accum.copy_within(frame_len..accum_len, 0);
                accum_len -= frame_len;
            }
        }
    }
}

async fn process_stream_frame(
    opcode: u8,
    payload: &[u8],
    class: &mut CdcAcmClass<'static, UsbDriver>,
) -> Result<(), EndpointError> {
    let Ok(opcode) = HostOp::try_from(opcode) else {
        return stream_send_error(class, StreamError::UnknownOpcode).await;
    };

    match opcode {
        HostOp::HelloReq => {
            let mut descriptors = [HostStreamDescriptor {
                stream_id: 0,
                addr: 0,
                capacity: 0,
            }; streams::MAX_STATUS_STREAMS];
            let count = streams::descriptors(&mut descriptors);

            let mut out = [0u8; 2 + streams::MAX_STATUS_STREAMS * 4];
            out[0] = STREAM_PROTO_VERSION;
            out[1] = count as u8;

            let mut w = 2;
            for descriptor in &descriptors[..count] {
                out[w] = descriptor.stream_id;
                out[w + 1] = descriptor.addr;
                out[w + 2] = (descriptor.capacity & 0xFF) as u8;
                out[w + 3] = (descriptor.capacity >> 8) as u8;
                w += 4;
            }

            stream_send_frame(class, DeviceOp::HelloResp, &out[..w]).await
        }
        HostOp::Feed => {
            if payload.is_empty() {
                return stream_send_error(class, StreamError::BadPayload).await;
            }

            let stream_id = payload[0];
            let data = &payload[1..];

            match streams::feed(stream_id, data) {
                Ok(result) => {
                    let accepted = result.accepted.min(u16::MAX as usize) as u16;
                    let free = result.free.min(u16::MAX as usize) as u16;
                    let ack = [
                        stream_id,
                        (accepted & 0xFF) as u8,
                        (accepted >> 8) as u8,
                        (free & 0xFF) as u8,
                        (free >> 8) as u8,
                    ];
                    stream_send_frame(class, DeviceOp::FeedAck, &ack).await
                }
                Err(FeedError::InvalidStreamId) => {
                    stream_send_error(class, StreamError::InvalidStreamId).await
                }
            }
        }
        HostOp::ResetStreams => {
            if !payload.is_empty() {
                return stream_send_error(class, StreamError::BadPayload).await;
            }
            streams::reset_all();
            stream_send_frame(class, DeviceOp::ResetAck, &[]).await
        }
    }
}

async fn stream_send_error(
    class: &mut CdcAcmClass<'static, UsbDriver>,
    code: StreamError,
) -> Result<(), EndpointError> {
    stream_send_frame(class, DeviceOp::Error, &[code as u8]).await
}

async fn stream_send_frame(
    class: &mut CdcAcmClass<'static, UsbDriver>,
    opcode: DeviceOp,
    payload: &[u8],
) -> Result<(), EndpointError> {
    let len = payload.len().min(u16::MAX as usize) as u16;
    let header = [opcode as u8, (len & 0xFF) as u8, (len >> 8) as u8];
    usb_write_all(class, &header).await?;
    usb_write_all(class, payload).await
}

// TODO: Try implementing a force NACK command. This would involve disabling the i2c peripheral, setting `IC_SLV_DATA_NACK_ONLY`, then re-enabling.
// There is no way to force a NACK from the RP2040 without following the procedure above due to peripheral limitations. See pg. 496 of rp2040 ds: https://pip-assets.raspberrypi.com/categories/814-rp2040/documents/RP-008371-DS-1-rp2040-datasheet.pdf
async fn handle_cli_command(
    cmd: &[u8],
    snapshot: &mut [log::Event; log::RING_CAPACITY],
    class: &mut CdcAcmClass<'static, UsbDriver>,
) {
    let line = match str::from_utf8(cmd) {
        Ok(s) => s.trim(),
        Err(_) => {
            let _ = usb_write_line(class, "ERR invalid UTF-8 command").await;
            return;
        }
    };

    if line.eq_ignore_ascii_case("help") {
        let _ = usb_write_line(class, "Commands:").await;
        let _ = usb_write_line(class, "  dump          -> dump I2C transaction log ring").await;
        let _ = usb_write_line(class, "  reset_i2c     -> reset RP2040 I2C peripheral").await;
        let _ = usb_write_line(class, "  clear         -> clear transaction ring").await;
        let _ = usb_write_line(class, "  stream_status -> show host stream buffer status").await;
        return;
    }

    if line.eq_ignore_ascii_case("dump") {
        let count = log::snapshot(snapshot);
        let mut line_buf: String<192> = String::new();

        if count == 0 {
            let _ = usb_write_line(class, "log is empty").await;
            return;
        }

        let _ = usb_write_line(class, "--- log dump begin ---").await;
        for event in &snapshot[..count] {
            log::format_event_line(event, &mut line_buf);
            let _ = usb_write_line(class, line_buf.as_str()).await;
        }
        let _ = usb_write_line(class, "--- log dump end ---").await;
        return;
    }

    if line.eq_ignore_ascii_case("clear") {
        log::clear();
        let _ = usb_write_line(class, "log cleared").await;
        return;
    }

    if line.eq_ignore_ascii_case("reset_i2c") {
        I2C_RESET_SIGNAL.signal(());
        let _ = usb_write_line(class, "i2c reset requested").await;
        return;
    }

    if line.eq_ignore_ascii_case("stream_status") {
        let mut statuses = [HostStreamStatus {
            stream_id: 0,
            addr: 0,
            level: 0,
            free: 0,
            underruns: 0,
            drops: 0,
            has_last: false,
            last_value: 0,
        }; streams::MAX_STATUS_STREAMS];
        let count = streams::statuses(&mut statuses);

        if count == 0 {
            let _ = usb_write_line(class, "no host_stream registers configured").await;
            return;
        }

        let mut line_buf: String<192> = String::new();
        let _ = usb_write_line(class, "--- stream status ---").await;
        for status in &statuses[..count] {
            line_buf.clear();
            let _ = write!(
                line_buf,
                "id={} addr=0x{:02X} fill={}/{} free={} underruns={} drops={} ",
                status.stream_id,
                status.addr,
                status.level,
                model::HOST_STREAM_BUFFER_CAPACITY,
                status.free,
                status.underruns,
                status.drops,
            );

            if status.has_last {
                let _ = write!(line_buf, "last=0x{:02X}", status.last_value);
            } else {
                let _ = line_buf.push_str("last=--");
            }

            let _ = usb_write_line(class, line_buf.as_str()).await;
        }

        return;
    }

    let _ = usb_write_line(class, "ERR unknown command. Try: help").await;
}

async fn usb_write_line(
    class: &mut CdcAcmClass<'static, UsbDriver>,
    line: &str,
) -> Result<(), EndpointError> {
    usb_write_all(class, line.as_bytes()).await?;
    usb_write_all(class, b"\r\n").await
}

async fn usb_write_all(
    class: &mut CdcAcmClass<'static, UsbDriver>,
    mut bytes: &[u8],
) -> Result<(), EndpointError> {
    let max_packet = class.max_packet_size() as usize;

    while !bytes.is_empty() {
        let chunk_len = bytes.len().min(max_packet);
        class.write_packet(&bytes[..chunk_len]).await?;
        bytes = &bytes[chunk_len..];
    }

    Ok(())
}

#[embassy_executor::task]
async fn i2c_slave_task(mut slave: i2c_slave::I2cSlave<'static, I2C1>) -> ! {
    let mut regfile = regfile::RegisterFile::new();
    let mut listen_buf = [0u8; 64];

    loop {
        match select(I2C_RESET_SIGNAL.wait(), slave.listen(&mut listen_buf)).await {
            Either::First(()) => {
                slave.reset();
                log::record(log::EventKind::Reset, regfile.pointer(), 0, 0, 0, &[]);
            }
            Either::Second(Ok(i2c_slave::Command::Write(len))) => {
                if len == 0 {
                    log::record(log::EventKind::Write, regfile.pointer(), 0, 0, 0, &[]);
                    continue;
                }

                let pointer = listen_buf[0];
                regfile.set_pointer(pointer);

                let payload = if len > 1 { &listen_buf[1..len] } else { &[] };

                let accepted = regfile.write_payload(payload);
                log::record(
                    log::EventKind::Write,
                    pointer,
                    payload.len(),
                    accepted as u8,
                    0,
                    payload,
                );
            }
            Either::Second(Ok(i2c_slave::Command::WriteRead(len))) => {
                if len > 0 {
                    let pointer = listen_buf[0];
                    regfile.set_pointer(pointer);

                    if len > 1 {
                        let payload = &listen_buf[1..len];
                        let _ = regfile.write_payload(payload);
                    }
                }

                serve_read(&mut slave, &mut regfile, log::EventKind::WriteRead).await;
            }
            Either::Second(Ok(i2c_slave::Command::Read)) => {
                serve_read(&mut slave, &mut regfile, log::EventKind::Read).await;
            }
            Either::Second(Ok(i2c_slave::Command::GeneralCall(len))) => {
                log::record(
                    log::EventKind::GeneralCall,
                    regfile.pointer(),
                    len,
                    0,
                    0,
                    &listen_buf[..len.min(listen_buf.len())],
                );
            }
            Either::Second(Err(err)) => {
                let code = encode_i2c_error(&err);
                log::record(
                    log::EventKind::ListenError,
                    regfile.pointer(),
                    0,
                    0,
                    code,
                    &[],
                );
            }
        }
    }
}

async fn serve_read(
    slave: &mut i2c_slave::I2cSlave<'static, I2C1>,
    regfile: &mut regfile::RegisterFile,
    kind: log::EventKind,
) {
    let pointer = regfile.pointer();
    let mut tx = [0u8; I2C_READ_CHUNK];
    let mut effects = [ReadEffect::None; I2C_READ_CHUNK];
    let mut total = 0;
    let mut preview = [0u8; 8];
    let mut preview_len = 0;

    loop {
        regfile.read_into(&mut tx, &mut effects);

        if preview_len < preview.len() {
            let to_copy = (preview.len() - preview_len).min(tx.len());
            preview[preview_len..preview_len + to_copy].copy_from_slice(&tx[..to_copy]);
            preview_len += to_copy;
        }

        total += tx.len();

        match slave.respond_to_read(&tx).await {
            Ok(i2c_slave::ReadStatus::NeedMoreBytes) => continue,
            Ok(i2c_slave::ReadStatus::Done) => {
                log::record(kind, pointer, total, 0, 0, &preview[..preview_len]);
                return;
            }
            // Currently, the number of leftover bytes is stored in the error value of the log and the unread data in the chunk is shown in payload.
            // TODO: Consider how this branch should be handled
            // More: Yep this caused major issues when only reading one byte from the master in quick succession. Going to need to find a better way to queue the appropriate number of bytes.
            Ok(i2c_slave::ReadStatus::LeftoverBytes(leftover)) => {
                let unread = leftover as usize;
                regfile.rollback_unread(&effects, unread);
                let real_len = total.saturating_sub(unread);
                if real_len < preview_len {
                    preview_len = real_len;
                }
                log::record(
                    kind,
                    pointer,
                    real_len,
                    1,
                    leftover as u32,
                    &preview[..preview_len],
                );
                return;
            }
            Err(err) => {
                log::record(
                    log::EventKind::ReadError,
                    pointer,
                    total,
                    0,
                    encode_i2c_error(&err),
                    &preview[..preview_len],
                );
                return;
            }
        }
    }
}

#[embassy_executor::task]
async fn heartbeat_task(mut led: HeartbeatLed) -> ! {
    let on = [RGB8::new(0x00, 0x10, 0x00)];
    let off = [RGB8::new(0x00, 0x00, 0x00)];

    loop {
        led.write(&on).await;
        Timer::after_millis(HEARTBEAT_ON_MS).await;
        led.write(&off).await;
        Timer::after_millis(HEARTBEAT_OFF_MS).await;
    }
}

fn encode_i2c_error(err: &i2c_slave::Error) -> u32 {
    match err {
        i2c_slave::Error::Abort(reason) => 0x1000 | encode_abort_reason(reason),
        i2c_slave::Error::InvalidResponseBufferLength => 0x2000,
        i2c_slave::Error::PartialWrite(len) => 0x3000 | (*len as u32),
        i2c_slave::Error::PartialGeneralCall(len) => 0x4000 | (*len as u32),
        _ => 0xFFFF,
    }
}

fn encode_abort_reason(reason: &AbortReason) -> u32 {
    match reason {
        AbortReason::NoAcknowledge => 0x01,
        AbortReason::ArbitrationLoss => 0x02,
        AbortReason::TxNotEmpty(left) => 0x0300 | (*left as u32),
        AbortReason::Other(bits) => 0x0400 | (bits & 0xff),
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Waveshare RP2040-Zero onboard WS2812 is on GP16.
    let mut pio = Pio::new(p.PIO0, Irqs);
    let ws2812_program = PioWs2812Program::new(&mut pio.common);
    let heartbeat_led = PioWs2812::<PIO0, 0, 1, Grb>::with_color_order(
        &mut pio.common,
        pio.sm0,
        p.DMA_CH0,
        p.PIN_16,
        &ws2812_program,
    );

    let mut i2c_cfg = i2c_slave::Config::default();
    i2c_cfg.addr = model::I2C_ADDRESS_7BIT as u16;
    i2c_cfg.general_call = false;
    // TODO: Make pull-ups part of the device config
    i2c_cfg.scl_pullup = true;
    i2c_cfg.sda_pullup = true;

    // RP2040 I2C1 default mapping used
    // SCL -> GP3, SDA -> GP2
    let slave = i2c_slave::I2cSlave::new(p.I2C1, p.PIN_3, p.PIN_2, Irqs, i2c_cfg);

    let usb_driver = Driver::new(p.USB, Irqs);

    let mut usb_config = UsbConfig::new(USB_VENDOR_ID, USB_PRODUCT_ID);
    usb_config.manufacturer = Some("morphIIC");
    usb_config.product = Some(model::DEVICE_NAME);
    usb_config.serial_number = Some("morphiic-p0");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    let cli_cdc_state = USB_CLI_CDC_STATE.init(CdcAcmState::new());
    let stream_cdc_state = USB_STREAM_CDC_STATE.init(CdcAcmState::new());
    let mut builder = Builder::new(
        usb_driver,
        usb_config,
        USB_CONFIG_DESCRIPTOR.init([0; 512]),
        USB_BOS_DESCRIPTOR.init([0; 256]),
        USB_MSOS_DESCRIPTOR.init([0; 256]),
        USB_CONTROL_BUF.init([0; 64]),
    );

    let cli_cdc = CdcAcmClass::new(&mut builder, cli_cdc_state, USB_MAX_PACKET_SIZE);
    let stream_cdc = CdcAcmClass::new(&mut builder, stream_cdc_state, USB_MAX_PACKET_SIZE);
    let usb = builder.build();

    streams::init();

    spawner.spawn(i2c_slave_task(slave)).unwrap();
    spawner.spawn(usb_device_task(usb)).unwrap();
    spawner.spawn(usb_cli_task(cli_cdc)).unwrap();
    spawner.spawn(usb_stream_task(stream_cdc)).unwrap();
    spawner.spawn(heartbeat_task(heartbeat_led)).unwrap();

    // Keep main alive so remaining PIO handles don't get dropped while SM0 drives WS2812.
    loop {
        Timer::after_secs(60).await;
    }
}
