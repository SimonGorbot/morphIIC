use embassy_usb::{
    class::cdc_acm::CdcAcmClass,
    driver::EndpointError,
};

use crate::{
    STREAM_FRAME_HEADER_BYTES, STREAM_PROTO_VERSION, STREAM_RX_ACCUM_BYTES, USB_MAX_PACKET_SIZE,
    UsbDriver,
    streams::{self, FeedError, HostStreamDescriptor},
};

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

#[embassy_executor::task]
pub async fn task(mut class: CdcAcmClass<'static, UsbDriver>) -> ! {
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
