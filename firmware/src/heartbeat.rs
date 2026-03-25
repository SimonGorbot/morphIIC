use embassy_time::Timer;
use smart_leds::RGB8;

use crate::{HEARTBEAT_OFF_MS, HEARTBEAT_ON_MS, HeartbeatLed};

#[embassy_executor::task]
pub async fn task(mut led: HeartbeatLed) -> ! {
    let on = [RGB8::new(0x00, 0x10, 0x00)];
    let off = [RGB8::new(0x00, 0x00, 0x00)];

    loop {
        led.write(&on).await;
        Timer::after_millis(HEARTBEAT_ON_MS).await;
        led.write(&off).await;
        Timer::after_millis(HEARTBEAT_OFF_MS).await;
    }
}
