use embassy_futures::select::{Either, select};
use embassy_rp::{
    i2c::AbortReason,
    i2c_slave,
    peripherals::I2C1,
};
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, signal::Signal};

use crate::{
    I2C_READ_CHUNK, log, regfile,
    streams::ReadEffect,
};

static I2C_RESET_SIGNAL: Signal<ThreadModeRawMutex, ()> = Signal::new();

pub fn request_reset() {
    I2C_RESET_SIGNAL.signal(());
}

#[embassy_executor::task]
pub async fn task(mut slave: i2c_slave::I2cSlave<'static, I2C1>) -> ! {
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
