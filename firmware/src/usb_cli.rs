use core::fmt::Write as _;
use core::str;

use embassy_usb::{
    class::cdc_acm::CdcAcmClass,
    driver::EndpointError,
};
use heapless::String;
use static_cell::StaticCell;

use crate::{
    USB_MAX_PACKET_SIZE, UsbDriver, log, model,
    streams::{self, HostStreamStatus},
};

static LOG_SNAPSHOT: StaticCell<[log::Event; log::RING_CAPACITY]> = StaticCell::new();

#[embassy_executor::task]
pub async fn task(mut class: CdcAcmClass<'static, UsbDriver>) -> ! {
    let snapshot = LOG_SNAPSHOT.init([log::Event::empty(); log::RING_CAPACITY]);
    let mut rx_packet = [0u8; USB_MAX_PACKET_SIZE as usize];
    let mut cmd_buf = [0u8; 64];
    let mut cmd_len: usize;

    loop {
        class.wait_connection().await;
        let _ = usb_write_line(
            &mut class,
            "mimIIC ready. Commands: dump, reset_i2c, clear, stream_status, help",
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
        crate::i2c_service::request_reset();
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
