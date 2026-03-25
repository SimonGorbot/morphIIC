#![no_std]
#![no_main]

mod heartbeat;
mod i2c_service;
mod log;
mod model;
mod regfile;
mod streams;
mod usb_cli;
mod usb_stream;

use embassy_executor::Spawner;
use embassy_rp::{
    bind_interrupts, i2c,
    i2c_slave,
    peripherals::{I2C1, PIO0, USB},
    pio::{InterruptHandler as PioInterruptHandler, Pio},
    pio_programs::ws2812::{Grb, PioWs2812, PioWs2812Program},
    usb::{Driver, InterruptHandler as UsbInterruptHandler},
};
use embassy_time::Timer;
use embassy_usb::{
    Builder, Config as UsbConfig,
    class::cdc_acm::{CdcAcmClass, State as CdcAcmState},
};
use static_cell::StaticCell;

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

static USB_CONFIG_DESCRIPTOR: StaticCell<[u8; 512]> = StaticCell::new();
static USB_BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static USB_MSOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static USB_CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
static USB_CLI_CDC_STATE: StaticCell<CdcAcmState<'static>> = StaticCell::new();
static USB_STREAM_CDC_STATE: StaticCell<CdcAcmState<'static>> = StaticCell::new();

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

    spawner.spawn(i2c_service::task(slave)).unwrap();
    spawner.spawn(usb_device_task(usb)).unwrap();
    spawner.spawn(usb_cli::task(cli_cdc)).unwrap();
    spawner.spawn(usb_stream::task(stream_cdc)).unwrap();
    spawner.spawn(heartbeat::task(heartbeat_led)).unwrap();

    // Keep main alive so remaining PIO handles don't get dropped while SM0 drives WS2812.
    loop {
        Timer::after_secs(60).await;
    }
}
