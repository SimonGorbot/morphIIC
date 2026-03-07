#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::peripherals::{I2C1, PIO0, USB};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{Grb, PioWs2812, PioWs2812Program};
use embassy_rp::usb::{Driver, InterruptHandler as UsbInterruptHandler};
use embassy_rp::{bind_interrupts, i2c, i2c_slave};
use embassy_time::Timer;
use smart_leds::RGB8;
use {defmt_rtt as _, panic_probe as _};

const SLAVE_ADDR: u8 = 0x5F;
const WHOAMI_REG: u8 = 0x0F;
const WHOAMI_VALUE: u8 = 0x42;

bind_interrupts!(struct UsbIrqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

bind_interrupts!(struct I2cIrqs {
    I2C1_IRQ => i2c::InterruptHandler<I2C1>;
});

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

fn read_register(reg: u8) -> u8 {
    match reg {
        WHOAMI_REG => WHOAMI_VALUE,
        _ => 0x00,
    }
}

#[embassy_executor::task]
async fn i2c_slave_task(mut dev: i2c_slave::I2cSlave<'static, I2C1>) -> ! {
    log::info!("i2c slave ready at address 0x{:02X}", SLAVE_ADDR);

    let mut current_reg = WHOAMI_REG;
    let mut buf = [0u8; 16];

    loop {
        match dev.listen(&mut buf).await {
            Ok(i2c_slave::Command::WriteRead(len)) => {
                if len > 0 {
                    current_reg = buf[0];
                }

                let value = read_register(current_reg);
                match dev.respond_and_fill(&[value], 0x00).await {
                    Ok(status) => log::info!(
                        "write_read reg=0x{:02X} -> 0x{:02X} ({:?})",
                        current_reg,
                        value,
                        status
                    ),
                    Err(e) => log::error!("write_read respond error: {:?}", e),
                }
            }
            Ok(i2c_slave::Command::Write(len)) => {
                if len > 0 {
                    current_reg = buf[0];
                    log::info!(
                        "write len={} pointer=0x{:02X} data={:?}",
                        len,
                        current_reg,
                        &buf[..len]
                    );
                } else {
                    log::info!("write len=0");
                }
            }
            Ok(i2c_slave::Command::Read) => {
                let value = read_register(current_reg);
                match dev.respond_and_fill(&[value], 0x00).await {
                    Ok(status) => log::info!(
                        "read reg=0x{:02X} -> 0x{:02X} ({:?})",
                        current_reg,
                        value,
                        status
                    ),
                    Err(e) => log::error!("read respond error: {:?}", e),
                }
            }
            Ok(i2c_slave::Command::GeneralCall(len)) => {
                log::info!("general call len={} data={:?}", len, &buf[..len]);
            }
            Err(e) => log::error!("listen error: {:?}", e),
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let driver = Driver::new(p.USB, UsbIrqs);
    let _ = spawner.spawn(logger_task(driver));
    log::info!("boot");

    let mut cfg = i2c_slave::Config::default();
    cfg.addr = SLAVE_ADDR as u16;

    // RP2040-Zero: SDA on GP2, SCL on GP3.
    let slave = i2c_slave::I2cSlave::new(p.I2C1, p.PIN_3, p.PIN_2, I2cIrqs, cfg);
    let _ = spawner.spawn(i2c_slave_task(slave));

    let mut pio = Pio::new(p.PIO0, UsbIrqs);
    let program = PioWs2812Program::new(&mut pio.common);
    let mut led = PioWs2812::<PIO0, 0, 1, Grb>::with_color_order(
        &mut pio.common,
        pio.sm0,
        p.DMA_CH0,
        p.PIN_16,
        &program,
    );

    let on = [RGB8::new(0x10, 0x00, 0x00)];
    let off = [RGB8::new(0x00, 0x00, 0x00)];

    loop {
        led.write(&on).await;
        Timer::after_millis(250).await;
        led.write(&off).await;
        Timer::after_millis(250).await;
    }
}
